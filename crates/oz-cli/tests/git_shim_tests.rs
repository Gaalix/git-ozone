// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end integration tests for the `oz git <args...>` compatibility shim.

use oz_cli::git_shim::resolve_real_git;
use oz_core::{OZ_SCHEMA_V1, OzoneLayout, OzoneRepo, WorkspaceState};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn oz_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oz"))
}

struct CmdOut {
    code: i32,
    stdout: String,
    stderr: String,
    json: Option<Value>,
}

fn run_oz(cwd: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> CmdOut {
    let mut cmd = Command::new(oz_bin());
    cmd.args(args)
        .current_dir(cwd)
        .env_remove("OZ_AGENT")
        .env_remove("CI")
        .stdin(Stdio::null());
    for (k, _) in std::env::vars() {
        if k.starts_with("OZ_AGENT")
            || k.starts_with("CLAUDE")
            || k.starts_with("CURSOR_")
            || k.starts_with("CODEX_")
            || k.starts_with("WINDSURF_")
            || k.starts_with("ANTIGRAVITY_")
            || k.starts_with("AGY_")
        {
            cmd.env_remove(&k);
        }
    }
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
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn init_repo(dir: &Path) -> String {
    fs::create_dir_all(dir).expect("mkdir repo");
    sys_git(dir, &["init", "-b", "main"]);
    sys_git(dir, &["config", "user.name", "Ozone Shim Test"]);
    sys_git(dir, &["config", "user.email", "shim@ozone.dev"]);
    fs::write(dir.join(".ozone.toml"), "backend = \"cas_hardlink\"\n").expect("write .ozone.toml");
    fs::write(dir.join("README.md"), "# shim test\n").expect("write readme");
    sys_git(dir, &["add", "."]);
    sys_git(dir, &["commit", "-m", "init"]);
    sys_git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

#[test]
fn real_git_resolution_skips_oz_alias_and_global_flags_work() {
    let real_git = resolve_real_git().expect("resolve real git");
    assert!(real_git.is_file());

    let tmp = TempDir::new().expect("tempdir");
    let fake_bin = tmp.path().join("fake-bin");
    fs::create_dir_all(&fake_bin).expect("mkdir fake-bin");

    // Place a symlink `git -> oz` ahead of real git in PATH; `oz git --version` must skip it
    // and avoid infinite recursion!
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(oz_bin(), fake_bin.join("git")).expect("symlink git -> oz");
        let orig_path = std::env::var("PATH").unwrap_or_default();
        let poisoned_path = format!("{}:{orig_path}", fake_bin.display());
        let ver = run_oz(
            tmp.path(),
            &["git", "--version"],
            &[("PATH", &poisoned_path)],
        );
        assert_eq!(ver.code, 0, "stderr: {}", ver.stderr);
        assert!(
            ver.stdout.contains("git version"),
            "unexpected git --version output: {}",
            ver.stdout
        );
    }

    // Also verify `-C <dir>`, sequential `-C`, `-c key=val`, and `--no-pager`.
    let repo_dir = tmp.path().join("globals");
    init_repo(&repo_dir);
    fs::create_dir_all(repo_dir.join("sub")).expect("mkdir sub");

    let rev = run_oz(
        tmp.path(),
        &[
            "git",
            "-C",
            repo_dir.to_str().unwrap(),
            "-C",
            "sub",
            "--no-pager",
            "rev-parse",
            "--show-toplevel",
        ],
        &[],
    );
    assert_eq!(rev.code, 0, "stderr: {}", rev.stderr);
    assert_eq!(
        fs::canonicalize(rev.stdout.trim()).unwrap(),
        fs::canonicalize(&repo_dir).unwrap()
    );

    // `-c user.name=...` propagates into native `oz git commit`.
    fs::write(repo_dir.join("README.md"), "# updated\n").unwrap();
    let commit = run_oz(
        tmp.path(),
        &[
            "git",
            "-C",
            repo_dir.to_str().unwrap(),
            "-c",
            "user.name=Custom Bot",
            "-c",
            "user.email=bot@custom.dev",
            "commit",
            "-m",
            "chore: test -c config",
            "--json",
        ],
        &[],
    );
    assert_eq!(commit.code, 0, "stdout={} stderr={}", commit.stdout, commit.stderr);
    let log_author = sys_git(&repo_dir, &["log", "-1", "--pretty=%an <%ae>"]);
    assert_eq!(log_author.trim(), "Custom Bot <bot@custom.dev>");
}

#[test]
fn git_add_status_diff_reset_and_commit_in_main_repo_and_workspace() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("shim-main");
    init_repo(&repo_dir);

    fs::write(repo_dir.join("a.rs"), "pub fn a() -> i32 { 1 }\n").unwrap();
    fs::write(repo_dir.join("b.rs"), "pub fn b() -> i32 { 1 }\n").unwrap();
    fs::write(repo_dir.join("c.rs"), "pub fn c() -> i32 { 1 }\n").unwrap();
    sys_git(&repo_dir, &["add", "."]);
    sys_git(&repo_dir, &["commit", "-m", "seed a b c"]);

    // Modify a.rs and b.rs, create untracked new_a.rs and new_b.rs.
    fs::write(repo_dir.join("a.rs"), "pub fn a() -> i32 { 2 }\n").unwrap();
    fs::write(repo_dir.join("b.rs"), "pub fn b() -> i32 { 2 }\n").unwrap();
    fs::write(repo_dir.join("new_a.rs"), "pub fn na() {}\n").unwrap();
    fs::write(repo_dir.join("new_b.rs"), "pub fn nb() {}\n").unwrap();

    // Non-interactive `oz git commit` without `-m` MUST exit 2 immediately without spawning $EDITOR!
    let no_msg = run_oz(
        &repo_dir,
        &["git", "commit"],
        &[("EDITOR", "sh -c 'exit 99'"), ("GIT_EDITOR", "sh -c 'exit 99'")],
    );
    assert_eq!(no_msg.code, 2, "expected exit 2, got {}", no_msg.code);

    // `oz git add a.rs new_a.rs` stores in `.git/ozone/pinned_main.json`, NOT `.git/ozone/state/`,
    // and never creates `.git/index.lock`.
    let add_out = run_oz(&repo_dir, &["git", "add", "a.rs", "new_a.rs"], &[]);
    assert_eq!(add_out.code, 0, "stderr: {}", add_out.stderr);
    assert!(repo_dir.join(".git/ozone/pinned_main.json").is_file());
    assert!(!repo_dir.join(".git/index.lock").exists());
    let state_files: Vec<_> = fs::read_dir(repo_dir.join(".git/ozone/state"))
        .expect("read state dir")
        .collect();
    assert!(
        state_files.is_empty(),
        "main-repo pinned paths must not reside inside .git/ozone/state/"
    );

    // Verify `oz git status --porcelain` and `-s -b` show staged (`M ` / `A `) vs unstaged (` M` / `??`) columns.
    let porcelain = run_oz(&repo_dir, &["git", "status", "--porcelain"], &[]);
    assert_eq!(porcelain.code, 0);
    assert!(porcelain.stdout.contains("M  a.rs"), "got:\n{}", porcelain.stdout);
    assert!(porcelain.stdout.contains("A  new_a.rs"), "got:\n{}", porcelain.stdout);
    assert!(porcelain.stdout.contains(" M b.rs"), "got:\n{}", porcelain.stdout);
    assert!(porcelain.stdout.contains("?? new_b.rs"), "got:\n{}", porcelain.stdout);

    let short_b = run_oz(&repo_dir, &["git", "status", "-s", "-b"], &[]);
    assert_eq!(short_b.code, 0);
    assert!(short_b.stdout.starts_with("## main\n"), "got:\n{}", short_b.stdout);
    assert!(short_b.stdout.contains("M  a.rs"), "got:\n{}", short_b.stdout);

    // Verify `oz git status --json` emits valid oz.v1 with pinned flags.
    let st_json_run = run_oz(&repo_dir, &["git", "status", "--json"], &[]);
    assert_eq!(st_json_run.code, 0);
    let st_json = st_json_run.json.expect("status json");
    assert_eq!(st_json["schema"], OZ_SCHEMA_V1);
    assert_eq!(st_json["ok"], true);

    // Verify `oz git diff` (working tree), `--cached --name-only`, `--stat`, and commit range `HEAD~1..HEAD`.
    let diff_work = run_oz(&repo_dir, &["git", "diff"], &[]);
    assert_eq!(diff_work.code, 0);
    assert!(diff_work.stdout.contains("diff --ozone"));
    assert!(diff_work.stdout.contains("b.rs"));

    let diff_cached = run_oz(&repo_dir, &["git", "diff", "--cached", "--name-only"], &[]);
    assert_eq!(diff_cached.code, 0);
    assert_eq!(diff_cached.stdout.lines().collect::<Vec<_>>(), vec!["a.rs", "new_a.rs"]);

    let diff_stat = run_oz(&repo_dir, &["git", "diff", "--stat"], &[]);
    assert_eq!(diff_stat.code, 0);
    assert!(diff_stat.stdout.contains("file(s) changed"));

    // Verify commit range `HEAD~1..HEAD` delegates cleanly read-only.
    let diff_range = run_oz(&repo_dir, &["git", "diff", "--name-only", "HEAD~1..HEAD"], &[]);
    assert_eq!(diff_range.code, 0);
    assert!(diff_range.stdout.contains("a.rs"));

    // Unpin `new_a.rs` via `oz git reset HEAD -- new_a.rs`.
    let unpin = run_oz(&repo_dir, &["git", "reset", "HEAD", "--", "new_a.rs"], &[]);
    assert_eq!(unpin.code, 0);
    let porcelain2 = run_oz(&repo_dir, &["git", "status", "--porcelain"], &[]);
    assert!(porcelain2.stdout.contains("?? new_a.rs"));
    assert!(porcelain2.stdout.contains("M  a.rs"));

    // Multi-paragraph `-m` commit commits ONLY pinned `a.rs`, adds Change-Id, clears pinned set!
    let c_pin = run_oz(
        &repo_dir,
        &["git", "commit", "-m", "feat: pin a", "-m", "Second paragraph body.", "--json"],
        &[],
    );
    assert_eq!(c_pin.code, 0, "stdout={} stderr={}", c_pin.stdout, c_pin.stderr);
    let c_pin_json = c_pin.json.expect("commit json");
    assert_eq!(c_pin_json["data"]["committed_paths"], serde_json::json!(["a.rs"]));
    assert_eq!(c_pin_json["data"]["remaining_dirty_count"], 3);
    let msg = c_pin_json["data"]["message"].as_str().unwrap();
    assert!(msg.contains("feat: pin a\n\nSecond paragraph body."));
    assert!(msg.contains("Change-Id: I"));

    // `oz git add -u` pins only tracked modified `b.rs` (not untracked `new_a.rs`/`new_b.rs`).
    let add_u = run_oz(&repo_dir, &["git", "add", "-u"], &[]);
    assert_eq!(add_u.code, 0);
    let porcelain_u = run_oz(&repo_dir, &["git", "status", "--porcelain"], &[]);
    assert!(porcelain_u.stdout.contains("M  b.rs"));
    assert!(porcelain_u.stdout.contains("?? new_a.rs"));

    // `oz git commit -a -F <msgfile>` ignores pinned filter and commits b.rs, new_a.rs, new_b.rs!
    let msg_file = tmp.path().join("msg.txt");
    fs::write(&msg_file, "feat: commit all remaining via -F\n").unwrap();
    let c_all = run_oz(
        &repo_dir,
        &[
            "git",
            "commit",
            "-a",
            "-F",
            msg_file.to_str().unwrap(),
            "--json",
        ],
        &[],
    );
    assert_eq!(c_all.code, 0, "stdout={} stderr={}", c_all.stdout, c_all.stderr);
    let c_all_json = c_all.json.expect("commit -a json");
    assert_eq!(
        c_all_json["data"]["committed_paths"],
        serde_json::json!(["b.rs", "new_a.rs", "new_b.rs"])
    );
    assert_eq!(c_all_json["data"]["remaining_dirty_count"], 0);

    // Now test `oz git worktree add` + `oz git add` + `oz git commit` inside spawned workspace!
    let wt_add = run_oz(
        &repo_dir,
        &["git", "worktree", "add", "-b", "feat/ws-shim", "ws-shim", "main", "--json"],
        &[],
    );
    assert_eq!(wt_add.code, 0, "stdout={} stderr={}", wt_add.stdout, wt_add.stderr);
    let ws_path = PathBuf::from(wt_add.json.unwrap()["data"]["worktree_path"].as_str().unwrap());

    fs::write(ws_path.join("ws_one.rs"), "pub fn one() {}\n").unwrap();
    fs::write(ws_path.join("ws_two.rs"), "pub fn two() {}\n").unwrap();
    let ws_add = run_oz(&ws_path, &["git", "add", "ws_one.rs"], &[]);
    assert_eq!(ws_add.code, 0, "stderr: {}", ws_add.stderr);

    // Verify WorkspaceState.pinned_paths in `.git/ozone/state/ws-shim.json`.
    let layout = OzoneLayout::from_git_common_dir(repo_dir.join(".git"));
    let st: WorkspaceState = serde_json::from_str(
        &fs::read_to_string(layout.workspace_state_path("ws-shim")).unwrap(),
    )
    .unwrap();
    assert!(st.pinned_paths.contains("ws_one.rs"));
    assert!(!st.pinned_paths.contains("ws_two.rs"));

    let ws_commit = run_oz(
        &ws_path,
        &["git", "commit", "-m", "feat(ws): commit pinned ws_one", "--json"],
        &[],
    );
    assert_eq!(ws_commit.code, 0, "stderr: {}", ws_commit.stderr);
    assert_eq!(
        ws_commit.json.unwrap()["data"]["committed_paths"],
        serde_json::json!(["ws_one.rs"])
    );
    let st_after: WorkspaceState = serde_json::from_str(
        &fs::read_to_string(layout.workspace_state_path("ws-shim")).unwrap(),
    )
    .unwrap();
    assert!(st_after.pinned_paths.is_empty());

    // `oz git worktree list --json` & `--porcelain` & `remove` & `prune`.
    let wt_list = run_oz(&repo_dir, &["git", "worktree", "list", "--json"], &[]);
    assert_eq!(wt_list.code, 0);
    assert_eq!(wt_list.json.unwrap()["data"].as_array().unwrap().len(), 2);

    let wt_rm = run_oz(&repo_dir, &["git", "worktree", "remove", "-f", "ws-shim"], &[]);
    assert_eq!(wt_rm.code, 0, "stderr: {}", wt_rm.stderr);
    assert!(!ws_path.exists());

    let wt_prune = run_oz(&repo_dir, &["git", "worktree", "prune", "--json"], &[]);
    assert_eq!(wt_prune.code, 0);
}

#[test]
fn git_switch_checkout_dirty_protection_and_undo() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("switch-repo");
    init_repo(&repo_dir);

    fs::write(repo_dir.join("shared.txt"), "base\n").unwrap();
    fs::write(repo_dir.join("untouched.txt"), "same\n").unwrap();
    sys_git(&repo_dir, &["add", "."]);
    sys_git(&repo_dir, &["commit", "-m", "base"]);

    // Create branch feat/target modifying `shared.txt`.
    let sw_create = run_oz(&repo_dir, &["git", "switch", "-c", "feat/target", "--json"], &[]);
    assert_eq!(sw_create.code, 0, "stdout={} stderr={}", sw_create.stdout, sw_create.stderr);
    assert_eq!(
        OzoneRepo::discover(Some(&repo_dir)).unwrap().branch.as_deref(),
        Some("feat/target")
    );
    fs::write(repo_dir.join("shared.txt"), "from feat\n").unwrap();
    fs::write(repo_dir.join("only_feat.txt"), "feat only\n").unwrap();
    let c_feat = run_oz(&repo_dir, &["git", "commit", "-m", "feat edit"], &[]);
    assert_eq!(c_feat.code, 0);

    // Dirty an unrelated file (`untouched.txt`) and switch back to `main` -> carries over cleanly!
    fs::write(repo_dir.join("untouched.txt"), "carried over\n").unwrap();
    let sw_main = run_oz(&repo_dir, &["git", "switch", "main"], &[]);
    assert_eq!(sw_main.code, 0, "stderr: {}", sw_main.stderr);
    assert_eq!(fs::read_to_string(repo_dir.join("shared.txt")).unwrap(), "base\n");
    assert!(!repo_dir.join("only_feat.txt").exists());
    assert_eq!(
        fs::read_to_string(repo_dir.join("untouched.txt")).unwrap(),
        "carried over\n"
    );

    // Dirty `shared.txt` on `main`, then try switching to `feat/target` -> MUST refuse overwrite!
    fs::write(repo_dir.join("shared.txt"), "dirty conflict\n").unwrap();
    let refused = run_oz(&repo_dir, &["git", "switch", "feat/target"], &[]);
    assert_ne!(refused.code, 0);
    assert_eq!(
        OzoneRepo::discover(Some(&repo_dir)).unwrap().branch.as_deref(),
        Some("main")
    );
    assert_eq!(fs::read_to_string(repo_dir.join("shared.txt")).unwrap(), "dirty conflict\n");

    // Force switch `-f` succeeds and overwrites `shared.txt`, wrapped in OpLog so `oz undo` restores!
    let forced = run_oz(&repo_dir, &["git", "switch", "-f", "feat/target", "--json"], &[]);
    assert_eq!(forced.code, 0, "stdout={} stderr={}", forced.stdout, forced.stderr);
    assert_eq!(fs::read_to_string(repo_dir.join("shared.txt")).unwrap(), "from feat\n");
    assert_eq!(
        OzoneRepo::discover(Some(&repo_dir)).unwrap().branch.as_deref(),
        Some("feat/target")
    );

    let undo_sw = run_oz(&repo_dir, &["undo", "--json"], &[]);
    assert_eq!(undo_sw.code, 0, "stderr: {}", undo_sw.stderr);
    assert_eq!(
        OzoneRepo::discover(Some(&repo_dir)).unwrap().branch.as_deref(),
        Some("main")
    );
    assert_eq!(fs::read_to_string(repo_dir.join("shared.txt")).unwrap(), "dirty conflict\n");

    // `oz git checkout -- shared.txt` restores from HEAD, and `oz git checkout feat/target -- only_feat.txt` restores from feat/target.
    let co_head = run_oz(&repo_dir, &["git", "checkout", "--", "shared.txt"], &[]);
    assert_eq!(co_head.code, 0, "stderr: {}", co_head.stderr);
    assert_eq!(fs::read_to_string(repo_dir.join("shared.txt")).unwrap(), "base\n");

    let co_rev = run_oz(
        &repo_dir,
        &["git", "checkout", "feat/target", "--", "only_feat.txt"],
        &[],
    );
    assert_eq!(co_rev.code, 0, "stderr: {}", co_rev.stderr);
    assert_eq!(fs::read_to_string(repo_dir.join("only_feat.txt")).unwrap(), "feat only\n");

    // `oz git checkout -b feat/co` creates and switches branch.
    let co_b = run_oz(&repo_dir, &["git", "checkout", "-b", "feat/co"], &[]);
    assert_eq!(co_b.code, 0, "stderr: {}", co_b.stderr);
    assert_eq!(
        OzoneRepo::discover(Some(&repo_dir)).unwrap().branch.as_deref(),
        Some("feat/co")
    );
}

