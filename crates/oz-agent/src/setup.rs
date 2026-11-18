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

/// Core high-signal Ozone workflow body shared across skill and rule formats.
#[must_use]
pub fn render_ozone_workflow_body() -> &'static str {
    "\
# Ozone (`oz`) Agent Workflow

Use `oz` instead of multi-step `git add` / `git stash` / `git reset` commands to avoid `.git/index.lock` contention and cut token/command overhead:

- **Status & Hunk IDs:** Run `oz status --json` (or `oz diff --json`) for `O(dirty)` status with deterministic file IDs (`f1`, `f2`) and hunk IDs (`h1a`, `h1b`).
- **One-Shot Surgical Commits:** Commit whole working trees with `oz commit -m \"feat: ...\"` or commit exact files/hunks without staging via `oz commit -m \"feat: ...\" --pick f1 h2a` (or `--exclude h1b`). Never run interactive `git add -p`.
- **Isolated CoW Workspaces:** Spawn instant warm workspaces with `oz ws spawn <name> --from main --scope \"crates/foo/**\" --deny \"Cargo.lock\"`.
- **Live Collision Radar:** Check overlapping edits across parallel workspaces before conflicts happen with `oz radar --json`.
- **Atomic In-Memory Integration:** Merge workspaces safely with test verification via `oz integrate <ws1> <ws2> --into main --verify \"cargo test\"` (and resolve indexed conflicts with `oz resolve c1 --take ours|theirs`).
- **Instant Rewind & Checkpoints:** Snapshot dirty state with `oz checkpoint --quiet` and reverse any mutation or checkpoint in `< 10 ms` via `oz undo [op_id | --steps N]`.
- **CI Self-Healing:** Inspect compact failing GitHub Actions logs with `oz ci --json`.
- **MCP Tools:** When `oz mcp` is connected, prefer structured tools (`oz_status`, `oz_diff`, `oz_commit`, `oz_ws_spawn`, `oz_radar`, `oz_checkpoint`, `oz_undo`, `oz_ci_logs`).
"
}

fn render_skill_markdown(skill_name: &str) -> String {
    format!(
        "---\nname: {skill_name}\ndescription: Stage-free Git client (`oz`) with addressable file/hunk commits (`--pick`), zero-bloat CoW workspaces, collision radar, and `< 10 ms` MVCC undo.\n---\n\n{}",
        render_ozone_workflow_body()
    )
}

fn render_cursor_mdc() -> String {
    format!(
        "---\ndescription: Ozone (`oz`) stage-free Git workflow, hunk commits (`--pick`), CoW workspaces, collision radar, and MVCC undo\nglobs:\nalwaysApply: true\n---\n\n{}",
        render_ozone_workflow_body()
    )
}

fn render_windsurf_rule() -> String {
    format!(
        "---\ntrigger: always_on\ndescription: Ozone (`oz`) stage-free Git workflow, hunk commits, CoW workspaces, radar, and undo\n---\n\n{}",
        render_ozone_workflow_body()
    )
}

fn render_agents_md_block() -> String {
    format!(
        "{AGENTS_MD_START_MARKER}\n{}\n{AGENTS_MD_END_MARKER}",
        render_ozone_workflow_body().trim_end()
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
        assert!(claude_skill.contains("oz status --json"));
        assert!(claude_skill.contains("oz diff --json"));
        assert!(claude_skill.contains("--pick"));
        assert!(claude_skill.contains("oz ws spawn"));
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
