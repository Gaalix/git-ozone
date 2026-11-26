// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `oz agent setup` harness auto-detection, idempotent `SKILL.md` / rule generation,
//! and non-destructive `PostToolUse` / edit checkpoint hook merging across Claude Code,
//! Cursor, Codex CLI, Windsurf, and Antigravity.

use crate::AgentHarness;
use oz_core::{OzError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Start marker for idempotent Ozone instructions inside `AGENTS.md`.
pub const AGENTS_MD_START_MARKER: &str = "<!-- oz-agent-start -->";
/// End marker for idempotent Ozone instructions inside `AGENTS.md`.
pub const AGENTS_MD_END_MARKER: &str = "<!-- oz-agent-end -->";
/// Canonical hook command installed by `oz agent setup`.
pub const OZ_CHECKPOINT_HOOK_CMD: &str = "oz checkpoint --quiet";

/// Options controlling [`setup_agent_harnesses`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSetupOptions {
    /// Explicit `--harness` targets. When empty and `all` is `false`, harnesses are auto-detected.
    pub harnesses: Vec<AgentHarness>,
    /// Configure all 5 supported harnesses regardless of auto-detection.
    pub all: bool,
    /// Check / dry-run mode (`--check` / `--dry-run`): compute file actions without writing to disk.
    pub check_only: bool,
    /// Install non-destructive `oz checkpoint --quiet` hook configurations (default: `true`).
    pub install_hooks: bool,
    /// Optional explicit home directory used during harness auto-detection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home_dir: Option<PathBuf>,
    /// Optional environment variable map override for deterministic testing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_overrides: Option<BTreeMap<String, String>>,
    /// Allow inspecting process environment and `$HOME` when the repo itself has no harness markers.
    pub detect_from_system: bool,
}

impl Default for AgentSetupOptions {
    fn default() -> Self {
        Self {
            harnesses: Vec::new(),
            all: false,
            check_only: false,
            install_hooks: true,
            home_dir: None,
            env_overrides: None,
            detect_from_system: true,
        }
    }
}