#[test]
fn git_reset_hard_head_tilde_1_and_clean_fd_restored_by_oz_undo() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("reset-undo");
    let c1 = init_repo(&repo_dir);

    fs::write(repo_dir.join(".gitignore"), "*.log\n").unwrap();
    fs::write(repo_dir.join("tracked.txt"), "v1\n").unwrap();
    sys_git(&repo_dir, &["add", "."]);
    sys_git(&repo_dir, &["commit", "-m", "commit c1b"]);
    let _c1b = sys_git(&repo_dir, &["rev-parse", "HEAD"]).trim().to_string();

    fs::write(repo_dir.join("tracked.txt"), "v2-committed\n").unwrap();
    fs::write(repo_dir.join("c2_file.txt"), "born in c2\n").unwrap();
    sys_git(&repo_dir, &["add", "."]);
    sys_git(&repo_dir, &["commit", "-m", "commit c2"]);
    let c2 = sys_git(&repo_dir, &["rev-parse", "HEAD"]).trim().to_string();
    assert_ne!(c1, c2);

    // Uncommitted edits: modified tracked file + untracked non-ignored file + nested untracked file + ignored file.
    fs::write(repo_dir.join("tracked.txt"), "precious dirty edit\n").unwrap();
    fs::write(repo_dir.join("untracked.txt"), "precious untracked\n").unwrap();
    fs::create_dir_all(repo_dir.join("scratch")).unwrap();
    fs::write(repo_dir.join("scratch/note.md"), "precious nested\n").unwrap();
    fs::write(repo_dir.join("debug.log"), "ignored stays\n").unwrap();

    // 1. Destructive `oz git reset --hard HEAD~1` rewinds commit AND wipes tracked.txt dirty edit!
    let reset_out = run_oz(&repo_dir, &["git", "reset", "--hard", "HEAD~1"], &[]);
    assert_eq!(reset_out.code, 0, "stderr: {}", reset_out.stderr);
    assert_ne!(sys_git(&repo_dir, &["rev-parse", "HEAD"]).trim(), c2);
    assert!(!repo_dir.join("c2_file.txt").exists());
    assert_eq!(fs::read_to_string(repo_dir.join("tracked.txt")).unwrap(), "v1\n");

    // `oz undo --json` MUST restore BOTH commit C2 AND every wiped dirty/untracked file!
    let undo_reset = run_oz(&repo_dir, &["undo", "--json"], &[]);
    assert_eq!(undo_reset.code, 0, "stdout={} stderr={}", undo_reset.stdout, undo_reset.stderr);
    let undo_json = undo_reset.json.expect("undo json");
    assert_eq!(undo_json["ok"], true);
    assert_eq!(sys_git(&repo_dir, &["rev-parse", "HEAD"]).trim(), c2);
    assert_eq!(fs::read_to_string(repo_dir.join("c2_file.txt")).unwrap(), "born in c2\n");
    assert_eq!(
        fs::read_to_string(repo_dir.join("tracked.txt")).unwrap(),
        "precious dirty edit\n"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("untracked.txt")).unwrap(),
        "precious untracked\n"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("scratch/note.md")).unwrap(),
        "precious nested\n"
    );
    let sys_st = sys_git(&repo_dir, &["status", "--porcelain"]);
    assert!(sys_st.contains(" M tracked.txt"), "git status:\n{sys_st}");
    assert!(sys_st.contains("?? untracked.txt"), "git status:\n{sys_st}");
    assert!(!sys_st.contains("c2_file.txt"), "c2_file.txt should be clean in index:\n{sys_st}");

    // 2. Destructive `oz git clean -fd` wipes untracked files; `oz undo` restores them!
    let clean_out = run_oz(&repo_dir, &["git", "clean", "-fd"], &[]);
    assert_eq!(clean_out.code, 0, "stderr: {}", clean_out.stderr);
    assert!(!repo_dir.join("untracked.txt").exists());
    assert!(!repo_dir.join("scratch/note.md").exists());
    assert!(repo_dir.join("debug.log").is_file());

    let undo_clean = run_oz(&repo_dir, &["undo", "--json"], &[]);
    assert_eq!(undo_clean.code, 0);
    assert_eq!(
        fs::read_to_string(repo_dir.join("untracked.txt")).unwrap(),
        "precious untracked\n"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("scratch/note.md")).unwrap(),
        "precious nested\n"
    );

    // 3. Also test delegated mutating commands (`rm`, `mv`) are undoable via `oz undo`.
    let rm_out = run_oz(&repo_dir, &["git", "rm", "-f", "c2_file.txt"], &[]);
    assert_eq!(rm_out.code, 0, "stderr: {}", rm_out.stderr);
    assert!(!repo_dir.join("c2_file.txt").exists());
    assert_eq!(run_oz(&repo_dir, &["undo"], &[]).code, 0);
    assert_eq!(fs::read_to_string(repo_dir.join("c2_file.txt")).unwrap(), "born in c2\n");

    let mv_out = run_oz(&repo_dir, &["git", "mv", "c2_file.txt", "c2_moved.txt"], &[]);
    assert_eq!(mv_out.code, 0, "stderr: {}", mv_out.stderr);
    assert!(!repo_dir.join("c2_file.txt").exists());
    assert!(repo_dir.join("c2_moved.txt").is_file());
    assert_eq!(run_oz(&repo_dir, &["undo"], &[]).code, 0);
    assert!(repo_dir.join("c2_file.txt").is_file());
    assert!(!repo_dir.join("c2_moved.txt").exists());
}

