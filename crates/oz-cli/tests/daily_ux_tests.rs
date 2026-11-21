// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Phase 5C Everyday Human Ergonomics integration tests covering:
//! - `oz log` (graph, `--oneline`, `-n`, `--author`, `--grep`, `-S`, `-G`, `--since`, `--json`)
//! - `oz show <rev>` (metadata, `Change-Id`, `refs/notes/ai` badge, `--stat`, diff, `--json`)
//! - `oz stash` (`push` / `pop` / `list` / `drop`, OpLog backup, `oz undo` & `oz redo`, conflicts)
//! - `oz cherry-pick <rev>` & `oz revert <rev>` (`--json`, `--no-commit`, `oz undo`)
//! - `oz tag` (lightweight, annotated, SSH-signed verified by `git verify-tag`, delete, `oz undo`)
//! - `oz branch` (list, create, rename including current branch, delete `-d`/`-D`, `oz undo`)
//! - Automatic TTY pager (`$OZ_PAGER` / `$GIT_PAGER` / `delta` / `less -FRX` and strict JSON/agent/non-TTY suppression)
//! - Shell completions (`oz completions bash|zsh|fish|powershell`)
//! - Built-in (`st`, `ci`, `co`, `lg`, `br`) and custom `.ozone.toml` `[aliases]` with `--json`, `--agent`, `-C`

