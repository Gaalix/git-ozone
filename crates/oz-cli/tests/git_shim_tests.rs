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

fn sys_git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
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