#[test]
fn git_clone_fetch_push_merge_and_rebase_wrappers() {
    let tmp = TempDir::new().expect("tempdir");
    let bare_remote = tmp.path().join("origin.git");
    fs::create_dir_all(&bare_remote).unwrap();
    sys_git(&bare_remote, &["init", "--bare", "-b", "main"]);

    // Seed bare remote via temporary upstream repo.
    let seed = tmp.path().join("seed");
    init_repo(&seed);
    sys_git(&seed, &["remote", "add", "origin", bare_remote.to_str().unwrap()]);
    sys_git(&seed, &["push", "-u", "origin", "main"]);

    // 1. `oz git clone <url> cloned --json` auto-initializes `.git/ozone/` and emits oz.v1 JSON.
    let clone_out = run_oz(
        tmp.path(),
        &["git", "clone", bare_remote.to_str().unwrap(), "cloned", "--json"],
        &[],
    );
    assert_eq!(clone_out.code, 0, "stdout={} stderr={}", clone_out.stdout, clone_out.stderr);
    let clone_json = clone_out.json.expect("clone json");
    assert_eq!(clone_json["schema"], OZ_SCHEMA_V1);
    assert_eq!(clone_json["ok"], true);
    let cloned_dir = tmp.path().join("cloned");
    assert!(cloned_dir.join(".git/ozone/oplog").is_dir());
    assert!(cloned_dir.join(".git/ozone/state").is_dir());
    sys_git(&cloned_dir, &["config", "user.name", "Cloned Bot"]);
    sys_git(&cloned_dir, &["config", "user.email", "cloned@ozone.dev"]);

    // 2. Push from `seed`, then `oz git fetch origin` in `cloned` -> OpLog records remote ref delta -> `oz undo` rewinds!
    let pre_fetch_remote = sys_git(&cloned_dir, &["rev-parse", "refs/remotes/origin/main"])
        .trim()
        .to_string();
    fs::write(seed.join("upstream.txt"), "from upstream\n").unwrap();
    sys_git(&seed, &["add", "upstream.txt"]);
    sys_git(&seed, &["commit", "-m", "upstream commit"]);
    sys_git(&seed, &["push", "origin", "main"]);
    let new_upstream_oid = sys_git(&seed, &["rev-parse", "HEAD"]).trim().to_string();

    let fetch_out = run_oz(&cloned_dir, &["git", "fetch", "origin", "--json"], &[]);
    assert_eq!(fetch_out.code, 0, "stdout={} stderr={}", fetch_out.stdout, fetch_out.stderr);
    assert_eq!(
        sys_git(&cloned_dir, &["rev-parse", "refs/remotes/origin/main"]).trim(),
        new_upstream_oid
    );

    let undo_fetch = run_oz(&cloned_dir, &["undo"], &[]);
    assert_eq!(undo_fetch.code, 0, "stderr: {}", undo_fetch.stderr);
    assert_eq!(
        sys_git(&cloned_dir, &["rev-parse", "refs/remotes/origin/main"]).trim(),
        pre_fetch_remote
    );

    // 3. `oz git push` wrapped in OpLog.
    fs::write(seed.join("local_only.txt"), "push me\n").unwrap();
    let c_seed = run_oz(&seed, &["git", "commit", "-m", "local push"], &[]);
    assert_eq!(c_seed.code, 0);
    let push_out = run_oz(&seed, &["git", "push", "origin", "main", "--json"], &[]);
    assert_eq!(push_out.code, 0, "stdout={} stderr={}", push_out.stdout, push_out.stderr);

    // 4. `oz git merge` and `oz git rebase` (Wave 1 OpLog wrappers) + `oz undo`.
    let sw_feat = run_oz(&cloned_dir, &["git", "switch", "-c", "feat/m"], &[]);
    assert_eq!(sw_feat.code, 0);
    fs::write(cloned_dir.join("feat_m.txt"), "m\n").unwrap();
    assert_eq!(
        run_oz(&cloned_dir, &["git", "commit", "-m", "feat m"], &[]).code,
        0
    );

    assert_eq!(run_oz(&cloned_dir, &["git", "switch", "main"], &[]).code, 0);
    fs::write(cloned_dir.join("main_m.txt"), "main\n").unwrap();
    assert_eq!(
        run_oz(&cloned_dir, &["git", "commit", "-m", "main m"], &[]).code,
        0
    );
    let main_before_merge = sys_git(&cloned_dir, &["rev-parse", "HEAD"]).trim().to_string();

    let merge_out = run_oz(
        &cloned_dir,
        &["git", "merge", "--no-ff", "feat/m", "-m", "merge feat/m"],
        &[],
    );
    assert_eq!(merge_out.code, 0, "stderr: {}", merge_out.stderr);
    assert!(cloned_dir.join("feat_m.txt").is_file());

    assert_eq!(run_oz(&cloned_dir, &["undo"], &[]).code, 0);
    assert_eq!(
        sys_git(&cloned_dir, &["rev-parse", "HEAD"]).trim(),
        main_before_merge
    );
    assert!(!cloned_dir.join("feat_m.txt").exists());

    // Test `oz git rebase main` from `feat/m` + `oz undo`.
    assert_eq!(run_oz(&cloned_dir, &["git", "switch", "feat/m"], &[]).code, 0);
    let feat_before_rebase = sys_git(&cloned_dir, &["rev-parse", "HEAD"]).trim().to_string();
    let rebase_out = run_oz(&cloned_dir, &["git", "rebase", "main"], &[]);
    assert_eq!(rebase_out.code, 0, "stderr: {}", rebase_out.stderr);
    assert!(cloned_dir.join("main_m.txt").is_file());
    assert_ne!(
        sys_git(&cloned_dir, &["rev-parse", "HEAD"]).trim(),
        feat_before_rebase
    );

    assert_eq!(run_oz(&cloned_dir, &["undo"], &[]).code, 0);
    assert_eq!(
        sys_git(&cloned_dir, &["rev-parse", "HEAD"]).trim(),
        feat_before_rebase
    );
    assert!(!cloned_dir.join("main_m.txt").exists());
}