impl AgentSetupOptions {
    /// Creates options targeting an explicit list of harnesses.
    #[must_use]
    pub fn for_harnesses<I>(harnesses: I) -> Self
    where
        I: IntoIterator<Item = AgentHarness>,
    {
        Self {
            harnesses: harnesses.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Creates options configuring all 5 supported harnesses.
    #[must_use]
    pub fn all_harnesses() -> Self {
        Self {
            all: true,
            ..Self::default()
        }
    }

    /// Enables `--check` / `--dry-run` mode (zero disk writes).
    #[must_use]
    pub fn with_check_only(mut self, check_only: bool) -> Self {
        self.check_only = check_only;
        self
    }
}

/// Structured summary returned by [`setup_agent_harnesses`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSetupReport {
    pub repo_root: PathBuf,
    pub harnesses: Vec<AgentHarness>,
    pub check_only: bool,
    pub created: Vec<PathBuf>,
    pub updated: Vec<PathBuf>,
    pub unchanged: Vec<PathBuf>,
}

impl AgentSetupReport {
    /// Returns `true` when all target files are already present and up to date.
    #[must_use]
    pub fn is_up_to_date(&self) -> bool {
        self.created.is_empty() && self.updated.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileDisposition {
    Created,
    Updated,
    Unchanged,
}

/// Auto-detects installed coding agent harnesses in `repo_root` (falling back to environment
/// markers and `$HOME` config directories when no repo-local markers exist).
#[must_use]
pub fn detect_harnesses(repo_root: &Path) -> Vec<AgentHarness> {
    let repo_found = detect_repo_harnesses(repo_root);
    if !repo_found.is_empty() {
        return repo_found;
    }

    let env_map: BTreeMap<String, String> = std::env::vars().collect();
    let home = default_home_dir();
    detect_harnesses_with_context(repo_root, home.as_deref(), &env_map)
}

/// Detects harnesses purely from repository-local marker files and directories.
#[must_use]
pub fn detect_repo_harnesses(repo_root: &Path) -> Vec<AgentHarness> {
    let mut set = BTreeSet::new();

    if repo_root.join(".claude").exists() || repo_root.join("CLAUDE.md").is_file() {
        set.insert(AgentHarness::ClaudeCode);
    }
    if repo_root.join(".cursor").exists() || repo_root.join(".cursorrules").is_file() {
        set.insert(AgentHarness::Cursor);
    }
    if repo_root.join(".codex").exists()
        || repo_root.join(".agents").exists()
        || repo_root.join("AGENTS.md").is_file()
    {
        set.insert(AgentHarness::CodexCli);
    }
    if repo_root.join(".windsurf").exists() || repo_root.join(".windsurfrules").is_file() {
        set.insert(AgentHarness::Windsurf);
    }
    if repo_root.join(".agent").exists() || repo_root.join(".gemini").exists() {
        set.insert(AgentHarness::Antigravity);
    }

    ordered_harnesses(&set)
}

/// Detects harnesses across repository markers, optional `home_dir`, and explicit `env_vars`.
#[must_use]
pub fn detect_harnesses_with_context(
    repo_root: &Path,
    home_dir: Option<&Path>,
    env_vars: &BTreeMap<String, String>,
) -> Vec<AgentHarness> {
    let mut set: BTreeSet<AgentHarness> = detect_repo_harnesses(repo_root).into_iter().collect();

    for h in detect_env_harnesses(env_vars) {
        set.insert(h);
    }

    if let Some(home) = home_dir {
        if home.join(".claude").is_dir() {
            set.insert(AgentHarness::ClaudeCode);
        }
        if home.join(".cursor").is_dir() {
            set.insert(AgentHarness::Cursor);
        }
        if home.join(".codex").is_dir() {
            set.insert(AgentHarness::CodexCli);
        }
        if home.join(".windsurf").is_dir() || home.join(".codeium").join("windsurf").is_dir() {
            set.insert(AgentHarness::Windsurf);
        }
        if home.join(".gemini").join("antigravity").is_dir() || home.join(".agent").is_dir() {
            set.insert(AgentHarness::Antigravity);
        }
    }

    ordered_harnesses(&set)
}

/// Detects harnesses from environment variables (`CLAUDECODE`, `CLAUDE_CODE_*`,
/// `CURSOR_TRACE_ID`, `CODEX_*`, `WINDSURF_*`, `ANTIGRAVITY_*`).
#[must_use]
pub fn detect_env_harnesses(env_vars: &BTreeMap<String, String>) -> Vec<AgentHarness> {
    let mut set = BTreeSet::new();

    if has_nonempty_env(env_vars, "CLAUDECODE")
        || has_prefix_env(env_vars, "CLAUDE_CODE_")
        || has_nonempty_env(env_vars, "CLAUDE_CONFIG_DIR")
    {
        set.insert(AgentHarness::ClaudeCode);
    }
    if has_nonempty_env(env_vars, "CURSOR_TRACE_ID")
        || has_nonempty_env(env_vars, "CURSOR_SESSION_ID")
        || has_nonempty_env(env_vars, "CURSOR_AGENT")
    {
        set.insert(AgentHarness::Cursor);
    }
    if has_prefix_env(env_vars, "CODEX_") || has_nonempty_env(env_vars, "OPENAI_CODEX") {
        set.insert(AgentHarness::CodexCli);
    }
    if has_prefix_env(env_vars, "WINDSURF_") || has_nonempty_env(env_vars, "CODEIUM_WINDSURF") {
        set.insert(AgentHarness::Windsurf);
    }
    if has_prefix_env(env_vars, "ANTIGRAVITY_") || has_nonempty_env(env_vars, "AGY_SESSION_ID") {
        set.insert(AgentHarness::Antigravity);
    }

    ordered_harnesses(&set)
}

/// Configures Ozone skills, rules, and `oz checkpoint --quiet` hooks idempotently in `repo_root`.
///
/// When `opts.check_only` is `true`, no files or directories are created or modified on disk,
/// and the returned [`AgentSetupReport`] describes what would be created, updated, or left unchanged.
pub fn setup_agent_harnesses(
    repo_root: &Path,
    opts: &AgentSetupOptions,
) -> Result<AgentSetupReport> {
    let harnesses = resolve_target_harnesses(repo_root, opts);
    let mut dispositions: BTreeMap<PathBuf, FileDisposition> = BTreeMap::new();

    for &harness in &harnesses {
        match harness {
            AgentHarness::ClaudeCode => {
                apply_exact_text_file(
                    repo_root,
                    Path::new(".claude/skills/oz/SKILL.md"),
                    &render_skill_markdown("oz"),
                    opts.check_only,
                    &mut dispositions,
                )?;
                if opts.install_hooks {
                    apply_json_merge_file(
                        repo_root,
                        Path::new(".claude/settings.json"),
                        merge_claude_settings_hooks,
                        opts.check_only,
                        &mut dispositions,
                    )?;
                }
            }
            AgentHarness::Cursor => {
                apply_exact_text_file(
                    repo_root,
                    Path::new(".cursor/rules/oz.mdc"),
                    &render_cursor_mdc(),
                    opts.check_only,
                    &mut dispositions,
                )?;
                if opts.install_hooks {
                    apply_json_merge_file(
                        repo_root,
                        Path::new(".cursor/hooks.json"),
                        merge_cursor_hooks,
                        opts.check_only,
                        &mut dispositions,
                    )?;
                }
            }
            AgentHarness::CodexCli => {
                let skill = render_skill_markdown("oz");
                apply_exact_text_file(
                    repo_root,
                    Path::new(".agents/skills/oz/SKILL.md"),
                    &skill,
                    opts.check_only,
                    &mut dispositions,
                )?;
                apply_exact_text_file(
                    repo_root,
                    Path::new(".codex/skills/oz/SKILL.md"),
                    &skill,
                    opts.check_only,
                    &mut dispositions,
                )?;
                apply_agents_md_section(
                    repo_root,
                    Path::new("AGENTS.md"),
                    opts.check_only,
                    &mut dispositions,
                )?;
                if opts.install_hooks {
                    apply_json_merge_file(
                        repo_root,
                        Path::new(".codex/hooks.json"),
                        merge_codex_hooks,
                        opts.check_only,
                        &mut dispositions,
                    )?;
                }
            }
            AgentHarness::Windsurf => {
                apply_exact_text_file(
                    repo_root,
                    Path::new(".windsurf/rules/oz.md"),
                    &render_windsurf_rule(),
                    opts.check_only,
                    &mut dispositions,
                )?;
                if opts.install_hooks {
                    apply_json_merge_file(
                        repo_root,
                        Path::new(".windsurf/hooks.json"),
                        merge_windsurf_hooks,
                        opts.check_only,
                        &mut dispositions,
                    )?;
                }
            }
            AgentHarness::Antigravity => {
                apply_exact_text_file(
                    repo_root,
                    Path::new(".agent/skills/oz/SKILL.md"),
                    &render_skill_markdown("oz"),
                    opts.check_only,
                    &mut dispositions,
                )?;
                if opts.install_hooks {
                    apply_json_merge_file(
                        repo_root,
                        Path::new(".agent/hooks.json"),
                        merge_antigravity_hooks,
                        opts.check_only,
                        &mut dispositions,
                    )?;
                }
            }
        }
    }

    let mut created = Vec::new();
    let mut updated = Vec::new();
    let mut unchanged = Vec::new();

    for (rel_path, disp) in dispositions {
        match disp {
            FileDisposition::Created => created.push(rel_path),
            FileDisposition::Updated => updated.push(rel_path),
            FileDisposition::Unchanged => unchanged.push(rel_path),
        }
    }

    Ok(AgentSetupReport {
        repo_root: repo_root.to_path_buf(),
        harnesses,
        check_only: opts.check_only,
        created,
        updated,
        unchanged,
    })
}

fn resolve_target_harnesses(repo_root: &Path, opts: &AgentSetupOptions) -> Vec<AgentHarness> {
    if opts.all {
        return AgentHarness::ALL.to_vec();
    }
    if !opts.harnesses.is_empty() {
        let set: BTreeSet<AgentHarness> = opts.harnesses.iter().copied().collect();
        return ordered_harnesses(&set);
    }

    let mut detected: BTreeSet<AgentHarness> =
        detect_repo_harnesses(repo_root).into_iter().collect();

    if let Some(ref overrides) = opts.env_overrides {
        for h in detect_env_harnesses(overrides) {
            detected.insert(h);
        }
    }
    if let Some(ref home) = opts.home_dir {
        for h in detect_harnesses_with_context(repo_root, Some(home), &BTreeMap::new()) {
            detected.insert(h);
        }
    }

    if detected.is_empty() && opts.detect_from_system {
        let sys_env: BTreeMap<String, String> = std::env::vars().collect();
        let home = default_home_dir();
        for h in detect_harnesses_with_context(repo_root, home.as_deref(), &sys_env) {
            detected.insert(h);
        }
    }

    if detected.is_empty() {
        AgentHarness::ALL.to_vec()
    } else {
        ordered_harnesses(&detected)
    }
}

fn ordered_harnesses(set: &BTreeSet<AgentHarness>) -> Vec<AgentHarness> {
    AgentHarness::ALL
        .iter()
        .copied()
        .filter(|h| set.contains(h))
        .collect()
}

fn default_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn has_nonempty_env(env: &BTreeMap<String, String>, key: &str) -> bool {
    env.get(key)
        .is_some_and(|v| !v.trim().is_empty() && v.trim() != "0" && v.trim() != "false")
}

fn has_prefix_env(env: &BTreeMap<String, String>, prefix: &str) -> bool {
    env.iter().any(|(k, v)| {
        k.starts_with(prefix) && !v.trim().is_empty() && v.trim() != "0" && v.trim() != "false"
    })
}

/// Compact, high-signal always-on rule body for `AGENTS.md`, Cursor `.mdc`, and Windsurf rules.
///
/// Designed for progressive disclosure: keeps per-turn context lean with critical Git-vs-Ozone
/// behavioral differences, a concise command map, and pointers to `SKILL.md` / `oz <cmd> --help`.
#[must_use]
pub fn render_ozone_rule_body() -> &'static str {
    "\
# Ozone (`oz`) Guidelines

Prefer `oz` over raw `git` and `gh` (`pr`/`issue`/`run`) in any Git repository. Target any repo directly with `oz -C <dir> <cmd>` (or `--directory <dir>`).

## Key Rules vs. Git
- **Stage-free commits:** `oz commit` includes untracked files automatically (no `git add`); use `--pick f1 h2a` / `--exclude f3` for partial commits and `--amend [--no-edit]` to amend while preserving `Change-Id:`.
- **Clean commit messages:** AI provenance (`[AI:...]`) is stored strictly in local `refs/notes/ai` git notes, NEVER in commit messages (`oz log --no-notes` hides badges; `oz blame <path>` shows line-level human vs. AI attribution).
- **Regex dialect:** `oz grep` uses Rust/ERE regex (`a|b`), not Git BRE (`a\\|b`).
- **OpLog-reversible undo:** Inspect `op_...` IDs with `oz oplog [-n N]` or `oz undo --list`, and rewind mutations in `< 10 ms` with `oz undo [OP_ID | --steps N]` / `oz redo`. Never run destructive `git reset --hard` or `git checkout -- .`.
- **OpLog-safe Git fallback:** For any Git command without a native `oz` equivalent, run `oz git <cmd>` (never raw `git`) so mutations stay OpLog-recorded and reversible.
- **Non-interactive agent mode:** Pass `--agent` or set `OZ_AGENT=1` (or `--json` for the canonical `oz.v1` envelope) to guarantee zero prompts, zero `$EDITOR`, and zero pagers.

## Command Map
- **Inspect:** `oz status [--hunks]`, `oz diff [A..B] [--stat] [--line-numbers] [--exclude <pat>]`, `oz log --oneline -n 10 [REV...] [-- <paths...>]`, `oz show <rev>|<rev>:<path> [--lines 1..50]`, `oz grep <pat> [<rev>] [-- <paths...>]`, `oz blame <path>`, `oz branch [-a|-r]`, `oz rev-parse`.
- **Commit & Undo:** `oz commit -m \"...\" [--pick|--exclude|--amend|--no-edit|-S|--force|--allow-out-of-scope]`, `oz restore <path|f1> [--source <rev>] [--remove-untracked]`, `oz checkpoint --quiet`, `oz oplog [-n N]`, `oz undo [--list | OP_ID | --steps N]`, `oz redo`, `oz stash`, `oz switch`, `oz merge`.
- **Workspaces, Stacks & Sync:** `oz ws spawn|list|rm|gc`, `oz radar`, `oz integrate <ws...> --into main --verify \"<cmd>\"`, `oz resolve <c1|--all> --take ours|theirs|base [--continue]`, `oz stack status|rebase`, `oz sync [--push]`.
- **GitHub PR / Issue / CI:** `oz pr create|edit|list|status|view|comments|reply|resolve|diff|read|review|stack`, `oz issue view|list|comment`, `oz ci [--watch|--runs|--pr <ids>|--workflow <w>|--job <id>] --json`, `oz push [-u] [--force-with-lease] [--watch --json]`.
- **Diagnostics & Fallback:** `oz doctor`, `oz fsck [--repair]`, `oz gc`, `oz git <cmd>`.

For multi-step workflows (conflict resolution in `../<repo>.oz/_integrate`, workspace lifecycle, stacked PR sync, CI log drill-down, and `oz.v1` JSON schema), read the `oz` skill (`SKILL.md`) or run `oz <cmd> --help`.
"
}

/// Comprehensive on-demand Ozone workflow body written into `SKILL.md` templates.
#[must_use]
pub fn render_ozone_workflow_body() -> &'static str {
    "\
# Ozone (`oz`) Agent Workflow Guide

Ozone is a pure-Rust colocated Git client (`.git/ozone/`) with deterministic `[f1]` / `h1a` IDs, an `< 10 ms` MVCC OpLog (`oz oplog` / `oz checkpoint` / `oz undo` / `oz redo`), warm-cached CoW workspaces, native GitHub PR/issue/CI commands, and an OpLog-safe `oz git` shim.

> **Targeting Repositories:** Pass global `oz -C <dir> <cmd>` (or `--directory <dir>`) to target any repository without `cd <dir> && ...`. Add `--json` for the canonical `oz.v1` envelope.

## 1. Fast Inspection (`status`, `diff`, `log`, `show`, `grep`, `blame`, `rev-parse`)

- **Compact status:** `oz status` (`--hunks` to expand >5 collapsed hunks; `oz status --json` for structured output). Combines status, `(+/-)` diffstat, and deterministic `[f1]` / `h1a` IDs.
- **Line-numbered diffs & excludes:** `oz diff [A..B | --base <rev> --rev <rev>] [--stat] [--line-numbers] [--exclude <pat|glob|f1|h1a>] [targets...]` (also supports `:!<path>` exclusions; `oz diff --json` always includes exact 1-based `old_line`/`new_line`).
- **History & branch comparison:** `oz log --oneline -n 10 [REV...] [-- <paths...>]` — supports multiple revs (`HEAD origin/main`), symmetric diff (`A...B --left-right`), `-i`/`--regexp-ignore-case`, `--no-notes`, `--format fuller`, `--author`, `--grep`, `-S`/`--pickaxe`, `-G`/`--grep-diff`, and `--since`.
- **Commits, blobs & rev-scoped grep (no checkout needed):**
  - `oz show <rev> [--stat] [--json]` and `oz show <rev>:<path> [--lines 1..50] [--line-numbers]`.
  - `oz grep <pattern> [<rev>] [-i] [-F] [-l] [--exclude <glob>] [--max-line-len N] [-- <paths...>]` — **uses Rust/ERE regex (`a|b`), NOT Git BRE (`a\\|b`)**; line numbers are always on and lines >400 chars truncate by default (`--max-line-len 0` disables).
- **Line-level human vs. AI attribution (`oz blame`):** `oz blame <path> [--rev <rev>] [--json]` reports per-line human vs. AI authorship backed by local `refs/notes/ai` git notes (never stored in commit messages; interoperable with `git notes --ref=ai show <oid>`).
- **Branches & revisions:** `oz branch [-a|-r] [--json]` and `oz rev-parse [--short[=<len>]] [--verify] [--abbrev-ref] [--show-toplevel] [--is-inside-work-tree] [revs...]`.

## 2. Stage-Free Commits, Amending, Signing, Guardrails & Restores

- **Stage-free & surgical commits:** `oz commit` includes untracked files automatically (no `git add`), appends a `Change-Id:` trailer, and records AI provenance strictly in `refs/notes/ai` (NEVER in commit messages):
  - Whole tree: `oz commit -m \"feat: ...\"`
  - Partial files/hunks: `oz commit -m \"feat: ...\" --pick f1 h2a` or `--exclude f3`.
- **Amending & SSH signing:**
  - `oz commit --amend [-m \"...\"] [--no-edit]` amends `HEAD` while preserving its `Change-Id:` trailer.
  - `oz commit -S` / `--sign` (or `--no-sign`, `--signing-key <key>`) signs with pure-Rust OpenSSH Ed25519 `SSHSIG` (`namespace = \"git\"`).
- **Pre-commit guardrails:** Ozone automatically blocks suspected secrets, unresolved conflict markers, `> 50 MB` blobs, and workspace `--scope`/`--deny` violations. Override only when intentional: `--force` (content guardrails), `--allow-out-of-scope` (workspace scope/deny guardrails), `--allow-empty`, or `-n`/`--no-verify` (skip Git hooks).
- **Reversible restores:** `oz restore <path|f1> [--source <rev>] [--remove-untracked]` reverts dirty edits safely with OpLog undo.

## 3. OpLog Inspection, Checkpoints, Undo & Safe Git Fallback

- **Checkpoints:** Run `oz checkpoint --quiet` (or `-m \"before script\"`) before destructive bulk shell scripts to capture an `< 10 ms` snapshot.
- **Inspect OpLog entries:** Run `oz oplog [-n|--limit <N>] [--json]` or `oz undo --list [-n|--limit <N>] [--json]` (non-destructive) to list recent `op_...` IDs, timestamps, operation kinds, and the undo cursor.
- **Rewind & reapply:** `oz undo` (last mutation), `oz undo --steps N`, `oz undo <OP_ID>`, and `oz redo`. Never run destructive `git reset --hard` or `git checkout -- .`.
- **OpLog-safe Git fallback (`oz git <cmd>`):** For any non-native Git command (e.g., `oz git reflog`, `oz git bisect ...`, `oz git rm ...`), invoke `oz git <cmd>` instead of raw `git`. Mutating fallback commands are wrapped in an OpLog transaction so `oz undo` reverses them safely.

## 4. Workspaces, Collision Radar & Conflict Resolution

- **Workspace layout & lifecycle (`../<repo>.oz/<name>`):**
  - Spawn: `oz ws spawn <name> --from origin/main --branch <branch> [--backend cas_hardlink] [--no-warm] [--scope \"crates/foo/**\"] [--deny \"Cargo.lock\"]` — materializes cleanly at `../<repo>.oz/<name>` and seeds warm `target/`, `node_modules/`, and `.venv/` caches automatically (on Linux, run `oz setup --systemd-user` once per machine so the user-wide `oz-vfsd.service` mounts FUSE3 natively even under `NoNewPrivs`).
  - List & health: `oz ws list [--json]` (shows branch, backend, mount health, dirty count, path).
  - Clean up: `oz ws rm <name> [--force] [--delete-branch]` and `oz ws gc [--max-gb N] [--target-branch main] [--dry-run]` (prunes merged/missing workspaces and LRU warm baselines).
- **Radar, merge & atomic integration:**
  - Check cross-workspace overlaps: `oz radar --json` (`--base main`, `--no-committed`).
  - Merge into current branch (OpLog-reversible): `oz merge <rev>... [--no-ff|--ff-only] [-m \"...\"] [--verify \"<cmd>\"] [--continue|--abort]`.
  - Integrate multiple workspaces atomically: `oz integrate <ws...> --into main --verify \"cargo test\"` (leaves `main` and agent workspaces untouched unless `--verify` passes).
- **Conflict resolution recipe (`../<repo>.oz/_integrate`):**
  1. On conflict, `oz integrate` / `oz merge` / `oz stack rebase` exits `1` (leaving target branches untouched) and materializes conflicts exclusively in `../<repo>.oz/_integrate` with deterministic IDs (`c1`, `c2`, ...).
  2. Resolve via CLI (`oz resolve c1 --take ours|theirs|base` or `oz resolve --all --take ours --continue`) or edit files directly in `../<repo>.oz/_integrate`.
  3. Resume with `oz integrate --continue [--verify \"cargo test\"]` (or `oz merge --continue` / `oz stack rebase --continue`), or cancel cleanly with `oz integrate --abort` (removes `_integrate`).

## 5. Stacked PRs, `oz sync`, GitHub PR/Issue Lifecycle & CI Watching

- **Stacked branches & `oz sync` recipe:**
  - Inspect & rebase local stacks preserving `Change-Id:`: `oz stack status [--onto main]` and `oz stack rebase [--onto main] [--continue|--abort]`.
  - Push stack atomically & create/update cross-linked PRs: `oz pr stack [--base main] [--draft] [--no-push]`.
  - Sync after PRs merge: `oz sync [--base main] [--push] [--no-fetch] [--no-rebase] [--no-gc] [--no-warm]` — fetches remote, fast-forwards base, retargets downstream stacked PR bases on GitHub, rebases downstream local stacks onto updated base, garbage-collects merged workspaces, and refreshes warm baselines (add `--push` to `--force-with-lease` push rebased downstream branches).
- **PR create, edit, list & triage:**
  - `oz pr create [--title \"...\"] [--body \"...\" | --body-file <path|->] [--base main] [--head <branch>] [--draft] [--no-push]`
  - `oz pr edit <n> [--title ...] [--body ... | --body-file <path|->] [--base ...] [--state open|closed] [--draft|--ready]`
  - `oz pr list [--state open|closed|merged|all] [--search \"...\"] [--author ...] [--base ...] [--label ...] [--limit N] [--json]`
  - `oz pr status [--pr 1,2,3] [--json]`
- **PR review & inline threads:**
  - `oz pr view <n> [--brief|--no-body|--no-files] [--comments]`
  - `oz pr comments <n> [--unresolved] [--json]` (displays `[t1] #<comment_id>` handles & resolved/outdated state)
  - `oz pr reply <n> <t1|#id|PRRT_...> (-m \"...\" | --body-file <path|->) [--resolve]`
  - `oz pr resolve <n> <t1|PRRT_...|#id> [--unresolve]`
  - `oz pr diff <n> [--stat] [--exclude <pat>] [--no-line-numbers]` and `oz pr read <n> <path> [--lines 1..50]`
  - `oz pr review <n> --comment|--approve|--request-changes [--body ... | --body-file <path|->] [--inline <path>:<line>[:LEFT|RIGHT]=<body>...] [--comments-file <json|->] [--dry-run]`
- **Issues (REST-backed):**
  - `oz issue view <n|url> [--comments] [--json]`
  - `oz issue list [--state open|closed|all] [--search \"...\"] [--author ...] [--assignee ...] [--label ...] [--limit N] [--json]`
  - `oz issue comment <n|url> (--body \"...\" | --body-file <path|->)`
- **Push & CI watching** (passing checks collapsed into `passed_checks` by default; pass `--verbose` for full check URLs/IDs):
  - `oz push [-u] [origin] [b1 b2 ...] [--atomic] [--force-with-lease] [--watch --json]`
  - `oz ci --watch --json`, `oz ci --json`, `oz ci --pr 1,2,3 --json`, `oz ci --runs [--branch main] [--limit N] --json`, `oz ci --workflow <file|name> [--branch main] [--limit N] [--watch] --json`, `oz ci --job <id> --max-excerpt-lines 150 [--step <name>] [--raw] --json`.

## 6. Diagnostics, Recovery & Automation (`--agent`, `--json` `oz.v1`)

- **Diagnostics & maintenance:**
  - `oz doctor [--json]`: checks OS/filesystem (`statfs`), FUSE3/reflink/ProjFS readiness, `oz-vfsd` health, SSH signing, GitHub auth, and active workspace mounts.
  - `oz fsck [--repair] [--json]`: verifies `.git/`, OpLog chain, snapshot reachability, and orphaned mounts/baselines (`--repair` safely recovers interrupted transactions and broken metadata without touching user working-tree files).
  - `oz gc [--oplog-days 30] [--max-entries N] [--dry-run] [--json]`: compacts `.git/ozone/oplog/` and prunes expired OpLog snapshots safely.
- **Non-interactive agent mode (`--agent` / `OZ_AGENT=1`):**
  - Pass `--agent` or set `OZ_AGENT=1` (also auto-enabled by `--json`, `CI=1`, or non-TTY stdio) to suppress interactive prompts, `$EDITOR`, and pagers (delegated commands automatically receive `GIT_TERMINAL_PROMPT=0`, `GIT_PAGER=cat`, `PAGER=cat`, `GCM_INTERACTIVE=never`, `OZ_AGENT=1`).
- **Canonical `--json` `oz.v1` envelope:**
  - Every `--json` command emits `{ \"schema\": \"oz.v1\", \"ok\": bool, \"op_id\": string|null, \"workspace\": string|null, \"branch\": string|null, \"head_oid\": string|null, \"data\": ..., \"warnings\": [...], \"conflicts\": [...], \"recovery_commands\": [...], \"error\"?: { \"code\", \"exit_code\", \"message\" } }` (`error` appears only when `ok == false`).
  - Exit codes: `0` success, `1` conflict or verification/CI failure, `2` usage, config, or guardrail error.
"
}

fn render_skill_markdown(skill_name: &str) -> String {
    format!(
        "---\nname: {skill_name}\ndescription: Stage-free Git client (`oz`) with addressable file/hunk commits (`--pick`, `--amend`, `-S`), OpLog inspection & `< 10 ms` undo (`oplog`, `undo --list`), `oz git` fallback, CoW workspaces, conflict resolution (`_integrate`, `resolve`), stacked PRs (`stack`, `sync`), GitHub PR/issue/CI workflows, `blame`, and `doctor`/`fsck`/`gc`.\n---\n\n{}",
        render_ozone_workflow_body()
    )
}

fn render_cursor_mdc() -> String {
    format!(
        "---\ndescription: Ozone (`oz`) compact always-on rules — stage-free commits, OpLog undo, `oz git` fallback, workspaces, PR/issue/CI, and non-interactive agent mode\nglobs:\nalwaysApply: true\n---\n\n{}",
        render_ozone_rule_body()
    )
}

fn render_windsurf_rule() -> String {
    format!(
        "---\ntrigger: always_on\ndescription: Ozone (`oz`) compact always-on rules — stage-free commits, OpLog undo, `oz git` fallback, workspaces, PR/issue/CI, and non-interactive agent mode\n---\n\n{}",
        render_ozone_rule_body()
    )
}

fn render_agents_md_block() -> String {
    format!(
        "{AGENTS_MD_START_MARKER}\n{}\n{AGENTS_MD_END_MARKER}",
        render_ozone_rule_body().trim_end()
    )
}

fn apply_exact_text_file(
    repo_root: &Path,
    rel_path: &Path,
    desired: &str,
    check_only: bool,
    dispositions: &mut BTreeMap<PathBuf, FileDisposition>,
) -> Result<()> {
    let full_path = repo_root.join(rel_path);
    if full_path.is_file() {
        let existing = fs::read_to_string(&full_path)?;
        if existing == desired {
            dispositions.insert(rel_path.to_path_buf(), FileDisposition::Unchanged);
            return Ok(());
        }
        if !check_only {
            write_atomic_text(&full_path, desired)?;
        }
        dispositions.insert(rel_path.to_path_buf(), FileDisposition::Updated);
    } else {
        if !check_only {
            write_atomic_text(&full_path, desired)?;
        }
        dispositions.insert(rel_path.to_path_buf(), FileDisposition::Created);
    }
    Ok(())
}

fn apply_agents_md_section(
    repo_root: &Path,
    rel_path: &Path,
    check_only: bool,
    dispositions: &mut BTreeMap<PathBuf, FileDisposition>,
) -> Result<()> {
    let full_path = repo_root.join(rel_path);
    let block = render_agents_md_block();

    if !full_path.is_file() {
        let content = format!("{block}\n");
        if !check_only {
            write_atomic_text(&full_path, &content)?;
        }
        dispositions.insert(rel_path.to_path_buf(), FileDisposition::Created);
        return Ok(());
    }

    let existing = fs::read_to_string(&full_path)?;
    let merged = merge_marked_markdown(&existing, &block);
    if merged == existing {
        dispositions.insert(rel_path.to_path_buf(), FileDisposition::Unchanged);
    } else {
        if !check_only {
            write_atomic_text(&full_path, &merged)?;
        }
        dispositions.insert(rel_path.to_path_buf(), FileDisposition::Updated);
    }
    Ok(())
}

fn merge_marked_markdown(existing: &str, block: &str) -> String {
    if let Some(start_idx) = existing.find(AGENTS_MD_START_MARKER)
        && let Some(rel_end_idx) = existing[start_idx..].find(AGENTS_MD_END_MARKER)
    {
        let end_idx = start_idx + rel_end_idx + AGENTS_MD_END_MARKER.len();
        let mut out = String::with_capacity(existing.len() + block.len());
        out.push_str(&existing[..start_idx]);
        out.push_str(block);
        out.push_str(&existing[end_idx..]);
        return out;
    }

    let trimmed = existing.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        format!("{block}\n")
    } else {
        format!("{trimmed}\n\n{block}\n")
    }
}