use oz_cli::pager::{PagerChoice, PagerSelectionInput, resolve_pager_spec};
use oz_core::OZ_SCHEMA_V1;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn find_delta_binary() -> Option<PathBuf> {
    let exe_name = if cfg!(windows) { "delta.exe" } else { "delta" };
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(exe_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
        let candidate = PathBuf::from(cargo_home).join("bin").join(exe_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = PathBuf::from(home).join(".cargo").join("bin").join(exe_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn oz_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oz"))
}

struct CmdOut {
    code: i32,
    stdout: String,
    stderr: String,
    json: Option<Value>,
}

impl CmdOut {
    fn unwrap_json(&self) -> &Value {
        self.json.as_ref().unwrap_or_else(|| {
            panic!(
                "expected valid JSON stdout (exit={}):\nstdout:\n{}\nstderr:\n{}",
                self.code, self.stdout, self.stderr
            )
        })
    }
}

fn is_ambient_agent_or_pager_env(key: &str) -> bool {
    key == "OZ_AGENT"
        || key.starts_with("OZ_AGENT_")
        || key == "CLAUDECODE"
        || key.starts_with("CLAUDE_")
        || key == "ANTHROPIC_MODEL"
        || key.starts_with("CURSOR_")
        || key.starts_with("CODEX_")
        || key == "OPENAI_CODEX"
        || key.starts_with("WINDSURF_")
        || key == "CODEIUM_WINDSURF"
        || key.starts_with("ANTIGRAVITY_")
        || key.starts_with("AGY_")
        || key == "OZ_PAGER"
        || key == "GIT_PAGER"
        || key == "PAGER"
        || key == "NO_COLOR"
        || key == "CI"
}

fn run_oz(cwd: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> CmdOut {
    let mut cmd = Command::new(oz_bin());
    cmd.args(args).current_dir(cwd).stdin(Stdio::null());
    for (k, _) in std::env::vars() {
        if is_ambient_agent_or_pager_env(&k) {
            cmd.env_remove(k);
        }
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("execute oz");
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let json = serde_json::from_str::<Value>(&stdout).ok();
    CmdOut {
        code,
        stdout,
        stderr,
        json,
    }
}

fn sys_git_bin() -> &'static str {
    if Path::new("/usr/bin/git").is_file() {
        "/usr/bin/git"
    } else {
        "git"
    }
}

fn ssh_keygen_bin() -> &'static str {
    if Path::new("/usr/bin/ssh-keygen").is_file() {
        "/usr/bin/ssh-keygen"
    } else {
        "ssh-keygen"
    }
}

fn sys_git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new(sys_git_bin())
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("execute git");
    assert!(
        out.status.success(),
        "git {:?} failed in {}:\nstdout: {}\nstderr: {}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn assert_git_fsck_clean(repo: &Path) {
    let out = Command::new(sys_git_bin())
        .args(["fsck", "--full", "--no-dangling"])
        .current_dir(repo)
        .stdin(Stdio::null())
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck --full failed in {}:\nstdout: {}\nstderr: {}",
        repo.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).expect("mkdir repo");
    sys_git(&repo, &["init", "-b", "main"]);
    sys_git(&repo, &["config", "user.name", "Alice Dev"]);
    sys_git(&repo, &["config", "user.email", "alice@ozone.dev"]);
    fs::write(repo.join(".ozone.toml"), "backend = \"cas_hardlink\"\n").expect("write .ozone.toml");
    fs::write(repo.join("README.md"), "# Daily UX Test\n").expect("write readme");
    sys_git(&repo, &["add", "."]);
    sys_git(&repo, &["commit", "-m", "chore: initial commit"]);
    repo
}

// ============================================================================
// 1. `oz log` & `oz show`
// ============================================================================

#[test]
fn log_supports_graph_oneline_filters_pickaxe_regex_since_and_json_envelope() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());

    // Commit 2: Alice adds auth module with token_secret
    fs::write(
        repo.join("auth.rs"),
        "pub fn verify_token() -> bool {\n    let token_secret = 42;\n    token_secret == 42\n}\n",
    )
    .unwrap();
    let c2 = run_oz(&repo, &["commit", "-m", "feat: add auth verifier", "--json"], &[]);
    assert_eq!(c2.code, 0, "c2 stderr: {}", c2.stderr);

    // Branch + commit by Bob on feat/ui, then merge into main so graph has a merge fork!
    assert_eq!(run_oz(&repo, &["git", "switch", "-c", "feat/ui", "--json"], &[]).code, 0);
    fs::write(repo.join("ui.rs"), "pub fn render_banner() {}\n").unwrap();
    let bob_env = [
        ("GIT_AUTHOR_NAME", "Bob Builder"),
        ("GIT_AUTHOR_EMAIL", "bob@ozone.dev"),
        ("GIT_COMMITTER_NAME", "Bob Builder"),
        ("GIT_COMMITTER_EMAIL", "bob@ozone.dev"),
    ];
    let c3 = run_oz(&repo, &["commit", "-m", "ui: render banner", "--json"], &bob_env);
    assert_eq!(c3.code, 0, "c3 stderr: {}", c3.stderr);

    assert_eq!(run_oz(&repo, &["git", "switch", "main", "--json"], &[]).code, 0);
    fs::write(repo.join("core.rs"), "pub fn heartbeat_ok() -> i32 { 777 }\n").unwrap();
    let c4 = run_oz(
        &repo,
        &[
            "commit",
            "-m",
            "core: add agent heartbeat",
            "--agent",
            "--json",
        ],
        &[
            ("OZ_AGENT_CLIENT", "claude-code"),
            ("OZ_AGENT_MODEL", "claude-sonnet-4-6"),
            ("OZ_AGENT_SESSION", "sess-log-1"),
        ],
    );
    assert_eq!(c4.code, 0, "c4 stderr: {}", c4.stderr);
    let c4_oid = c4.unwrap_json()["data"]["commit_oid"]
        .as_str()
        .unwrap()
        .to_string();

    // Merge feat/ui into main cleanly (non-FF merge commit).
    sys_git(&repo, &["merge", "--no-ff", "feat/ui", "-m", "merge: integrate feat/ui"]);

    // Full log with graph + JSON envelope
    let log_all = run_oz(&repo, &["log", "--json"], &[]);
    assert_eq!(log_all.code, 0, "log stderr: {}", log_all.stderr);
    let lj = log_all.unwrap_json();
    assert_eq!(lj["schema"], OZ_SCHEMA_V1);
    assert_eq!(lj["ok"], true);
    let commits = lj["data"]["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 5);
    assert!(commits[0]["graph_prefix"].as_str().unwrap().contains('*'));

    // Human graph + --oneline + -n 2
    let log_graph = run_oz(&repo, &["log", "--oneline"], &[]);
    assert_eq!(log_graph.code, 0);
    assert!(log_graph.stdout.contains("|\\") || log_graph.stdout.contains("| *"));
    assert!(log_graph.stdout.contains("HEAD -> main"));

    let log_n2 = run_oz(&repo, &["log", "--oneline", "-n", "2", "--json"], &[]);
    assert_eq!(log_n2.code, 0);
    assert_eq!(log_n2.unwrap_json()["data"]["commits"].as_array().unwrap().len(), 2);

    // --author filter
    let log_bob = run_oz(&repo, &["log", "--author", "bob", "--json"], &[]);
    assert_eq!(log_bob.code, 0);
    let bob_commits = log_bob.unwrap_json()["data"]["commits"].as_array().unwrap();
    assert_eq!(bob_commits.len(), 1);
    assert_eq!(bob_commits[0]["subject"], "ui: render banner");

    // --grep filter
    let log_grep = run_oz(&repo, &["log", "--grep", "heartbeat", "--json"], &[]);
    assert_eq!(log_grep.code, 0);
    let grep_commits = log_grep.unwrap_json()["data"]["commits"].as_array().unwrap();
    assert_eq!(grep_commits.len(), 1);
    assert_eq!(grep_commits[0]["oid"], c4_oid);
    assert_eq!(grep_commits[0]["ai_provenance"]["client"], "claude-code");

    // -S pickaxe filter
    let log_pickaxe = run_oz(&repo, &["log", "-S", "token_secret", "--json"], &[]);
    assert_eq!(log_pickaxe.code, 0);
    let s_commits = log_pickaxe.unwrap_json()["data"]["commits"].as_array().unwrap();
    assert_eq!(s_commits.len(), 1);
    assert_eq!(s_commits[0]["subject"], "feat: add auth verifier");

    // -G regex diff filter
    let log_regex = run_oz(&repo, &["log", "-G", r"heartbeat_ok.*777", "--json"], &[]);
    assert_eq!(log_regex.code, 0);
    let g_commits = log_regex.unwrap_json()["data"]["commits"].as_array().unwrap();
    assert_eq!(g_commits.len(), 1);
    assert_eq!(g_commits[0]["oid"], c4_oid);

    // --since filter (relative duration includes recent commits; future timestamp excludes all)
    let log_since_recent = run_oz(&repo, &["log", "--since", "2 days ago", "--json"], &[]);
    assert_eq!(log_since_recent.code, 0);
    assert_eq!(
        log_since_recent.unwrap_json()["data"]["commits"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    let log_since_future = run_oz(&repo, &["log", "--since", "2099-01-01", "--json"], &[]);
    assert_eq!(log_since_future.code, 0);
    assert_eq!(
        log_since_future.unwrap_json()["data"]["commits"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // `oz show <rev>` verifies metadata, Change-Id, refs/notes/ai badge, --stat, and unified diff
    let show_json = run_oz(&repo, &["show", &c4_oid, "--json"], &[]);
    assert_eq!(show_json.code, 0, "show stderr: {}", show_json.stderr);
    let sj = show_json.unwrap_json();
    assert_eq!(sj["schema"], OZ_SCHEMA_V1);
    assert_eq!(sj["ok"], true);
    assert_eq!(sj["data"]["commit_oid"], c4_oid);
    assert!(sj["data"]["change_id"].as_str().unwrap().starts_with('I'));
    assert_eq!(sj["data"]["ai_provenance"]["client"], "claude-code");
    assert_eq!(sj["data"]["ai_provenance"]["model"], "claude-sonnet-4-6");
    assert_eq!(sj["data"]["ai_provenance"]["session_id"], "sess-log-1");
    assert_eq!(sj["data"]["stat"]["files_changed"], 1);
    assert_eq!(sj["data"]["stat"]["total_additions"], 1);
    assert!(
        sj["data"]["unified_diff"]
            .as_str()
            .unwrap()
            .contains("heartbeat_ok")
    );

    let show_human_stat = run_oz(&repo, &["show", &c4_oid, "--stat"], &[]);
    assert_eq!(show_human_stat.code, 0);
    assert!(show_human_stat.stdout.contains("Change-Id:"));
    assert!(show_human_stat.stdout.contains("AI-Provenance: [AI:claude-code"));
    assert!(show_human_stat.stdout.contains("core.rs | +1 -0"));
    assert!(!show_human_stat.stdout.contains("@@ "));
}

// ============================================================================
// 2. `oz stash` (`push` / `pop` / `list` / `drop`) + `oz undo` / `oz redo`
// ============================================================================

#[test]
fn stash_push_pop_list_drop_are_reversed_by_oz_undo_and_redo() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());

    // Create a tracked file to delete later and an existing file to modify.
    fs::write(repo.join("tracked.txt"), "base line\n").unwrap();
    fs::write(repo.join("to_delete.txt"), "gone soon\n").unwrap();
    assert_eq!(
        run_oz(&repo, &["commit", "-m", "chore: seed stash fixtures", "--json"], &[]).code,
        0
    );

    // Apply dirty edits: modify, add untracked, create executable, delete tracked, create symlink.
    fs::write(repo.join("tracked.txt"), "stashed edit\n").unwrap();
    fs::write(repo.join("new_file.txt"), "untracked payload\n").unwrap();
    fs::write(repo.join("run.sh"), "#!/bin/sh\necho ok\n").unwrap();
    fs::set_permissions(repo.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_file(repo.join("to_delete.txt")).unwrap();
    std::os::unix::fs::symlink("tracked.txt", repo.join("link_to_tracked")).unwrap();

    // Capture pre-stash `oz status --json` and `git status --short` for exact parity after pop.
    let pre_stash_oz_status = run_oz(&repo, &["status", "--json"], &[]).unwrap_json().clone();
    let pre_stash_changes = pre_stash_oz_status["data"]["changes"].as_array().unwrap().clone();
    assert!(
        pre_stash_changes
            .iter()
            .any(|c| c["path"] == "new_file.txt" && c["kind"] == "added" && c["pinned"] == false),
        "pre-stash oz status changes: {pre_stash_changes:?}"
    );

    // 1. `oz stash push`
    let push_out = run_oz(&repo, &["stash", "push", "-m", "wip feature", "--json"], &[]);
    assert_eq!(push_out.code, 0, "stash push stderr: {}", push_out.stderr);
    let pj = push_out.unwrap_json();
    assert_eq!(pj["schema"], OZ_SCHEMA_V1);
    assert_eq!(pj["ok"], true);
    assert!(pj["op_id"].as_str().unwrap().starts_with("op_"));
    assert_eq!(pj["data"]["stash"]["ref_label"], "stash@{0}");
    assert_eq!(pj["data"]["stash"]["message"], "wip feature");

    // Working tree must now be completely clean!
    let status_clean = run_oz(&repo, &["status", "--json"], &[]);
    assert_eq!(status_clean.code, 0);
    assert_eq!(
        status_clean.unwrap_json()["data"]["changes"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(fs::read_to_string(repo.join("tracked.txt")).unwrap(), "base line\n");
    assert!(!repo.join("new_file.txt").exists());
    assert!(repo.join("to_delete.txt").is_file());
    assert!(!repo.join("link_to_tracked").exists());

    // `oz stash list` shows 1 entry
    let list_1 = run_oz(&repo, &["stash", "list", "--json"], &[]);
    assert_eq!(list_1.code, 0);
    assert_eq!(list_1.unwrap_json()["data"]["count"], 1);
    assert_eq!(
        list_1.unwrap_json()["data"]["stashes"][0]["message"],
        "wip feature"
    );

    // 2. `oz undo` reverses `oz stash push`!
    let undo_push = run_oz(&repo, &["undo", "--json"], &[]);
    assert_eq!(undo_push.code, 0, "undo push stderr: {}", undo_push.stderr);
    assert_eq!(fs::read_to_string(repo.join("tracked.txt")).unwrap(), "stashed edit\n");
    assert_eq!(fs::read_to_string(repo.join("new_file.txt")).unwrap(), "untracked payload\n");
    assert!(!repo.join("to_delete.txt").exists());
    assert!(fs::symlink_metadata(repo.join("link_to_tracked")).unwrap().file_type().is_symlink());
    assert_eq!(
        fs::metadata(repo.join("run.sh")).unwrap().permissions().mode() & 0o111,
        0o111
    );
    let list_after_undo = run_oz(&repo, &["stash", "list", "--json"], &[]);
    assert_eq!(list_after_undo.unwrap_json()["data"]["count"], 0);

    // 3. `oz redo` re-applies `oz stash push`!
    let redo_push = run_oz(&repo, &["redo", "--json"], &[]);
    assert_eq!(redo_push.code, 0, "redo push stderr: {}", redo_push.stderr);
    assert_eq!(fs::read_to_string(repo.join("tracked.txt")).unwrap(), "base line\n");
    assert!(!repo.join("new_file.txt").exists());
    assert_eq!(run_oz(&repo, &["stash", "list", "--json"], &[]).unwrap_json()["data"]["count"], 1);

    // Advance HEAD on an unrelated file while stash is held, then verify `oz stash pop` still works!
    fs::write(repo.join("unrelated.txt"), "other commit\n").unwrap();
    assert_eq!(run_oz(&repo, &["commit", "-m", "chore: unrelated", "--json"], &[]).code, 0);

    // Conflict protection: if conflicting dirty edit exists on tracked.txt, pop returns exit code 1 and keeps stash!
    fs::write(repo.join("tracked.txt"), "conflicting local edit\n").unwrap();
    let pop_conflict = run_oz(&repo, &["stash", "pop", "--json"], &[]);
    assert_eq!(pop_conflict.code, 1);
    assert_eq!(pop_conflict.unwrap_json()["error"]["code"], "conflict");
    assert_eq!(run_oz(&repo, &["stash", "list", "--json"], &[]).unwrap_json()["data"]["count"], 1);
    // Clean conflicting file back to HEAD before popping cleanly
    fs::write(repo.join("tracked.txt"), "base line\n").unwrap();

    // 4. `oz stash pop` cleanly applies stash onto working tree and removes stash@{0}
    let pop_ok = run_oz(&repo, &["stash", "pop", "--json"], &[]);
    assert_eq!(pop_ok.code, 0, "stash pop stderr: {}", pop_ok.stderr);
    assert_eq!(fs::read_to_string(repo.join("tracked.txt")).unwrap(), "stashed edit\n");
    assert_eq!(fs::read_to_string(repo.join("new_file.txt")).unwrap(), "untracked payload\n");
    assert!(!repo.join("to_delete.txt").exists());
    assert_eq!(run_oz(&repo, &["stash", "list", "--json"], &[]).unwrap_json()["data"]["count"], 0);

    // Popping must NOT stage dirty or untracked files in `.git/index` or `oz status`.
    assert_eq!(
        sys_git(&repo, &["diff", "--cached", "--name-only"]),
        "",
        "oz stash pop must leave .git/index clean against HEAD"
    );
    let raw_git_st = Command::new(sys_git_bin())
        .args(["status", "--short"])
        .current_dir(&repo)
        .stdin(Stdio::null())
        .output()
        .expect("git status --short");
    assert!(raw_git_st.status.success());
    let raw_git_st_str = String::from_utf8_lossy(&raw_git_st.stdout);
    let git_lines: Vec<&str> = raw_git_st_str.lines().collect();
    assert!(
        git_lines.contains(&" M tracked.txt"),
        "expected unstaged ` M tracked.txt`, got:\n{raw_git_st_str}"
    );
    assert!(
        git_lines.contains(&" D to_delete.txt"),
        "expected unstaged ` D to_delete.txt`, got:\n{raw_git_st_str}"
    );
    assert!(
        git_lines.contains(&"?? new_file.txt"),
        "expected untracked `?? new_file.txt`, got:\n{raw_git_st_str}"
    );
    assert!(
        !git_lines
            .iter()
            .any(|l| l.starts_with("A ") || l.starts_with("M ") || l.starts_with("D ")),
        "no popped file should be staged in column 1:\n{raw_git_st_str}"
    );

    let post_pop_oz_status = run_oz(&repo, &["status", "--json"], &[]).unwrap_json().clone();
    let post_pop_changes = post_pop_oz_status["data"]["changes"].as_array().unwrap();
    for pre in &pre_stash_changes {
        assert!(
            post_pop_changes.iter().any(|post| {
                post["path"] == pre["path"]
                    && post["kind"] == pre["kind"]
                    && post["pinned"] == pre["pinned"]
            }),
            "post-pop oz status drifted for {}:\npre={pre:?}\npost={post_pop_changes:?}",
            pre["path"]
        );
    }
    let oz_git_st = run_oz(&repo, &["git", "status", "--short"], &[]);
    assert_eq!(oz_git_st.code, 0);
    assert!(
        oz_git_st.stdout.lines().any(|l| l == "?? new_file.txt")
            && oz_git_st.stdout.lines().any(|l| l == " M tracked.txt"),
        "oz git status --short after pop:\n{}",
        oz_git_st.stdout
    );

    // 5. `oz undo` reverses `oz stash pop`!
    let undo_pop = run_oz(&repo, &["undo", "--json"], &[]);
    assert_eq!(undo_pop.code, 0, "undo pop stderr: {}", undo_pop.stderr);
    assert_eq!(fs::read_to_string(repo.join("tracked.txt")).unwrap(), "base line\n");
    assert!(!repo.join("new_file.txt").exists());
    assert!(repo.join("to_delete.txt").is_file());
    assert_eq!(run_oz(&repo, &["stash", "list", "--json"], &[]).unwrap_json()["data"]["count"], 1);

    // 6. `oz stash drop` + `oz undo`
    let drop_out = run_oz(&repo, &["stash", "drop", "stash@{0}", "--json"], &[]);
    assert_eq!(drop_out.code, 0, "stash drop stderr: {}", drop_out.stderr);
    assert_eq!(drop_out.unwrap_json()["data"]["remaining_count"], 0);
    assert_eq!(run_oz(&repo, &["stash", "list", "--json"], &[]).unwrap_json()["data"]["count"], 0);

    let undo_drop = run_oz(&repo, &["undo", "--json"], &[]);
    assert_eq!(undo_drop.code, 0);
    assert_eq!(run_oz(&repo, &["stash", "list", "--json"], &[]).unwrap_json()["data"]["count"], 1);

    assert_git_fsck_clean(&repo);
}

// ============================================================================
// 3. `oz cherry-pick`, `oz revert`, `oz tag` (SSH signed), `oz branch`
// ============================================================================

#[test]
fn cherry_pick_revert_signed_tag_and_branch_ops_with_json_and_undo() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());

    // Generate Ed25519 SSH key for tag signing
    let key_path = tmp.path().join("tag_ed25519");
    let keygen = Command::new(ssh_keygen_bin())
        .args(["-t", "ed25519", "-N", "", "-f"])
        .arg(&key_path)
        .output()
        .expect("ssh-keygen");
    assert!(keygen.status.success());

    // Create feature branch via `oz branch`
    let br_create = run_oz(&repo, &["branch", "feat/cp", "--json"], &[]);
    assert_eq!(br_create.code, 0, "br create stderr: {}", br_create.stderr);
    assert_eq!(br_create.unwrap_json()["data"]["name"], "feat/cp");

    // Rename `feat/cp` -> `feat/cherry`
    let br_rename = run_oz(&repo, &["branch", "-m", "feat/cp", "feat/cherry", "--json"], &[]);
    assert_eq!(br_rename.code, 0, "br rename stderr: {}", br_rename.stderr);
    assert_eq!(br_rename.unwrap_json()["data"]["new_name"], "feat/cherry");

    // Switch to `feat/cherry` and make a commit
    assert_eq!(run_oz(&repo, &["git", "switch", "feat/cherry", "--json"], &[]).code, 0);
    fs::write(repo.join("feature.txt"), "from cherry branch\n").unwrap();
    let feat_commit = run_oz(&repo, &["commit", "-m", "feat: add feature.txt", "--json"], &[]);
    assert_eq!(feat_commit.code, 0);
    let feat_oid = feat_commit.unwrap_json()["data"]["commit_oid"]
        .as_str()
        .unwrap()
        .to_string();

    // Switch back to main, verify `-d` refuses unmerged `feat/cherry`
    assert_eq!(run_oz(&repo, &["git", "switch", "main", "--json"], &[]).code, 0);
    let del_unmerged = run_oz(&repo, &["branch", "-d", "feat/cherry", "--json"], &[]);
    assert_eq!(del_unmerged.code, 2);

    // Cherry-pick `feat_oid` onto `main`
    let cp_out = run_oz(&repo, &["cherry-pick", &feat_oid, "--json"], &[]);
    assert_eq!(cp_out.code, 0, "cherry-pick stderr: {}", cp_out.stderr);
    let cpj = cp_out.unwrap_json();
    assert_eq!(cpj["schema"], OZ_SCHEMA_V1);
    assert_eq!(cpj["ok"], true);
    assert!(cpj["op_id"].as_str().unwrap().starts_with("op_"));
    assert_eq!(fs::read_to_string(repo.join("feature.txt")).unwrap(), "from cherry branch\n");
    let cp_oid = cpj["data"]["commit_oid"].as_str().unwrap().to_string();

    // Revert the cherry-picked commit!
    let rev_out = run_oz(&repo, &["revert", &cp_oid, "--json"], &[]);
    assert_eq!(rev_out.code, 0, "revert stderr: {}", rev_out.stderr);
    let revj = rev_out.unwrap_json();
    assert_eq!(revj["ok"], true);
    assert!(!repo.join("feature.txt").exists());

    // `oz undo` reverses `oz revert`, bringing `feature.txt` back!
    assert_eq!(run_oz(&repo, &["undo", "--json"], &[]).code, 0);
    assert_eq!(fs::read_to_string(repo.join("feature.txt")).unwrap(), "from cherry branch\n");

    // `oz undo` reverses `oz cherry-pick`, removing `feature.txt`!
    assert_eq!(run_oz(&repo, &["undo", "--json"], &[]).code, 0);
    assert!(!repo.join("feature.txt").exists());

    // Force-delete `feat/cherry` with `-D` and undo deletion!
    let del_force = run_oz(&repo, &["branch", "-D", "feat/cherry", "--json"], &[]);
    assert_eq!(del_force.code, 0, "branch -D stderr: {}", del_force.stderr);
    assert_eq!(
        run_oz(&repo, &["undo", "--json"], &[]).code,
        0,
        "undo branch -D"
    );
    let br_list = run_oz(&repo, &["branch", "--json"], &[]);
    assert_eq!(br_list.code, 0);
    let names: Vec<&str> = br_list.unwrap_json()["data"]["branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"main"));
    assert!(names.contains(&"feat/cherry"));

    // Rename current branch `main` -> `trunk` and undo!
    let ren_cur = run_oz(&repo, &["branch", "-m", "trunk", "--json"], &[]);
    assert_eq!(ren_cur.code, 0, "rename current stderr: {}", ren_cur.stderr);
    assert_eq!(ren_cur.unwrap_json()["branch"], "trunk");
    let undo_ren = run_oz(&repo, &["undo", "--json"], &[]);
    assert_eq!(undo_ren.code, 0, "undo rename current stderr: {}", undo_ren.stderr);
    assert_eq!(undo_ren.unwrap_json()["branch"], "main");

    // Tags: lightweight, annotated, and SSH-signed verified by `oz tag -v` AND `git verify-tag`!
    let lw_tag = run_oz(&repo, &["tag", "v0.1.0", "--json"], &[]);
    assert_eq!(lw_tag.code, 0);
    assert_eq!(lw_tag.unwrap_json()["data"]["kind"], "lightweight");

    let signed_tag = run_oz(
        &repo,
        &[
            "tag",
            "-s",
            "v1.0.0",
            "-m",
            "Release v1.0.0",
            "--signing-key",
            key_path.to_str().unwrap(),
            "--json",
        ],
        &[],
    );
    assert_eq!(signed_tag.code, 0, "signed tag stderr: {}", signed_tag.stderr);
    let stj = signed_tag.unwrap_json();
    assert_eq!(stj["data"]["kind"], "signed");
    assert_eq!(stj["data"]["signed"], true);

    // Pure-Rust verify via `oz tag -v`
    let verify_oz = run_oz(&repo, &["tag", "-v", "v1.0.0", "--json"], &[]);
    assert_eq!(verify_oz.code, 0, "oz tag -v stderr: {}", verify_oz.stderr);
    assert_eq!(verify_oz.unwrap_json()["data"]["valid"], true);

    // Real system `git verify-tag` with allowedSignersFile!
    let pub_line = fs::read_to_string(tmp.path().join("tag_ed25519.pub")).unwrap();
    let allowed = tmp.path().join("allowed_signers");
    fs::write(&allowed, format!("alice@ozone.dev {pub_line}")).unwrap();
    let git_verify = Command::new(sys_git_bin())
        .args([
            "-c",
            &format!("gpg.ssh.allowedSignersFile={}", allowed.display()),
            "verify-tag",
            "v1.0.0",
        ])
        .current_dir(&repo)
        .output()
        .expect("git verify-tag");
    assert!(
        git_verify.status.success(),
        "git verify-tag failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&git_verify.stdout),
        String::from_utf8_lossy(&git_verify.stderr)
    );

    // `oz show v1.0.0 --json` peels tag cleanly and surfaces `tag_name`
    let show_tag = run_oz(&repo, &["show", "v1.0.0", "--json"], &[]);
    assert_eq!(show_tag.code, 0);
    assert_eq!(show_tag.unwrap_json()["data"]["tag_name"], "v1.0.0");

    // Delete tag and undo deletion
    let del_tag = run_oz(&repo, &["tag", "-d", "v0.1.0", "--json"], &[]);
    assert_eq!(del_tag.code, 0);
    assert_eq!(run_oz(&repo, &["undo", "--json"], &[]).code, 0);
    let tag_list_out = run_oz(&repo, &["tag", "--json"], &[]);
    let tag_names: Vec<&str> = tag_list_out.unwrap_json()["data"]["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(tag_names.contains(&"v0.1.0"));
    assert!(tag_names.contains(&"v1.0.0"));

    assert_git_fsck_clean(&repo);
}

// ============================================================================
// 4. Completions, Built-in & Custom Aliases, and Pager Suppression / PTY Pager
// ============================================================================

#[test]
fn shell_completions_generate_valid_scripts_for_all_four_shells() {
    let tmp = TempDir::new().expect("tempdir");

    for (shell, expected_marker) in [
        ("bash", "_oz("),
        ("zsh", "#compdef oz"),
        ("fish", "complete -c oz"),
        ("powershell", "Register-ArgumentCompleter"),
    ] {
        let out = run_oz(tmp.path(), &["completions", shell], &[]);
        assert_eq!(out.code, 0, "completions {shell} stderr: {}", out.stderr);
        assert!(
            out.stdout.contains(expected_marker),
            "expected '{expected_marker}' in {shell} completions:\n{}",
            &out.stdout[..out.stdout.len().min(400)]
        );
        assert!(out.stdout.contains("cherry-pick"));
        assert!(out.stdout.contains("completions"));
    }

    let json_comp = run_oz(tmp.path(), &["completions", "bash", "--json"], &[]);
    assert_eq!(json_comp.code, 0);
    let j = json_comp.unwrap_json();
    assert_eq!(j["schema"], OZ_SCHEMA_V1);
    assert_eq!(j["ok"], true);
    assert_eq!(j["data"]["shell"], "bash");
    assert!(j["data"]["script"].as_str().unwrap().contains("_oz("));
}

#[test]
fn builtin_and_custom_aliases_expand_cleanly_with_json_agent_and_dash_c() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();

    // 1. Built-in `oz st --json`
    fs::write(repo.join("dirty.txt"), "hello\n").unwrap();
    let st = run_oz(&repo, &["st", "--json"], &[]);
    assert_eq!(st.code, 0, "st stderr: {}", st.stderr);
    assert_eq!(st.unwrap_json()["data"]["changes"][0]["path"], "dirty.txt");

    // 2. Built-in `oz ci -m "..." --json` (and verify `oz ci --ref main --branch main --json` still invokes built-in CI!)
    let ci = run_oz(&repo, &["ci", "-m", "feat: via alias", "--json"], &[]);
    assert_eq!(ci.code, 0, "ci stderr: {}", ci.stderr);
    assert_eq!(ci.unwrap_json()["data"]["committed_paths"], serde_json::json!(["dirty.txt"]));

    let ci_builtin = run_oz(
        &repo,
        &["ci", "--ref", "main", "--branch", "main", "--json"],
        &[],
    );
    assert_eq!(ci_builtin.code, 2);
    assert_eq!(ci_builtin.unwrap_json()["error"]["code"], "invalid_usage");

    // 3. Built-in `oz br` and `oz co`
    assert_eq!(run_oz(&repo, &["br", "feat/alias", "--json"], &[]).code, 0);
    let co = run_oz(&repo, &["co", "feat/alias", "--json"], &[]);
    assert_eq!(co.code, 0, "co stderr: {}", co.stderr);
    assert_eq!(co.unwrap_json()["branch"], "feat/alias");

    // 4. Built-in `oz lg`
    let lg = run_oz(&repo, &["lg", "-n", "1", "--oneline", "--json"], &[]);
    assert_eq!(lg.code, 0, "lg stderr: {}", lg.stderr);
    assert_eq!(lg.unwrap_json()["data"]["commits"][0]["subject"], "feat: via alias");

    // 5. Custom `[aliases]` override in `.ozone.toml` invoked from outside via `-C <repo>` + `--json` + `--agent`
    fs::write(
        repo.join(".ozone.toml"),
        r#"
backend = "cas_hardlink"

[aliases]
lg = "log --oneline -n 1"
cm = "commit -m"
"#,
    )
    .unwrap();
    fs::write(repo.join("custom.txt"), "via custom alias\n").unwrap();

    let cm_custom = run_oz(
        &outside,
        &["-C", repo.to_str().unwrap(), "--agent", "cm", "feat: custom", "--json"],
        &[],
    );
    assert_eq!(cm_custom.code, 0, "cm stderr: {}", cm_custom.stderr);
    assert_eq!(cm_custom.unwrap_json()["branch"], "feat/alias");

    let lg_custom = run_oz(
        &outside,
        &["--json", "lg", "-C", repo.to_str().unwrap()],
        &[],
    );
    assert_eq!(lg_custom.code, 0, "lg_custom stderr: {}", lg_custom.stderr);
    assert_eq!(
        lg_custom.unwrap_json()["data"]["commits"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        lg_custom.unwrap_json()["data"]["commits"][0]["subject"],
        "feat: custom"
    );
}

#[test]
fn pager_selection_and_strict_non_tty_json_agent_suppression() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repo(tmp.path());

    // Create a sentinel pager script that records if it was ever executed!
    let marker = tmp.path().join("pager_was_invoked");
    let spy_pager = tmp.path().join("spy-pager.sh");
    fs::write(
        &spy_pager,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&spy_pager, fs::Permissions::from_mode(0o755)).unwrap();

    // 1. Non-TTY piped stdout MUST NEVER invoke `$OZ_PAGER`
    let piped = run_oz(
        &repo,
        &["log", "--oneline"],
        &[("OZ_PAGER", spy_pager.to_str().unwrap())],
    );
    assert_eq!(piped.code, 0);
    assert!(!marker.exists(), "pager must NOT run when stdout is piped non-TTY");

    // 2. Unit-check delta binary resolution (when installed) and fallback
    if let Some(delta_bin) = find_delta_binary()
        && let Some(delta_bin_dir) = delta_bin.parent()
    {
        let choice = resolve_pager_spec(PagerSelectionInput {
            json_flag: false,
            agent_flag: false,
            oz_agent_env: None,
            ci_env: None,
            stdout_is_tty: true,
            oz_pager_env: None,
            git_pager_env: None,
            path_env: Some(delta_bin_dir.as_os_str()),
        });
        assert_eq!(
            choice,
            PagerChoice::Command {
                program: delta_bin.to_string_lossy().into_owned(),
                args: Vec::new(),
            }
        );
    }

    // 3. Real PTY test: spawn `oz log` with stdout attached to a real Linux PTY!
    run_pty_pager_verification(&repo, &spy_pager, &marker);
}

#[cfg(unix)]
fn run_pty_pager_verification(repo: &Path, spy_pager: &Path, marker: &Path) {
    use std::io::Read;
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};

    let mut master_fd: libc::c_int = -1;
    let mut slave_fd: libc::c_int = -1;
    let rc = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };

    // A) With `--json` on a real TTY, pager MUST STILL be suppressed!
    let slave_dup1 = slave.try_clone().expect("dup slave");
    let mut cmd_json = Command::new(oz_bin());
    cmd_json
        .args(["log", "--json"])
        .current_dir(repo)
        .env("OZ_PAGER", spy_pager)
        .env_remove("OZ_AGENT")
        .env_remove("CI")
        .stdin(Stdio::null())
        .stdout(unsafe { Stdio::from_raw_fd(slave_dup1.into_raw_fd()) })
        .stderr(Stdio::null());
    assert!(cmd_json.status().expect("oz log --json tty").success());
    assert!(!marker.exists(), "--json must strictly suppress pager even on TTY");

    // B) With `--agent` or `OZ_AGENT=1` on a real TTY, pager MUST STILL be suppressed!
    let slave_dup2 = slave.try_clone().expect("dup slave");
    let mut cmd_agent = Command::new(oz_bin());
    cmd_agent
        .args(["log", "--oneline", "--agent"])
        .current_dir(repo)
        .env("OZ_PAGER", spy_pager)
        .env_remove("CI")
        .stdin(Stdio::null())
        .stdout(unsafe { Stdio::from_raw_fd(slave_dup2.into_raw_fd()) })
        .stderr(Stdio::null());
    assert!(cmd_agent.status().expect("oz log --agent tty").success());
    assert!(!marker.exists(), "--agent must strictly suppress pager even on TTY");

    let slave_dup3 = slave.try_clone().expect("dup slave");
    let mut cmd_env_agent = Command::new(oz_bin());
    cmd_env_agent
        .args(["log", "--oneline"])
        .current_dir(repo)
        .env("OZ_PAGER", spy_pager)
        .env("OZ_AGENT", "1")
        .env_remove("CI")
        .stdin(Stdio::null())
        .stdout(unsafe { Stdio::from_raw_fd(slave_dup3.into_raw_fd()) })
        .stderr(Stdio::null());
    assert!(cmd_env_agent.status().expect("oz log OZ_AGENT=1 tty").success());
    assert!(!marker.exists(), "OZ_AGENT=1 must strictly suppress pager even on TTY");

    // C) Interactive TTY stdout WITHOUT --json / --agent / OZ_AGENT=1 / CI MUST invoke `$OZ_PAGER`!
    let mut cmd_tty = Command::new(oz_bin());
    cmd_tty
        .args(["log", "--oneline"])
        .current_dir(repo)
        .env("OZ_PAGER", spy_pager)
        .env_remove("OZ_AGENT")
        .env_remove("CI")
        .stdin(Stdio::null())
        .stdout(unsafe { Stdio::from_raw_fd(slave.into_raw_fd()) })
        .stderr(Stdio::null());
    assert!(cmd_tty.status().expect("oz log tty").success());
    assert!(marker.exists(), "interactive TTY stdout must invoke configured pager");

    // Drain PTY master cleanly
    let mut pty_file = std::fs::File::from(master);
    let mut buf = [0u8; 4096];
    let _ = pty_file.read(&mut buf);
    let _ = &mut pty_file;
}

#[test]
fn edge_cases_multi_stash_conflicts_no_commit_unborn_and_pty_diff_delta() {
    let tmp = TempDir::new().expect("tempdir");

    // 1. Unborn repo handling for log / show / tag
    let unborn = tmp.path().join("unborn");
    fs::create_dir_all(&unborn).unwrap();
    sys_git(&unborn, &["init", "-b", "main"]);
    fs::write(unborn.join(".ozone.toml"), "backend = \"cas_hardlink\"\n").unwrap();

    let log_unborn = run_oz(&unborn, &["log", "--json"], &[]);
    assert_eq!(log_unborn.code, 0);
    assert_eq!(log_unborn.unwrap_json()["data"]["unborn"], true);

    let show_unborn = run_oz(&unborn, &["show", "--json"], &[]);
    assert_eq!(show_unborn.code, 2);

    let tag_unborn = run_oz(&unborn, &["tag", "v0", "--json"], &[]);
    assert_eq!(tag_unborn.code, 2);

    // 2. Multi-stash stack + `oz stash` shorthand + empty-stash exit code 2
    let repo = init_repo(tmp.path());
    let empty_stash = run_oz(&repo, &["stash", "push", "--json"], &[]);
    assert_eq!(empty_stash.code, 2);

    fs::write(repo.join("s1.txt"), "first stash\n").unwrap();
    assert_eq!(
        run_oz(&repo, &["stash", "-m", "first", "--json"], &[]).code,
        0
    );
    fs::write(repo.join("s2.txt"), "second stash\n").unwrap();
    assert_eq!(
        run_oz(&repo, &["stash", "push", "-m", "second", "--json"], &[]).code,
        0
    );
    let list_two = run_oz(&repo, &["stash", "list", "--json"], &[]);
    assert_eq!(list_two.unwrap_json()["data"]["count"], 2);
    assert_eq!(
        list_two.unwrap_json()["data"]["stashes"][0]["message"],
        "second"
    );
    assert_eq!(
        list_two.unwrap_json()["data"]["stashes"][1]["message"],
        "first"
    );

    // Drop older entry `stash@{1}` and undo it
    let drop_one = run_oz(&repo, &["stash", "drop", "stash@{1}", "--json"], &[]);
    assert_eq!(drop_one.code, 0);
    assert_eq!(drop_one.unwrap_json()["data"]["remaining_count"], 1);
    assert_eq!(run_oz(&repo, &["undo", "--json"], &[]).code, 0);
    assert_eq!(
        run_oz(&repo, &["stash", "list", "--json"], &[]).unwrap_json()["data"]["count"],
        2
    );

    // 3. `cherry-pick` / `revert` `--no-commit` and conflict handling (exit code 1)
    fs::write(repo.join("shared.txt"), "line1\nline2\nline3\n").unwrap();
    let base_c = run_oz(&repo, &["commit", "-m", "base shared", "--json"], &[]);
    assert_eq!(base_c.code, 0);

    assert_eq!(run_oz(&repo, &["branch", "side", "--json"], &[]).code, 0);
    assert_eq!(run_oz(&repo, &["git", "switch", "side", "--json"], &[]).code, 0);
    fs::write(repo.join("shared.txt"), "side1\nline2\nline3\n").unwrap();
    fs::write(repo.join("side_only.txt"), "side only\n").unwrap();
    let side_c = run_oz(&repo, &["commit", "-m", "side edit", "--json"], &[]);
    let side_oid = side_c.unwrap_json()["data"]["commit_oid"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(run_oz(&repo, &["git", "switch", "main", "--json"], &[]).code, 0);
    fs::write(repo.join("shared.txt"), "main1\nline2\nline3\n").unwrap();
    let main_c = run_oz(&repo, &["commit", "-m", "main edit", "--json"], &[]);
    let main_oid = main_c.unwrap_json()["data"]["commit_oid"]
        .as_str()
        .unwrap()
        .to_string();

    // Conflicting cherry-pick returns exit code 1 (`conflict`) cleanly
    let cp_conflict = run_oz(&repo, &["cherry-pick", &side_oid, "--json"], &[]);
    assert_eq!(cp_conflict.code, 1);
    assert_eq!(cp_conflict.unwrap_json()["error"]["code"], "conflict");
    assert_eq!(
        fs::read_to_string(repo.join("shared.txt")).unwrap(),
        "main1\nline2\nline3\n"
    );

    // Non-conflicting `--no-commit` revert + `oz undo`
    let rev_nc = run_oz(&repo, &["revert", "--no-commit", &main_oid, "--json"], &[]);
    assert_eq!(rev_nc.code, 0, "revert --no-commit stderr: {}", rev_nc.stderr);
    assert_eq!(rev_nc.unwrap_json()["data"]["no_commit"], true);
    assert_eq!(
        fs::read_to_string(repo.join("shared.txt")).unwrap(),
        "line1\nline2\nline3\n"
    );
    assert_eq!(run_oz(&repo, &["undo", "--json"], &[]).code, 0);
    assert_eq!(
        fs::read_to_string(repo.join("shared.txt")).unwrap(),
        "main1\nline2\nline3\n"
    );

    // `oz log --all` + `oz log main..side` + `oz show` (default HEAD)
    let log_range = run_oz(&repo, &["log", "main..side", "--json"], &[]);
    assert_eq!(log_range.code, 0);
    assert_eq!(
        log_range.unwrap_json()["data"]["commits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let log_all = run_oz(&repo, &["log", "--all", "--no-graph", "--json"], &[]);
    assert_eq!(log_all.code, 0);
    assert!(
        log_all.unwrap_json()["data"]["commits"]
            .as_array()
            .unwrap()
            .len()
            >= 3
    );
    let show_head = run_oz(&repo, &["show", "--json"], &[]);
    assert_eq!(show_head.code, 0);
    assert_eq!(show_head.unwrap_json()["data"]["commit_oid"], main_oid);

    // Annotated unsigned tag + pattern listing
    let ann_tag = run_oz(&repo, &["tag", "-a", "v2.0.0", "-m", "Annotated v2", "--json"], &[]);
    assert_eq!(ann_tag.code, 0);
    assert_eq!(ann_tag.unwrap_json()["data"]["kind"], "annotated");
    let tag_filter = run_oz(&repo, &["tag", "-l", "v2.*", "--json"], &[]);
    assert_eq!(tag_filter.unwrap_json()["data"]["count"], 1);

    // 4. PTY test for `oz diff` pager dispatch and live `delta` execution when available
    fs::write(repo.join("shared.txt"), "main1\nline2-edited\nline3\n").unwrap();
    run_pty_diff_and_delta_check(&repo, tmp.path());

    assert_git_fsck_clean(&repo);
}

#[cfg(unix)]
fn run_pty_diff_and_delta_check(repo: &Path, scratch: &Path) {
    use std::io::Read;
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};

    let marker = scratch.join("diff_pager_invoked");
    let spy = scratch.join("diff-spy.sh");
    fs::write(
        &spy,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&spy, fs::Permissions::from_mode(0o755)).unwrap();

    let mut master_fd: libc::c_int = -1;
    let mut slave_fd: libc::c_int = -1;
    let rc = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };

    // `oz diff --json` on TTY must suppress pager
    let slave_json = slave.try_clone().expect("dup");
    let mut cmd_json = Command::new(oz_bin());
    cmd_json
        .args(["diff", "--json"])
        .current_dir(repo)
        .env("GIT_PAGER", &spy)
        .env_remove("OZ_PAGER")
        .env_remove("OZ_AGENT")
        .env_remove("CI")
        .stdin(Stdio::null())
        .stdout(unsafe { Stdio::from_raw_fd(slave_json.into_raw_fd()) })
        .stderr(Stdio::null());
    assert!(cmd_json.status().expect("oz diff --json").success());
    assert!(!marker.exists(), "oz diff --json must suppress pager on TTY");

    // `oz diff` on interactive TTY invokes `$GIT_PAGER`
    let slave_tty = slave.try_clone().expect("dup");
    let mut cmd_diff = Command::new(oz_bin());
    cmd_diff
        .args(["diff"])
        .current_dir(repo)
        .env("GIT_PAGER", &spy)
        .env_remove("OZ_PAGER")
        .env_remove("OZ_AGENT")
        .env_remove("CI")
        .stdin(Stdio::null())
        .stdout(unsafe { Stdio::from_raw_fd(slave_tty.into_raw_fd()) })
        .stderr(Stdio::null());
    assert!(cmd_diff.status().expect("oz diff tty").success());
    assert!(marker.exists(), "oz diff on interactive TTY must invoke pager");

    // If `delta` is installed, run `oz show` with default delta on TTY
    if let Some(delta_bin) = find_delta_binary()
        && let Some(delta_bin_dir) = delta_bin.parent()
    {
        let path_with_delta = format!("{}:/usr/bin:/bin", delta_bin_dir.display());
        let mut cmd_delta = Command::new(oz_bin());
        cmd_delta
            .args(["show"])
            .current_dir(repo)
            .env("PATH", path_with_delta)
            .env("DELTA_PAGER", "cat")
            .env_remove("OZ_PAGER")
            .env_remove("GIT_PAGER")
            .env_remove("OZ_AGENT")
            .env_remove("CI")
            .stdin(Stdio::null())
            .stdout(unsafe { Stdio::from_raw_fd(slave.into_raw_fd()) })
            .stderr(Stdio::null());
        assert!(cmd_delta.status().expect("oz show via delta").success());
    } else {
        drop(slave);
    }

    let mut pty_file = std::fs::File::from(master);
    let mut buf = [0u8; 4096];
    let _ = pty_file.read(&mut buf);
    let _ = &mut pty_file;
}