fn seed_m3_bare_remote(root: &Path) -> (PathBuf, PathBuf, String, String, String) {
    let bare = root.join("m3-origin.git");
    fs::create_dir_all(&bare).unwrap();
    sys_git(&bare, &["init", "--bare", "-b", "main"]);
    sys_git(&bare, &["config", "uploadpack.allowfilter", "true"]);
    sys_git(&bare, &["config", "uploadpack.allowanysha1inwant", "true"]);
    sys_git(&bare, &["config", "receive.advertiseAtomic", "true"]);

    let seed = root.join("m3-seed");
    fs::create_dir_all(&seed).unwrap();
    sys_git(&seed, &["init", "-b", "main"]);
    sys_git(&seed, &["config", "user.name", "M3 Seed"]);
    sys_git(&seed, &["config", "user.email", "seed@ozone.dev"]);

    // Commit 1 adds a historical blob that Commit 2 deletes, so `--filter=blob:none` at HEAD
    // never downloads `legacy_blob.bin` into `.git/objects`!
    fs::write(
        seed.join("legacy_blob.bin"),
        "HISTORICAL_BLOB_PAYLOAD_".repeat(256),
    )
    .unwrap();
    sys_git(&seed, &["add", "legacy_blob.bin"]);
    let legacy_blob_oid = sys_git(&seed, &["rev-parse", ":legacy_blob.bin"])
        .trim()
        .to_string();
    sys_git(&seed, &["commit", "-m", "chore: add legacy blob"]);

    fs::remove_file(seed.join("legacy_blob.bin")).unwrap();
    fs::create_dir_all(seed.join("src")).unwrap();
    fs::write(seed.join("README.md"), "# M3 Shim Wire v2\n").unwrap();
    fs::write(seed.join("src/lib.rs"), "pub fn version() -> u32 { 3 }\n").unwrap();
    sys_git(&seed, &["add", "-A"]);
    sys_git(&seed, &["commit", "-m", "feat: active main tree"]);
    let main_tip = sys_git(&seed, &["rev-parse", "HEAD"]).trim().to_string();

    sys_git(&seed, &["checkout", "-b", "feat/alpha"]);
    fs::write(seed.join("alpha.txt"), "alpha branch\n").unwrap();
    sys_git(&seed, &["add", "alpha.txt"]);
    sys_git(&seed, &["commit", "-m", "feat: alpha"]);
    let alpha_tip = sys_git(&seed, &["rev-parse", "HEAD"]).trim().to_string();
    sys_git(&seed, &["checkout", "main"]);

    sys_git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
    sys_git(&seed, &["push", "origin", "main:main", "feat/alpha:feat/alpha"]);

    (bare, seed, legacy_blob_oid, main_tip, alpha_tip)
}

#[test]
fn m3_git_clone_partial_blob_none_and_exotic_fallbacks() {
    let tmp = TempDir::new().expect("tempdir");
    let (bare_remote, _seed, legacy_blob_oid, main_tip, alpha_tip) =
        seed_m3_bare_remote(tmp.path());

    // 1. Native `oz git clone --filter=blob:none <remote> <dest> --json`
    let clone_out = run_oz(
        tmp.path(),
        &[
            "git",
            "clone",
            "--filter=blob:none",
            "-o",
            "origin",
            bare_remote.to_str().unwrap(),
            "partial-dest",
            "--json",
        ],
        &[],
    );
    assert_eq!(
        clone_out.code, 0,
        "stdout={} stderr={}",
        clone_out.stdout, clone_out.stderr
    );
    let cj = clone_out.json.expect("clone json");
    assert_eq!(cj["schema"], OZ_SCHEMA_V1);
    assert_eq!(cj["ok"], true);
    assert_eq!(cj["data"]["protocol_version"], 2);
    assert_eq!(cj["data"]["partial_clone"], true);
    assert_eq!(cj["data"]["filter"], "blob:none");
    assert_eq!(cj["data"]["filter_negotiated"], true);
    assert_eq!(cj["data"]["branch"], "main");
    assert_eq!(cj["data"]["head_oid"], main_tip);
    assert!(
        cj["op_id"]
            .as_str()
            .is_some_and(|s| s.starts_with("op_")),
        "expected initial OpLog op_id in {cj}"
    );

    let partial_dir = tmp.path().join("partial-dest");
    assert!(partial_dir.join(".git/ozone/oplog/HEAD").is_file());
    assert!(partial_dir.join(".git/ozone/state").is_dir());
    assert_eq!(
        fs::read_to_string(partial_dir.join("src/lib.rs")).unwrap(),
        "pub fn version() -> u32 { 3 }\n"
    );

    let git_cfg = fs::read_to_string(partial_dir.join(".git/config")).unwrap();
    assert!(git_cfg.contains("promisor = true"), "config:\n{git_cfg}");
    assert!(
        git_cfg.contains("partialclonefilter = blob:none"),
        "config:\n{git_cfg}"
    );

    let pack_files: Vec<String> = fs::read_dir(partial_dir.join(".git/objects/pack"))
        .unwrap()
        .filter_map(|e| e.ok().and_then(|d| d.file_name().into_string().ok()))
        .collect();
    assert!(
        pack_files.iter().any(|n| n.ends_with(".promisor")),
        "expected .promisor marker in {pack_files:?}"
    );

    // Verify historical `legacy_blob_oid` from c1 was NOT downloaded into local `.git/objects`!
    let has_legacy = Command::new(sys_git_bin())
        .args(["--no-lazy-fetch", "cat-file", "-e", &legacy_blob_oid])
        .current_dir(&partial_dir)
        .output()
        .expect("git cat-file -e");
    assert!(
        !has_legacy.status.success(),
        "partial clone blob:none should not fetch historical unreachable-from-HEAD-tree blob"
    );

    // 2. Native `-b feat/alpha --filter blob:none -q`
    let branch_clone = run_oz(
        tmp.path(),
        &[
            "git",
            "clone",
            "-q",
            "-b",
            "feat/alpha",
            "--filter",
            "blob:none",
            bare_remote.to_str().unwrap(),
            "alpha-dest",
            "--json",
        ],
        &[],
    );
    assert_eq!(branch_clone.code, 0, "stderr={}", branch_clone.stderr);
    let aj = branch_clone.json.expect("alpha clone json");
    assert_eq!(aj["data"]["branch"], "feat/alpha");
    assert_eq!(aj["data"]["head_oid"], alpha_tip);
    assert!(tmp.path().join("alpha-dest/alpha.txt").is_file());

    // 3. Exotic clone flags (`--depth 1` and `--bare`) fall back cleanly to system `git clone`,
    // initializing `.git/ozone/` only when non-bare.
    let file_url = format!("file://{}", bare_remote.display());
    let shallow_clone = run_oz(
        tmp.path(),
        &[
            "git",
            "clone",
            "--depth",
            "1",
            &file_url,
            "shallow-dest",
            "--json",
        ],
        &[],
    );
    assert_eq!(shallow_clone.code, 0, "stderr={}", shallow_clone.stderr);
    assert!(tmp.path().join("shallow-dest/.git/ozone/oplog").is_dir());
    assert!(tmp.path().join("shallow-dest/.git/shallow").is_file());

    let bare_clone = run_oz(
        tmp.path(),
        &["git", "clone", "--bare", &file_url, "bare-copy.git", "--json"],
        &[],
    );
    assert_eq!(bare_clone.code, 0, "stderr={}", bare_clone.stderr);
    assert!(tmp.path().join("bare-copy.git/HEAD").is_file());
    assert!(
        !tmp.path().join("bare-copy.git/ozone").exists(),
        "bare fallback clone must not initialize colocated .git/ozone/"
    );
}