fn apply_json_merge_file<F>(
    repo_root: &Path,
    rel_path: &Path,
    mutator: F,
    check_only: bool,
    dispositions: &mut BTreeMap<PathBuf, FileDisposition>,
) -> Result<()>
where
    F: FnOnce(&mut Map<String, Value>) -> Result<bool>,
{
    let full_path = repo_root.join(rel_path);
    let existed = full_path.is_file();

    let mut root_map = if existed {
        let raw = fs::read_to_string(&full_path)?;
        if raw.trim().is_empty() {
            Map::new()
        } else {
            let parsed: Value =
                serde_json::from_str(&raw).map_err(|err| OzError::InvalidConfig {
                    path: full_path.clone(),
                    message: format!("expected valid JSON object for hook merge: {err}"),
                })?;
            match parsed {
                Value::Object(map) => map,
                _ => {
                    return Err(OzError::InvalidConfig {
                        path: full_path,
                        message: "top-level JSON value must be an object".to_string(),
                    });
                }
            }
        }
    } else {
        Map::new()
    };

    let mutated = mutator(&mut root_map)?;
    if existed && !mutated {
        dispositions.insert(rel_path.to_path_buf(), FileDisposition::Unchanged);
        return Ok(());
    }

    let mut serialized = serde_json::to_string_pretty(&Value::Object(root_map))?;
    serialized.push('\n');

    if !check_only {
        write_atomic_text(&full_path, &serialized)?;
    }

    let disp = if existed {
        FileDisposition::Updated
    } else {
        FileDisposition::Created
    };
    dispositions.insert(rel_path.to_path_buf(), disp);
    Ok(())
}