#[test]
fn m3_git_fetch_prune_and_push_atomic_and_force_with_lease() {
    let tmp = TempDir::new().expect("tempdir");
    let (bare_remote, seed, _legacy_blob_oid, main_tip, alpha_tip) =
        seed_m3_bare_remote(tmp.path());

    let clone_out = run_oz(
        tmp.path(),
        &[
            "git",
            "clone",
            bare_remote.to_str().unwrap(),
            "work",
            "--json",
        ],
        &[],
    );
    assert_eq!(clone_out.code, 0, "stderr={}", clone_out.stderr);
    let work_dir = tmp.path().join("work");
    sys_git(&work_dir, &["config", "user.name", "M3 Worker"]);
    sys_git(&work_dir, &["config", "user.email", "worker@ozone.dev"]);

    assert_eq!(
        sys_git(&work_dir, &["rev-parse", "refs/remotes/origin/feat/alpha"]).trim(),
        alpha_tip
    );

    // Advance `main` on `seed` -> push to `bare_remote`, AND delete `feat/alpha` on `bare_remote`.
    fs::write(seed.join("upstream_v2.txt"), "v2\n").unwrap();
    sys_git(&seed, &["add", "upstream_v2.txt"]);
    sys_git(&seed, &["commit", "-m", "feat: upstream v2"]);
    sys_git(&seed, &["push", "origin", "main"]);
    let new_main_tip = sys_git(&seed, &["rev-parse", "HEAD"]).trim().to_string();
    sys_git(&bare_remote, &["branch", "-D", "feat/alpha"]);

    // 1. `oz git fetch --prune` updates `refs/remotes/origin/main` AND prunes `refs/remotes/origin/feat/alpha` natively!
    let fetch_out = run_oz(&work_dir, &["git", "fetch", "--prune", "--json"], &[]);
    assert_eq!(
        fetch_out.code, 0,
        "stdout={} stderr={}",
        fetch_out.stdout, fetch_out.stderr
    );
    let fj = fetch_out.json.expect("fetch json");
    assert_eq!(fj["schema"], OZ_SCHEMA_V1);
    assert_eq!(fj["ok"], true);
    assert_eq!(fj["data"]["protocol_version"], 2);
    assert_eq!(
        fj["data"]["pruned_refs"],
        serde_json::json!(["refs/remotes/origin/feat/alpha"])
    );
    assert!(fj["op_id"].as_str().is_some());
    assert_eq!(
        sys_git(&work_dir, &["rev-parse", "refs/remotes/origin/main"]).trim(),
        new_main_tip
    );
    let check_alpha = Command::new(sys_git_bin())
        .args(["rev-parse", "--verify", "refs/remotes/origin/feat/alpha"])
        .current_dir(&work_dir)
        .output()
        .unwrap();
    assert!(!check_alpha.status.success(), "feat/alpha should be pruned");

    // `oz undo` reverses BOTH the updated `origin/main` ref AND the pruned `origin/feat/alpha` ref!
    let undo_fetch = run_oz(&work_dir, &["undo", "--json"], &[]);
    assert_eq!(undo_fetch.code, 0, "stderr={}", undo_fetch.stderr);
    assert_eq!(
        sys_git(&work_dir, &["rev-parse", "refs/remotes/origin/main"]).trim(),
        main_tip
    );
    assert_eq!(
        sys_git(&work_dir, &["rev-parse", "refs/remotes/origin/feat/alpha"]).trim(),
        alpha_tip
    );

    // Re-fetch `-p -q` so remote-tracking refs are up to date again.
    assert_eq!(run_oz(&work_dir, &["git", "fetch", "-p", "-q"], &[]).code, 0);

    // 2. `oz git push -u origin feat/stack --atomic --json`
    assert_eq!(
        run_oz(&work_dir, &["git", "switch", "-c", "feat/stack"], &[]).code,
        0
    );
    fs::write(work_dir.join("stack.rs"), "pub fn s() -> i32 { 1 }\n").unwrap();
    assert_eq!(
        run_oz(&work_dir, &["git", "commit", "-m", "feat: stack v1"], &[]).code,
        0
    );
    let stack_v1 = sys_git(&work_dir, &["rev-parse", "HEAD"]).trim().to_string();

    let push_atomic = run_oz(
        &work_dir,
        &["git", "push", "-u", "origin", "feat/stack", "--atomic", "--json"],
        &[],
    );
    assert_eq!(
        push_atomic.code, 0,
        "stdout={} stderr={}",
        push_atomic.stdout, push_atomic.stderr
    );
    let pj = push_atomic.json.expect("push json");
    assert_eq!(pj["schema"], OZ_SCHEMA_V1);
    assert_eq!(pj["ok"], true);
    assert_eq!(pj["data"]["atomic"], true);
    assert_eq!(
        pj["data"]["upstream_configured"],
        serde_json::json!(["feat/stack"])
    );
    assert_eq!(
        sys_git(&bare_remote, &["rev-parse", "refs/heads/feat/stack"]).trim(),
        stack_v1
    );
    assert_eq!(
        sys_git(&work_dir, &["config", "--get", "branch.feat/stack.remote"]).trim(),
        "origin"
    );

    // 3. Amend locally -> non-ff push without lease fails (exit 1), `--force-with-lease` succeeds!
    fs::write(work_dir.join("stack.rs"), "pub fn s() -> i32 { 2 }\n").unwrap();
    assert_eq!(
        run_oz(
            &work_dir,
            &["git", "commit", "--amend", "-m", "feat: stack v2"],
            &[]
        )
        .code,
        0
    );
    let stack_v2 = sys_git(&work_dir, &["rev-parse", "HEAD"]).trim().to_string();
    assert_ne!(stack_v1, stack_v2);

    let rejected_non_ff = run_oz(&work_dir, &["git", "push", "--json"], &[]);
    assert_eq!(
        rejected_non_ff.code, 1,
        "expected exit 1 on non-ff push, got {}: {}",
        rejected_non_ff.code, rejected_non_ff.stdout
    );
    let rj = rejected_non_ff.json.expect("rejected push json");
    assert_eq!(rj["schema"], OZ_SCHEMA_V1);
    assert_eq!(rj["ok"], false);
    assert_eq!(rj["error"]["exit_code"], 1);

    let lease_ok = run_oz(
        &work_dir,
        &["git", "push", "--force-with-lease", "--json"],
        &[],
    );
    assert_eq!(
        lease_ok.code, 0,
        "stdout={} stderr={}",
        lease_ok.stdout, lease_ok.stderr
    );
    let lj = lease_ok.json.expect("lease push json");
    assert_eq!(lj["ok"], true);
    assert_eq!(lj["data"]["pushed_refs"][0]["forced"], true);
    assert_eq!(
        sys_git(&bare_remote, &["rev-parse", "refs/heads/feat/stack"]).trim(),
        stack_v2
    );

    // 4. Simulate concurrent teammate updating `feat/stack` on `bare_remote` behind `work_dir`'s back,
    // then verify `oz git push --force-with-lease --json` rejects with ExitCode::ConflictOrCheckFail (1)!
    sys_git(&seed, &["fetch", "origin", "feat/stack:feat/stack"]);
    sys_git(&seed, &["checkout", "feat/stack"]);
    fs::write(seed.join("teammate.rs"), "pub fn teammate() {}\n").unwrap();
    sys_git(&seed, &["add", "teammate.rs"]);
    sys_git(&seed, &["commit", "-m", "feat: teammate concurrent push"]);
    sys_git(&seed, &["push", "origin", "feat/stack"]);
    let teammate_oid = sys_git(&seed, &["rev-parse", "HEAD"]).trim().to_string();

    fs::write(work_dir.join("stack.rs"), "pub fn s() -> i32 { 3 }\n").unwrap();
    assert_eq!(
        run_oz(&work_dir, &["git", "commit", "--amend", "--no-edit"], &[]).code,
        0
    );

    let stale_lease = run_oz(
        &work_dir,
        &["git", "push", "--force-with-lease", "--json"],
        &[],
    );
    assert_eq!(
        stale_lease.code, 1,
        "expected exit 1 on stale lease, got {}: {}",
        stale_lease.code, stale_lease.stdout
    );
    let sj = stale_lease.json.expect("stale lease json");
    assert_eq!(sj["schema"], OZ_SCHEMA_V1);
    assert_eq!(sj["ok"], false);
    assert_eq!(sj["error"]["exit_code"], 1);
    assert_eq!(sj["conflicts"][0]["id"], "stale-lease");
    assert_eq!(
        sys_git(&bare_remote, &["rev-parse", "refs/heads/feat/stack"]).trim(),
        teammate_oid,
        "remote ref must remain untouched after stale-lease rejection"
    );

    // Explicit `--force-with-lease=feat/stack:<teammate_oid>` succeeds once caller supplies expected OID!
    let explicit_lease_arg = format!("--force-with-lease=feat/stack:{teammate_oid}");
    let explicit_lease_ok = run_oz(
        &work_dir,
        &["git", "push", "origin", "feat/stack", &explicit_lease_arg, "--json"],
        &[],
    );
    assert_eq!(
        explicit_lease_ok.code, 0,
        "stdout={} stderr={}",
        explicit_lease_ok.stdout, explicit_lease_ok.stderr
    );
}

#[test]
fn m3_git_commit_ssh_signing_and_native_amend_change_id() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("sign-amend-repo");
    init_repo(&repo_dir);

    // Generate an unencrypted Ed25519 SSH signing keypair via `ssh-keygen`.
    let key_dir = tmp.path().join("keys");
    fs::create_dir_all(&key_dir).unwrap();
    let priv_key = key_dir.join("id_ed25519");
    let pub_key = key_dir.join("id_ed25519.pub");
    let keygen = Command::new(ssh_keygen_bin())
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "shim@ozone.dev",
            "-f",
            priv_key.to_str().unwrap(),
        ])
        .output()
        .expect("run ssh-keygen");
    assert!(keygen.status.success(), "ssh-keygen failed");

    let pub_line = fs::read_to_string(&pub_key).unwrap();
    let allowed_signers = key_dir.join("allowed_signers");
    fs::write(&allowed_signers, format!("shim@ozone.dev {}", pub_line)).unwrap();

    sys_git(&repo_dir, &["config", "gpg.format", "ssh"]);
    sys_git(
        &repo_dir,
        &["config", "user.signingkey", pub_key.to_str().unwrap()],
    );
    sys_git(
        &repo_dir,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            allowed_signers.to_str().unwrap(),
        ],
    );

    // 1. `oz git commit -S -m "..."` signs natively and passes `/usr/bin/git verify-commit`!
    fs::write(repo_dir.join("signed.rs"), "pub fn verified() -> bool { true }\n").unwrap();
    let signed_out = run_oz(
        &repo_dir,
        &["git", "commit", "-S", "-m", "feat: ssh signed commit", "--json"],
        &[],
    );
    assert_eq!(
        signed_out.code, 0,
        "stdout={} stderr={}",
        signed_out.stdout, signed_out.stderr
    );
    assert!(!repo_dir.join(".git/index.lock").exists());
    let signed_json = signed_out.json.expect("signed commit json");
    assert_eq!(signed_json["data"]["signed"], true);
    assert!(
        signed_json["data"]["signature_fingerprint"]
            .as_str()
            .is_some_and(|fp| fp.starts_with("SHA256:"))
    );
    let orig_change_id = signed_json["data"]["change_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(orig_change_id.starts_with('I'));

    let verify_initial = Command::new(sys_git_bin())
        .args(["verify-commit", "HEAD"])
        .current_dir(&repo_dir)
        .output()
        .expect("git verify-commit");
    assert!(
        verify_initial.status.success(),
        "git verify-commit failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&verify_initial.stdout),
        String::from_utf8_lossy(&verify_initial.stderr)
    );

    // 2. `oz git add` + `oz git commit --amend --no-edit` honors virtual `pinned_paths`,
    // preserves `HEAD`'s message + `Change-Id:`, and never touches `.git/index.lock`!
    fs::write(repo_dir.join("pinned_amend.rs"), "pub fn pinned() {}\n").unwrap();
    fs::write(repo_dir.join("unpinned_dirty.rs"), "pub fn keep_dirty() {}\n").unwrap();
    assert_eq!(
        run_oz(&repo_dir, &["git", "add", "pinned_amend.rs"], &[]).code,
        0
    );

    let amend_no_edit = run_oz(
        &repo_dir,
        &["git", "commit", "--amend", "--no-edit", "--json"],
        &[],
    );
    assert_eq!(
        amend_no_edit.code, 0,
        "stdout={} stderr={}",
        amend_no_edit.stdout, amend_no_edit.stderr
    );
    assert!(!repo_dir.join(".git/index.lock").exists());
    let ane_json = amend_no_edit.json.expect("amend --no-edit json");
    assert_eq!(ane_json["data"]["amended"], true);
    assert_eq!(ane_json["data"]["change_id"], orig_change_id);
    assert_eq!(
        ane_json["data"]["committed_paths"],
        serde_json::json!(["pinned_amend.rs"])
    );
    assert_eq!(ane_json["data"]["remaining_dirty_count"], 1);
    assert!(
        ane_json["data"]["message"]
            .as_str()
            .unwrap()
            .starts_with("feat: ssh signed commit")
    );

    // 3. `oz git commit --gpg-sign=<key> --amend -m "..."` folds remaining dirty file, rewords,
    // signs, AND still preserves `HEAD`'s `Change-Id:` trailer!
    let gpg_sign_arg = format!("--gpg-sign={}", priv_key.display());
    let amend_reword = run_oz(
        &repo_dir,
        &[
            "git",
            "commit",
            &gpg_sign_arg,
            "--amend",
            "-m",
            "feat: amended and re-signed commit",
            "--json",
        ],
        &[],
    );
    assert_eq!(
        amend_reword.code, 0,
        "stdout={} stderr={}",
        amend_reword.stdout, amend_reword.stderr
    );
    assert!(!repo_dir.join(".git/index.lock").exists());
    let ar_json = amend_reword.json.expect("amend -m json");
    assert_eq!(ar_json["data"]["amended"], true);
    assert_eq!(ar_json["data"]["signed"], true);
    assert_eq!(ar_json["data"]["change_id"], orig_change_id);
    assert_eq!(ar_json["data"]["remaining_dirty_count"], 0);

    let head_body = sys_git(&repo_dir, &["log", "-1", "--pretty=%B"]);
    assert!(head_body.contains("feat: amended and re-signed commit"));
    assert!(head_body.contains(&format!("Change-Id: {orig_change_id}")));

    let verify_amended = Command::new(sys_git_bin())
        .args(["verify-commit", "HEAD"])
        .current_dir(&repo_dir)
        .output()
        .expect("git verify-commit amended");
    assert!(
        verify_amended.status.success(),
        "git verify-commit failed after amend:\nstderr: {}",
        String::from_utf8_lossy(&verify_amended.stderr)
    );

    // 4. Verify `--no-gpg-sign` overrides `commit.gpgsign = true`.
    sys_git(&repo_dir, &["config", "commit.gpgsign", "true"]);
    fs::write(repo_dir.join("unsigned.rs"), "pub fn u() {}\n").unwrap();
    let unsigned_out = run_oz(
        &repo_dir,
        &[
            "git",
            "commit",
            "--no-gpg-sign",
            "-m",
            "chore: explicit unsigned",
            "--json",
        ],
        &[],
    );
    assert_eq!(unsigned_out.code, 0, "stderr={}", unsigned_out.stderr);
    assert_eq!(unsigned_out.json.unwrap()["data"]["signed"], false);
}

#[test]
fn m3_git_commit_agent_provenance_notes() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("agent-notes-repo");
    init_repo(&repo_dir);

    // 1. Human commit (`oz git commit` without `--agent` or agent env vars) does NOT write `refs/notes/ai`.
    fs::write(repo_dir.join("human.rs"), "pub fn human() -> i32 { 1 }\n").unwrap();
    let human_commit = run_oz(
        &repo_dir,
        &["git", "commit", "-m", "feat: human authored", "--json"],
        &[],
    );
    assert_eq!(human_commit.code, 0, "stderr={}", human_commit.stderr);
    assert!(human_commit.json.unwrap()["data"].get("provenance").is_none());
    let no_note = Command::new(sys_git_bin())
        .args(["notes", "--ref=ai", "show", "HEAD"])
        .current_dir(&repo_dir)
        .output()
        .unwrap();
    assert!(!no_note.status.success(), "human commit should not create refs/notes/ai");

    // 2. `oz --agent git commit -m "..."` writes `refs/notes/ai` readable by `git notes --ref=ai show HEAD`!
    fs::write(
        repo_dir.join("agent_mod.rs"),
        "pub fn generated() -> &'static str {\n    \"hello from agent\"\n}\n",
    )
    .unwrap();
    let agent_commit = run_oz(
        &repo_dir,
        &[
            "--agent",
            "git",
            "commit",
            "-m",
            "feat: agent generated module",
            "--json",
        ],
        &[
            ("OZ_AGENT_SESSION", "sess-m3-shim-01"),
            ("OZ_AGENT_MODEL", "claude-opus-4-6"),
            ("OZ_AGENT_PROMPT", "add generated() helper"),
        ],
    );
    assert_eq!(
        agent_commit.code, 0,
        "stdout={} stderr={}",
        agent_commit.stdout, agent_commit.stderr
    );
    let ac_json = agent_commit.json.expect("agent commit json");
    assert_eq!(ac_json["data"]["provenance"]["session_id"], "sess-m3-shim-01");
    assert_eq!(ac_json["data"]["provenance"]["model"], "claude-opus-4-6");

    let note_raw = sys_git(&repo_dir, &["notes", "--ref=ai", "show", "HEAD"]);
    let note_json: Value = serde_json::from_str(&note_raw).expect("valid JSON in refs/notes/ai");
    assert_eq!(note_json["schema_version"], 1);
    assert_eq!(note_json["session_id"], "sess-m3-shim-01");
    assert_eq!(note_json["model"], "claude-opus-4-6");
    assert_eq!(note_json["prompt_summary"], "add generated() helper");
    assert_eq!(note_json["line_ranges"][0]["path"], "agent_mod.rs");
    assert_eq!(note_json["line_ranges"][0]["start_line"], 1);
    assert_eq!(note_json["line_ranges"][0]["end_line"], 3);
    assert_eq!(note_json["line_ranges"][0]["author_kind"], "agent");

    // Human commit message stays clean without AI metadata bloat.
    let commit_msg = sys_git(&repo_dir, &["log", "-1", "--pretty=%B"]);
    assert!(!commit_msg.contains("sess-m3-shim-01"));

    // 3. Ambient agent harness env vars (without `--agent` flag) ALSO auto-record `refs/notes/ai`!
    fs::write(repo_dir.join("env_agent.rs"), "pub const X: usize = 7;\n").unwrap();
    let env_commit = run_oz(
        &repo_dir,
        &["git", "commit", "-m", "feat: env detected agent commit", "--json"],
        &[
            ("CLAUDE_CODE_SESSION_ID", "claude-auto-77"),
            ("ANTHROPIC_MODEL", "claude-sonnet-4-6"),
        ],
    );
    assert_eq!(env_commit.code, 0, "stderr={}", env_commit.stderr);
    let env_note_raw = sys_git(&repo_dir, &["notes", "--ref=ai", "show", "HEAD"]);
    let env_note: Value = serde_json::from_str(&env_note_raw).expect("valid env note JSON");
    assert_eq!(env_note["client"], "claude-code");
    assert_eq!(env_note["session_id"], "claude-auto-77");
    assert_eq!(env_note["model"], "claude-sonnet-4-6");
    assert_eq!(env_note["line_ranges"][0]["path"], "env_agent.rs");
}