/// Non-destructively merges `oz checkpoint --quiet` into `.claude/settings.json` `hooks.PostToolUse`.
fn merge_claude_settings_hooks(root: &mut Map<String, Value>) -> Result<bool> {
    let hooks_obj = ensure_child_object(root, "hooks")?;
    let post_tool_use = ensure_child_array(hooks_obj, "PostToolUse")?;

    if array_contains_oz_checkpoint(post_tool_use) {
        return Ok(false);
    }

    post_tool_use.push(json!({
        "matcher": "Edit|MultiEdit|Write|Bash",
        "hooks": [
            {
                "type": "command",
                "command": OZ_CHECKPOINT_HOOK_CMD
            }
        ]
    }));
    Ok(true)
}

/// Non-destructively merges `oz checkpoint --quiet` into `.cursor/hooks.json` (`afterFileEdit` + `stop`).
fn merge_cursor_hooks(root: &mut Map<String, Value>) -> Result<bool> {
    let mut changed = false;
    if !root.contains_key("version") {
        root.insert("version".to_string(), json!(1));
        changed = true;
    }

    let hooks_obj = ensure_child_object(root, "hooks")?;
    for event in ["afterFileEdit", "stop"] {
        let arr = ensure_child_array(hooks_obj, event)?;
        if !array_contains_oz_checkpoint(arr) {
            arr.push(json!({ "command": OZ_CHECKPOINT_HOOK_CMD }));
            changed = true;
        }
    }
    Ok(changed)
}