#[test]
fn ide_tooling_flags_vscode_jetbrains_lazygit_and_gh() {
    use std::io::Write;

    let tmp = TempDir::new().unwrap();
    let sub_src = tmp.path().join("sub");
    fs::create_dir_all(&sub_src).unwrap();
    sys_git(&sub_src, &["init", "-b", "main"]);
    fs::write(sub_src.join("s.txt"), "sub1\n").unwrap();
    sys_git(&sub_src, &["add", "."]);
    sys_git(&sub_src, &["commit", "-m", "s1"]);
    fs::write(sub_src.join("s.txt"), "sub2\n").unwrap();
    sys_git(&sub_src, &["add", "."]);
    sys_git(&sub_src, &["commit", "-m", "s2"]);
    let sub_v2 = sys_git(&sub_src, &["rev-parse", "HEAD"]).trim().to_string();
    sys_git(&sub_src, &["checkout", "-q", "HEAD~1"]);
    let sub_v1 = sys_git(&sub_src, &["rev-parse", "HEAD"]).trim().to_string();

    let repo_dir = tmp.path().join("repo");
    let head_oid = init_repo(&repo_dir);
    fs::write(repo_dir.join(".gitignore"), "*.log\n").unwrap();
    fs::write(repo_dir.join("tracked.txt"), "line1\n").unwrap();
    sys_git(&repo_dir, &["add", ".gitignore", "tracked.txt"]);
    sys_git(
        &repo_dir,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub_src.to_str().unwrap(),
            "deps/sub",
        ],
    );
    sys_git(&repo_dir, &["commit", "-m", "add tracked + submodule"]);
    let head_oid2 = sys_git(&repo_dir, &["rev-parse", "HEAD"]).trim().to_string();
    assert_ne!(head_oid, head_oid2);

    // Dirty tracked file, staged file via `oz git add`, untracked file, ignored file, and bumped submodule
    fs::write(repo_dir.join("tracked.txt"), "line1\nline2\n").unwrap();
    fs::write(repo_dir.join("staged.txt"), "staged\n").unwrap();
    assert_eq!(run_oz(&repo_dir, &["git", "add", "staged.txt"], &[]).code, 0);
    fs::write(repo_dir.join("untracked.txt"), "untracked\n").unwrap();
    fs::write(repo_dir.join("debug.log"), "ignored\n").unwrap();
    sys_git(&repo_dir, &["-C", "deps/sub", "checkout", "-q", &sub_v2]);

    // 1. VS Code / JetBrains / lazygit `status --porcelain=v2 --branch -z --no-optional-locks`
    let st_v2 = run_oz(
        &repo_dir,
        &[
            "git",
            "--no-optional-locks",
            "-c",
            "core.quotepath=false",
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
        ],
        &[],
    );
    assert_eq!(st_v2.code, 0, "stderr={}", st_v2.stderr);
    let recs: Vec<&str> = st_v2.stdout.split('\0').filter(|s| !s.is_empty()).collect();
    assert!(
        recs.contains(&format!("# branch.oid {head_oid2}").as_str()),
        "recs={recs:?}"
    );
    assert!(recs.contains(&"# branch.head main"), "recs={recs:?}");
    assert!(
        recs.iter()
            .any(|r| r.starts_with("1 .M N...") && r.ends_with(" tracked.txt")),
        "recs={recs:?}"
    );
    assert!(
        recs.iter()
            .any(|r| r.starts_with("1 .M S... 160000 160000 160000") && r.ends_with(" deps/sub")),
        "recs={recs:?}"
    );
    assert!(recs.contains(&"1 A. N... 000000 100644 100644 0000000000000000000000000000000000000000 0000000000000000000000000000000000000000 staged.txt"), "recs={recs:?}");
    assert!(recs.contains(&"? untracked.txt"), "recs={recs:?}");
    assert!(!recs.iter().any(|r| r.contains("debug.log")), "recs={recs:?}");

    // 2. `--porcelain --null` (v1 NUL-terminated)
    let st_v1z = run_oz(&repo_dir, &["git", "status", "--porcelain", "--null"], &[]);
    assert_eq!(st_v1z.code, 0);
    let v1_recs: Vec<&str> = st_v1z.stdout.split('\0').filter(|s| !s.is_empty()).collect();
    assert!(v1_recs.contains(&" M tracked.txt"), "{v1_recs:?}");
    assert!(v1_recs.contains(&"A  staged.txt"), "{v1_recs:?}");
    assert!(v1_recs.contains(&"?? untracked.txt"), "{v1_recs:?}");

    // 3. `oz git diff --numstat -z`, `--shortstat`, `--quiet`, `--ignore-submodules=all`
    let diff_num = run_oz(
        &repo_dir,
        &["git", "diff", "--numstat", "-z", "--ignore-submodules=all"],
        &[],
    );
    assert_eq!(diff_num.code, 0);
    let d_recs: Vec<&str> = diff_num.stdout.split('\0').filter(|s| !s.is_empty()).collect();
    assert!(d_recs.contains(&"1\t0\ttracked.txt"), "{d_recs:?}");
    assert!(!d_recs.iter().any(|r| r.contains("deps/sub")), "{d_recs:?}");

    let diff_short = run_oz(&repo_dir, &["git", "diff", "--shortstat"], &[]);
    assert_eq!(diff_short.code, 0);
    assert!(diff_short.stdout.contains("changed,"), "{}", diff_short.stdout);

    let diff_quiet = run_oz(&repo_dir, &["git", "diff", "--quiet"], &[]);
    assert_eq!(diff_quiet.code, 1);
    assert!(diff_quiet.stdout.is_empty());

    // 4. `rev-parse`, `ls-files -z`, `check-ignore` (exit codes + piped `--stdin -z`)
    let rp_top = run_oz(
        &repo_dir,
        &["git", "--no-optional-locks", "rev-parse", "--show-toplevel"],
        &[],
    );
    assert_eq!(rp_top.code, 0);
    assert_eq!(rp_top.stdout.trim(), repo_dir.to_string_lossy());

    let rp_bad = run_oz(
        &repo_dir,
        &["git", "rev-parse", "--verify", "refs/heads/does-not-exist"],
        &[],
    );
    assert_eq!(rp_bad.code, 2);

    let ls_z = run_oz(&repo_dir, &["git", "ls-files", "-z"], &[]);
    assert_eq!(ls_z.code, 0);
    let ls_items: Vec<&str> = ls_z.stdout.split('\0').filter(|s| !s.is_empty()).collect();
    assert!(ls_items.contains(&"tracked.txt"), "{ls_items:?}");
    assert!(ls_items.contains(&"deps/sub"), "{ls_items:?}");

    let ci_hit = run_oz(&repo_dir, &["git", "check-ignore", "debug.log"], &[]);
    assert_eq!(ci_hit.code, 0);
    assert_eq!(ci_hit.stdout.trim(), "debug.log");

    let ci_miss = run_oz(&repo_dir, &["git", "check-ignore", "tracked.txt"], &[]);
    assert_eq!(ci_miss.code, 1);

    let ci_miss_json = run_oz(
        &repo_dir,
        &["git", "--json", "check-ignore", "tracked.txt"],
        &[],
    );
    assert_eq!(ci_miss_json.code, 1);
    assert_eq!(
        ci_miss_json.json.unwrap()["error"]["code"],
        "check_failed"
    );

    // Piped stdin to `oz git check-ignore -z --stdin` MUST pass stdin cleanly even when non-TTY
    let mut child = Command::new(oz_bin())
        .args(["git", "check-ignore", "-z", "--stdin"])
        .current_dir(&repo_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"debug.log\0tracked.txt\0")
        .unwrap();
    let piped_out = child.wait_with_output().unwrap();
    assert_eq!(piped_out.status.code(), Some(0));
    assert_eq!(piped_out.stdout, b"debug.log\0");

    let _ = sub_v1;
}