/// Non-destructively merges `oz checkpoint --quiet` into `.codex/hooks.json` (`PostToolUse`).
fn merge_codex_hooks(root: &mut Map<String, Value>) -> Result<bool> {
    let hooks_obj = ensure_child_object(root, "hooks")?;
    let post_tool_use = ensure_child_array(hooks_obj, "PostToolUse")?;

    if array_contains_oz_checkpoint(post_tool_use) {
        return Ok(false);
    }

    post_tool_use.push(json!({
        "matcher": ".*",
        "hooks": [
            {
                "type": "command",
                "command": OZ_CHECKPOINT_HOOK_CMD
            }
        ]
    }));
    Ok(true)
}

/// Non-destructively merges `oz checkpoint --quiet` into `.windsurf/hooks.json` Cascade hooks.
fn merge_windsurf_hooks(root: &mut Map<String, Value>) -> Result<bool> {
    let hooks_obj = ensure_child_object(root, "hooks")?;
    let mut changed = false;

    for event in ["post_write_code", "post_run_command"] {
        let arr = ensure_child_array(hooks_obj, event)?;
        if !array_contains_oz_checkpoint(arr) {
            arr.push(json!({
                "command": OZ_CHECKPOINT_HOOK_CMD,
                "show_output": false
            }));
            changed = true;
        }
    }
    Ok(changed)
}

/// Non-destructively merges `oz checkpoint --quiet` into `.agent/hooks.json` (`PostToolUse`).
fn merge_antigravity_hooks(root: &mut Map<String, Value>) -> Result<bool> {
    let hooks_obj = ensure_child_object(root, "hooks")?;
    let post_tool_use = ensure_child_array(hooks_obj, "PostToolUse")?;

    if array_contains_oz_checkpoint(post_tool_use) {
        return Ok(false);
    }

    post_tool_use.push(json!({
        "matcher": "edit_file|write_file|run_command",
        "hooks": [
            {
                "type": "command",
                "command": OZ_CHECKPOINT_HOOK_CMD
            }
        ]
    }));
    Ok(true)
}

fn ensure_child_object<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>> {
    let entry = parent
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    entry.as_object_mut().ok_or_else(|| OzError::InvalidUsage {
        message: format!("expected JSON field '{key}' to be an object"),
        recovery: Vec::new(),
    })
}

fn ensure_child_array<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Vec<Value>> {
    let entry = parent
        .entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    entry.as_array_mut().ok_or_else(|| OzError::InvalidUsage {
        message: format!("expected JSON field '{key}' to be an array"),
        recovery: Vec::new(),
    })
}

fn array_contains_oz_checkpoint(items: &[Value]) -> bool {
    items.iter().any(value_contains_oz_checkpoint)
}