#[cfg(unix)]
#[test]
fn oz_commit_and_oz_push_hooks_json_exit_1_and_no_verify() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("remote.git");
    fs::create_dir_all(&bare).unwrap();
    sys_git(&bare, &["init", "--bare", "-b", "main"]);

    let repo_dir = tmp.path().join("repo");
    init_repo(&repo_dir);
    sys_git(&repo_dir, &["remote", "add", "origin", bare.to_str().unwrap()]);
    sys_git(&repo_dir, &["push", "-u", "origin", "main"]);

    let husky_dir = repo_dir.join(".husky/_");
    fs::create_dir_all(&husky_dir).unwrap();
    sys_git(&repo_dir, &["config", "core.hooksPath", ".husky/_"]);

    let pre_commit = husky_dir.join("pre-commit");
    fs::write(&pre_commit, "#!/bin/sh\necho 'bad lint' >&2\nexit 1\n").unwrap();
    fs::set_permissions(&pre_commit, fs::Permissions::from_mode(0o755)).unwrap();

    let pre_push = husky_dir.join("pre-push");
    fs::write(&pre_push, "#!/bin/sh\necho 'push gate failed' >&2\nexit 1\n").unwrap();
    fs::set_permissions(&pre_push, fs::Permissions::from_mode(0o755)).unwrap();

    fs::write(repo_dir.join("feat.txt"), "hello\n").unwrap();

    // 1. `oz commit --json` fails with exit code 1 + structured `oz.v1` `check_failed`
    let c_fail = run_oz(&repo_dir, &["commit", "-m", "feat: fail", "--json"], &[]);
    assert_eq!(c_fail.code, 1, "stdout={}", c_fail.stdout);
    let c_json = c_fail.json.expect("json envelope");
    assert_eq!(c_json["schema"], OZ_SCHEMA_V1);
    assert_eq!(c_json["ok"], false);
    assert_eq!(c_json["error"]["code"], "check_failed");
    assert!(
        c_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bad lint")
    );

    // 2. `oz commit --no-verify --json` succeeds
    let c_ok = run_oz(
        &repo_dir,
        &["commit", "--no-verify", "-m", "feat: bypass", "--json"],
        &[],
    );
    assert_eq!(c_ok.code, 0, "stdout={} stderr={}", c_ok.stdout, c_ok.stderr);

    // 3. `oz git -nm "..."` honors combined `-n` bypass on shim
    fs::write(repo_dir.join("feat2.txt"), "two\n").unwrap();
    let shim_ok = run_oz(&repo_dir, &["git", "commit", "-nm", "feat: shim -n"], &[]);
    assert_eq!(shim_ok.code, 0, "stderr={}", shim_ok.stderr);

    // 4. `oz push --json` fails with exit code 1 + `check_failed`
    let p_fail = run_oz(&repo_dir, &["push", "origin", "main", "--json"], &[]);
    assert_eq!(p_fail.code, 1, "stdout={}", p_fail.stdout);
    let p_json = p_fail.json.expect("push json envelope");
    assert_eq!(p_json["schema"], OZ_SCHEMA_V1);
    assert_eq!(p_json["ok"], false);
    assert_eq!(p_json["error"]["code"], "check_failed");
    assert!(
        p_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("push gate failed")
    );

    // 5. `oz push --no-verify --json` and `oz git push --no-verify` bypass pre-push cleanly
    let p_ok = run_oz(
        &repo_dir,
        &["push", "origin", "main", "--no-verify", "--json"],
        &[],
    );
    assert_eq!(p_ok.code, 0, "stdout={} stderr={}", p_ok.stdout, p_ok.stderr);
    let head_local = sys_git(&repo_dir, &["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(
        sys_git(&bare, &["rev-parse", "refs/heads/main"]).trim(),
        head_local
    );
}

#[cfg(unix)]
#[test]
fn oz_clone_and_push_enterprise_ssh_config_and_insteadof_e2e() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path().join("home");
    let fake_bin = tmp.path().join("bin");
    let fetch_bare = tmp.path().join("fetch_origin.git");
    let push_bare = tmp.path().join("push_origin.git");
    let work_parent = fake_home.join("work");
    fs::create_dir_all(fake_home.join(".ssh")).unwrap();
    fs::create_dir_all(&fake_bin).unwrap();
    fs::create_dir_all(&fetch_bare).unwrap();
    fs::create_dir_all(&push_bare).unwrap();
    fs::create_dir_all(&work_parent).unwrap();

    sys_git(&fetch_bare, &["init", "--bare", "-b", "main"]);
    sys_git(&push_bare, &["init", "--bare", "-b", "main"]);
    let seed = tmp.path().join("seed");
    init_repo(&seed);
    sys_git(&seed, &["push", fetch_bare.to_str().unwrap(), "main"]);
    sys_git(&seed, &["push", push_bare.to_str().unwrap(), "main"]);

    let ssh_log = tmp.path().join("ssh_invocations.log");
    let fake_key = fake_home.join(".ssh/id_enterprise");
    fs::write(&fake_key, "dummy-key\n").unwrap();

    fs::write(
        fake_home.join(".ssh/config"),
        "Host corp-fetch\n\
             HostName 127.0.0.1\n\
             User fetchuser\n\
             Port 2222\n\
             IdentityFile ~/.ssh/id_enterprise\n\
             IdentitiesOnly yes\n\
             ProxyJump jumpbox.corp.internal\n\
         Host corp-push\n\
             HostName 127.0.0.1\n\
             User pushuser\n\
             Port 2223\n\
             IdentityFile ~/.ssh/id_enterprise\n",
    )
    .unwrap();

    let work_inc = fake_home.join("work.gitconfig");
    fs::write(&work_inc, "[user]\n\temail = corp@enterprise.test\n").unwrap();
    fs::write(
        fake_home.join(".gitconfig"),
        format!(
            "[user]\n\tname = Enterprise User\n\temail = personal@example.com\n\
             [url \"ssh://corp-fetch/\"]\n\tinsteadOf = \"gh:\"\n\
             [url \"ssh://corp-push/\"]\n\tpushInsteadOf = \"gh:\"\n\
             [includeIf \"gitdir:~/work/\"]\n\tpath = {}\n",
            work_inc.display()
        ),
    )
    .unwrap();

    // Fake `ssh` wrapper verifying all SSH config CLI flags + agent env and routing to local bare repos
    let ssh_wrapper = fake_bin.join("ssh");
    fs::write(
        &ssh_wrapper,
        format!(
            "#!/bin/sh\n\
             printf 'SOCK=%s ARGS=%s\\n' \"$SSH_AUTH_SOCK\" \"$*\" >> '{}'\n\
             op=\"\"\n\
             case \"$*\" in\n\
               *\"git-upload-pack\"*) op=\"git-upload-pack\" ;;\n\
               *\"git-receive-pack\"*) op=\"git-receive-pack\" ;;\n\
               *) echo \"unexpected op: $*\" >&2; exit 1 ;;\n\
             esac\n\
             case \"$*\" in\n\
               *\"corp-fetch\"*) target=\"{}\" ;;\n\
               *\"corp-push\"*) target=\"{}\" ;;\n\
               *) echo \"unexpected host args: $*\" >&2; exit 1 ;;\n\
             esac\n\
             exec \"$op\" \"$target\"\n",
            ssh_log.display(),
            fetch_bare.display(),
            push_bare.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh_wrapper, fs::Permissions::from_mode(0o755)).unwrap();

    let orig_path = std::env::var("PATH").unwrap_or_default();
    let custom_path = format!("{}:{orig_path}", fake_bin.display());
    let clone_dest = work_parent.join("app");

    let env = [
        ("HOME", fake_home.to_str().unwrap()),
        ("PATH", custom_path.as_str()),
        ("SSH_AUTH_SOCK", "/tmp/fake-enterprise-agent.sock"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ];

    // 1. `oz clone gh:app.git ~/work/app` uses `insteadOf` -> `ssh://corp-fetch/app.git` + `~/.ssh/config`,
    //    while persisting `"gh:app.git"` in `.git/config`!
    let cloned = run_oz(
        &work_parent,
        &["clone", "gh:app.git", clone_dest.to_str().unwrap()],
        &env,
    );
    let logged_dbg = fs::read_to_string(&ssh_log).unwrap_or_default();
    assert_eq!(
        cloned.code, 0,
        "stdout={} stderr={} ssh_log={logged_dbg}",
        cloned.stdout, cloned.stderr
    );
    assert!(clone_dest.join("README.md").is_file());
    let cfg_txt = fs::read_to_string(clone_dest.join(".git/config")).unwrap();
    assert!(cfg_txt.contains("url = gh:app.git"), "{cfg_txt}");

    // 2. Commit inside `~/work/app` inherits `includeIf gitdir:~/work/` user.email!
    fs::write(clone_dest.join("work.txt"), "enterprise commit\n").unwrap();
    let comm = run_oz(&clone_dest, &["commit", "-m", "feat: enterprise"], &env);
    assert_eq!(comm.code, 0, "stdout={} stderr={}", comm.stdout, comm.stderr);
    let author_email = sys_git(&clone_dest, &["log", "-1", "--pretty=%ae"]);
    assert_eq!(author_email.trim(), "corp@enterprise.test");

    // 3. `oz push` rewrites `gh:app.git` via `pushInsteadOf` -> `ssh://corp-push/app.git` (`Port=2223`, `User=pushuser`)
    let pushed = run_oz(&clone_dest, &["push", "origin", "main"], &env);
    assert_eq!(pushed.code, 0, "stdout={} stderr={}", pushed.stdout, pushed.stderr);

    let pushed_oid = sys_git(&push_bare, &["rev-parse", "refs/heads/main"]).trim().to_string();
    let local_oid = sys_git(&clone_dest, &["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(pushed_oid, local_oid);

    let ssh_calls = fs::read_to_string(&ssh_log).unwrap();
    assert!(ssh_calls.contains("SOCK=/tmp/fake-enterprise-agent.sock"), "{ssh_calls}");
    assert!(ssh_calls.contains("-F "), "{ssh_calls}");
    assert!(ssh_calls.contains("HostName=127.0.0.1"), "{ssh_calls}");
    assert!(ssh_calls.contains("User=fetchuser"), "{ssh_calls}");
    assert!(ssh_calls.contains("Port=2222"), "{ssh_calls}");
    assert!(ssh_calls.contains("IdentitiesOnly=yes"), "{ssh_calls}");
    assert!(ssh_calls.contains("ProxyJump=jumpbox.corp.internal"), "{ssh_calls}");
    assert!(ssh_calls.contains("User=pushuser"), "{ssh_calls}");
    assert!(ssh_calls.contains("Port=2223"), "{ssh_calls}");
}