fn value_contains_oz_checkpoint(val: &Value) -> bool {
    match val {
        Value::String(s) => is_oz_checkpoint_cmd(s),
        Value::Array(arr) => arr.iter().any(value_contains_oz_checkpoint),
        Value::Object(map) => {
            if let Some(Value::String(cmd)) = map.get("command")
                && is_oz_checkpoint_cmd(cmd)
            {
                return true;
            }
            if let Some(nested) = map.get("hooks")
                && value_contains_oz_checkpoint(nested)
            {
                return true;
            }
            false
        }
        _ => false,
    }
}

fn is_oz_checkpoint_cmd(cmd: &str) -> bool {
    let normalized = cmd.trim();
    normalized.contains("oz checkpoint") || normalized.contains("git-ozone checkpoint")
}

fn write_atomic_text(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("oz_setup");
    let tmp_path = path.with_file_name(format!(".{file_name}.oz-tmp-{}", std::process::id()));
    fs::write(&tmp_path, content.as_bytes())?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn setups_all_five_harnesses_and_is_idempotent() {
        let dir = TempDir::new().expect("tempdir");
        let opts = AgentSetupOptions::all_harnesses();

        // Check / dry-run first: must report created paths without writing anything to disk.
        let dry_opts = opts.clone().with_check_only(true);
        let dry_report = setup_agent_harnesses(dir.path(), &dry_opts).expect("dry run");
        assert!(dry_report.check_only);
        assert_eq!(dry_report.harnesses, AgentHarness::ALL.to_vec());
        assert!(!dry_report.created.is_empty());
        assert!(dry_report.updated.is_empty());
        assert!(dry_report.unchanged.is_empty());
        assert!(!dir.path().join(".claude/skills/oz/SKILL.md").exists());

        // Real setup creates all skill, rule, AGENTS.md, and hook files.
        let first = setup_agent_harnesses(dir.path(), &opts).expect("first setup");
        assert!(!first.check_only);
        assert_eq!(first.created.len(), dry_report.created.len());
        assert!(first.updated.is_empty());
        assert!(first.unchanged.is_empty());

        for expected_rel in [
            ".claude/skills/oz/SKILL.md",
            ".claude/settings.json",
            ".cursor/rules/oz.mdc",
            ".cursor/hooks.json",
            ".agents/skills/oz/SKILL.md",
            ".codex/skills/oz/SKILL.md",
            "AGENTS.md",
            ".codex/hooks.json",
            ".windsurf/rules/oz.md",
            ".windsurf/hooks.json",
            ".agent/skills/oz/SKILL.md",
            ".agent/hooks.json",
        ] {
            let full = dir.path().join(expected_rel);
            assert!(full.is_file(), "expected {expected_rel} to exist");
        }

        // Verify key workflow instructions are present.
        let claude_skill =
            fs::read_to_string(dir.path().join(".claude/skills/oz/SKILL.md")).expect("read skill");
        assert!(claude_skill.contains("oz -C <dir>"));
        assert!(claude_skill.contains("--directory <dir>"));
        assert!(claude_skill.contains("oz status --json"));
        assert!(claude_skill.contains("oz diff --json"));
        assert!(claude_skill.contains("--pick"));
        assert!(claude_skill.contains("oz ws spawn"));
        assert!(claude_skill.contains("cas_hardlink"));
        assert!(claude_skill.contains("oz radar --json"));
        assert!(claude_skill.contains("oz integrate"));
        assert!(claude_skill.contains("oz undo"));
        assert!(claude_skill.contains("oz ci --json"));

        // Verify Cursor .mdc frontmatter and Antigravity YAML frontmatter.
        let cursor_mdc =
            fs::read_to_string(dir.path().join(".cursor/rules/oz.mdc")).expect("read mdc");
        assert!(cursor_mdc.starts_with("---\ndescription:"));
        assert!(cursor_mdc.contains("alwaysApply: true"));

        let agy_skill =
            fs::read_to_string(dir.path().join(".agent/skills/oz/SKILL.md")).expect("read agy");
        assert!(agy_skill.starts_with("---\nname: oz\ndescription:"));

        // Second run must be 100% idempotent (zero created, zero updated, all unchanged).
        let second = setup_agent_harnesses(dir.path(), &opts).expect("second setup");
        assert!(second.is_up_to_date());
        assert!(second.created.is_empty());
        assert!(second.updated.is_empty());
        assert_eq!(second.unchanged.len(), first.created.len());
    }

    #[test]
    fn merges_claude_settings_and_agents_md_without_clobbering_existing_content() {
        let dir = TempDir::new().expect("tempdir");
        fs::create_dir_all(dir.path().join(".claude")).expect("mkdir .claude");
        let initial_settings = json!({
            "model": "claude-sonnet-4-5",
            "permissions": {
                "allow": ["Bash(cargo test:*)"]
            },
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [{ "type": "command", "command": "echo pre-hook" }]
                    }
                ],
                "PostToolUse": [
                    {
                        "matcher": "Write",
                        "hooks": [{ "type": "command", "command": "cargo fmt --check" }]
                    }
                ]
            }
        });
        fs::write(
            dir.path().join(".claude/settings.json"),
            serde_json::to_string_pretty(&initial_settings).unwrap(),
        )
        .expect("write initial settings.json");

        fs::write(
            dir.path().join("AGENTS.md"),
            "# Project Instructions\n\nAlways run `cargo test` before finishing.\n",
        )
        .expect("write initial AGENTS.md");

        let opts =
            AgentSetupOptions::for_harnesses([AgentHarness::ClaudeCode, AgentHarness::CodexCli]);
        let report = setup_agent_harnesses(dir.path(), &opts).expect("setup");
        assert!(
            report
                .updated
                .contains(&PathBuf::from(".claude/settings.json"))
        );
        assert!(report.updated.contains(&PathBuf::from("AGENTS.md")));

        let merged_raw =
            fs::read_to_string(dir.path().join(".claude/settings.json")).expect("read merged");
        let merged: Value = serde_json::from_str(&merged_raw).expect("parse merged");
        assert_eq!(merged["model"], "claude-sonnet-4-5");
        assert_eq!(merged["permissions"]["allow"][0], "Bash(cargo test:*)");
        assert_eq!(
            merged["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "echo pre-hook"
        );
        let post_hooks = merged["hooks"]["PostToolUse"]
            .as_array()
            .expect("PostToolUse array");
        assert_eq!(post_hooks.len(), 2);
        assert_eq!(post_hooks[0]["hooks"][0]["command"], "cargo fmt --check");
        assert_eq!(post_hooks[1]["hooks"][0]["command"], OZ_CHECKPOINT_HOOK_CMD);

        let agents_md = fs::read_to_string(dir.path().join("AGENTS.md")).expect("read AGENTS.md");
        assert!(agents_md.starts_with("# Project Instructions\n\nAlways run `cargo test`"));
        assert!(agents_md.contains(AGENTS_MD_START_MARKER));
        assert!(agents_md.contains(AGENTS_MD_END_MARKER));

        // Re-running does not duplicate hooks or AGENTS.md blocks!
        let second = setup_agent_harnesses(dir.path(), &opts).expect("second setup");
        assert!(second.is_up_to_date());
        let agents_md_after =
            fs::read_to_string(dir.path().join("AGENTS.md")).expect("read AGENTS.md again");
        assert_eq!(agents_md, agents_md_after);
        assert_eq!(agents_md_after.matches(AGENTS_MD_START_MARKER).count(), 1);
    }

    #[test]
    fn auto_detects_harnesses_from_repo_home_and_env_markers() {
        let repo = TempDir::new().expect("repo");
        let home = TempDir::new().expect("home");

        fs::create_dir_all(repo.path().join(".cursor")).expect("mkdir .cursor");
        fs::create_dir_all(home.path().join(".windsurf")).expect("mkdir .windsurf");

        let mut env = BTreeMap::new();
        env.insert("CLAUDECODE".to_string(), "1".to_string());
        env.insert("ANTIGRAVITY_SESSION_ID".to_string(), "sess-123".to_string());

        let detected = detect_harnesses_with_context(repo.path(), Some(home.path()), &env);
        assert_eq!(
            detected,
            vec![
                AgentHarness::ClaudeCode,
                AgentHarness::Cursor,
                AgentHarness::Windsurf,
                AgentHarness::Antigravity,
            ]
        );
    }
}
