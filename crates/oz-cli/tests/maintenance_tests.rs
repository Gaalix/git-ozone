// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for `oz gc` (`--oplog-days`, `--dry-run`, `--json`) and
//! `oz fsck` (`--repair`, `--json`, colocated `.git` + OpLog + mount/baseline checks).

use chrono::{Duration, Utc};
use oz_core::{BackendKind, MountRecord, OZ_SCHEMA_V1, OzoneLayout, OzoneRepo, WorkspaceState};
use oz_oplog::{
    ActorInfo, ActorKind, OpKind, OpLogEntry, UndoTarget, append_entry, checkpoint, list_entries,
    load_entry, read_head_op_id, redo, undo,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn oz_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oz"))
}

fn sys_git_bin() -> PathBuf {
    let unix_git = Path::new("/usr/bin/git");
    if unix_git.is_file() {
        unix_git.to_path_buf()
    } else {
        PathBuf::from("git")
    }
}

struct CmdOut {
    code: i32,
    stdout: String,
    stderr: String,
    json: Option<Value>,
}

fn run_oz(cwd: &Path, args: &[&str]) -> CmdOut {
    let out = Command::new(oz_bin())
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::null())
        .output()
        .expect("run oz");
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

fn run_git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new(sys_git_bin())
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::null())
        .output()
        .expect("run git");
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

fn assert_git_fsck_clean(repo_dir: &Path) {
    let out = Command::new(sys_git_bin())
        .args(["fsck", "--full"])
        .current_dir(repo_dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .expect("run git fsck --full");
    assert!(
        out.status.success(),
        "git fsck --full failed in {}:\nstdout: {}\nstderr: {}",
        repo_dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_oz_v1_envelope(v: &Value, expected_ok: bool) {
    assert_eq!(v["schema"], OZ_SCHEMA_V1);
    assert_eq!(v["ok"], expected_ok);
    assert!(v.get("op_id").is_some());
    assert!(v.get("workspace").is_some());
    assert!(v.get("branch").is_some());
    assert!(v.get("head_oid").is_some());
    assert!(v.get("data").is_some());
    assert!(v["warnings"].is_array());
    assert!(v["conflicts"].is_array());
    assert!(v["recovery_commands"].is_array());
    if expected_ok {
        assert!(v.get("error").is_none());
    } else {
        assert!(v["error"].is_object());
        assert!(v["error"]["code"].is_string());
        assert!(v["error"]["exit_code"].is_i64());
        assert!(v["error"]["message"].is_string());
    }
}

fn init_test_repo(dir: &Path) -> String {
    fs::create_dir_all(dir).expect("mkdir repo");
    run_git(dir, &["init", "-b", "main"]);
    run_git(dir, &["config", "user.name", "Ozone Maint Test"]);
    run_git(dir, &["config", "user.email", "maint@ozone.test"]);
    fs::write(dir.join("README.md"), "# Maintenance Test\n").expect("write readme");
    fs::write(dir.join("lib.rs"), "pub fn v() -> i32 { 1 }\n").expect("write lib.rs");
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-m", "initial commit"]);
    run_git(dir, &["rev-parse", "HEAD"])
}

fn rewrite_entry_timestamp(layout: &OzoneLayout, op_id: &str, days_ago: i64) {
    let p = layout.oplog_dir().join("ops").join(format!("{op_id}.json"));
    let mut entry: OpLogEntry =
        serde_json::from_str(&fs::read_to_string(&p).expect("read op")).expect("parse op");
    entry.timestamp = Utc::now() - Duration::days(days_ago);
    fs::write(&p, serde_json::to_string_pretty(&entry).unwrap()).expect("write backdated op");
}

#[test]
fn gc_dry_run_and_retention_preserves_head_active_undo_chain_and_objects() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("gc-repo");
    let head_oid = init_test_repo(&repo_dir);
    let repo = OzoneRepo::discover(Some(&repo_dir)).expect("discover");
    repo.layout.ensure_initialized().expect("init ozone");

    let actor_default = ActorInfo {
        kind: ActorKind::Human,
        client: Some("maint-test".to_string()),
        workspace: None,
    };

    // 1. Old superseded checkpoints on `default` (50 days & 45 days old).
    fs::write(repo_dir.join("lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
    let cp_old_1 = checkpoint(&repo, "oz checkpoint", Some("old-1"), actor_default.clone(), None)
        .expect("cp_old_1");
    fs::write(repo_dir.join("lib.rs"), "pub fn v() -> i32 { 3 }\n").unwrap();
    let cp_old_2 = checkpoint(&repo, "oz checkpoint", Some("old-2"), actor_default.clone(), None)
        .expect("cp_old_2");

    // 2. Entry for an inactive/removed workspace `ws-dead` (50 days old) -> should be pruned.
    let dead_entry = OpLogEntry {
        op_id: oz_oplog::new_op_id(),
        parents: Vec::new(),
        timestamp: Utc::now() - Duration::days(50),
        actor: ActorInfo {
            kind: ActorKind::Agent,
            client: Some("test".to_string()),
            workspace: Some("ws-dead".to_string()),
        },
        kind: OpKind::Mutate,
        command: "oz commit -m dead".to_string(),
        message: None,
        inverse_of: None,
        undone_ops: Vec::new(),
        rewound_uncheckpointed: false,
        ref_deltas: BTreeMap::new(),
        ws_snapshots: BTreeMap::new(),
    };
    let dead_op = append_entry(&repo.layout, dead_entry).expect("append dead op");

    // 3. Active workspace `ws-idle` whose latest op was 45 days ago -> must be PROTECTED!
    let ws_idle_dir = tmp.path().join("gc-repo.oz/ws-idle");
    fs::create_dir_all(&ws_idle_dir).unwrap();
    let ws_idle_state = WorkspaceState {
        schema_version: WorkspaceState::CURRENT_SCHEMA_VERSION,
        name: "ws-idle".to_string(),
        branch: "feat/idle".to_string(),
        base_branch: Some("main".to_string()),
        base_commit: Some(head_oid.clone()),
        base_tree: head_oid.clone(),
        backend: BackendKind::CasHardlink,
        worktree_path: ws_idle_dir.clone(),
        git_worktree_dir: None,
        baseline_id: None,
        dirty_paths: BTreeMap::new(),
        pinned_paths: Default::default(),
        scope: Vec::new(),
        deny: Vec::new(),
        created_at: Utc::now() - Duration::days(45),
        updated_at: Utc::now() - Duration::days(45),
        last_checkpoint_oid: None,
    };
    fs::write(
        repo.layout.workspace_state_path("ws-idle"),
        serde_json::to_string_pretty(&ws_idle_state).unwrap(),
    )
    .unwrap();
    let idle_entry = OpLogEntry {
        op_id: oz_oplog::new_op_id(),
        parents: Vec::new(),
        timestamp: Utc::now() - Duration::days(45),
        actor: ActorInfo {
            kind: ActorKind::Agent,
            client: Some("test".to_string()),
            workspace: Some("ws-idle".to_string()),
        },
        kind: OpKind::Mutate,
        command: "oz ws spawn ws-idle".to_string(),
        message: None,
        inverse_of: None,
        undone_ops: Vec::new(),
        rewound_uncheckpointed: false,
        ref_deltas: BTreeMap::new(),
        ws_snapshots: BTreeMap::new(),
    };
    let idle_op = append_entry(&repo.layout, idle_entry).expect("append idle op");

    // 4. Old checkpoint `cp_undone_target` (40 days old) that gets undone RECENTLY (2 days ago):
    //    because the recent Undo entry references `cp_undone_target`, GC MUST protect BOTH!
    fs::write(repo_dir.join("lib.rs"), "pub fn v() -> i32 { 4 }\n").unwrap();
    let cp_undone_target = checkpoint(
        &repo,
        "oz checkpoint",
        Some("to-undo"),
        actor_default.clone(),
        None,
    )
    .expect("cp_undone_target");
    let undo_recent = undo(
        &repo,
        UndoTarget::OpId(cp_undone_target.op_id.clone()),
        actor_default.clone(),
    )
    .expect("undo target");

    // Backdate older ops on disk while leaving `undo_recent` within 30 days.
    rewrite_entry_timestamp(&repo.layout, &cp_old_1.op_id, 50);
    rewrite_entry_timestamp(&repo.layout, &cp_old_2.op_id, 45);
    rewrite_entry_timestamp(&repo.layout, &dead_op.op_id, 50);
    rewrite_entry_timestamp(&repo.layout, &idle_op.op_id, 45);
    rewrite_entry_timestamp(&repo.layout, &cp_undone_target.op_id, 40);
    rewrite_entry_timestamp(&repo.layout, &undo_recent.op_id, 2);

    // Dry-run first: must NOT remove anything.
    let dry = run_oz(&repo_dir, &["gc", "--oplog-days", "30", "--dry-run", "--json"]);
    assert_eq!(dry.code, 0, "dry-run failed: {}\n{}", dry.stdout, dry.stderr);
    let dj = dry.json.expect("dry json");
    assert_oz_v1_envelope(&dj, true);
    assert!(dj["op_id"].is_null(), "oz gc has op_id=null");
    assert_eq!(dj["data"]["dry_run"], true);
    assert_eq!(dj["data"]["oplog_days"], 30);
    let predicted_removed: Vec<String> = dj["data"]["removed_entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(predicted_removed.contains(&cp_old_1.op_id));
    assert!(
        !predicted_removed.contains(&cp_old_2.op_id),
        "latest active forward op on default must be protected"
    );
    assert!(predicted_removed.contains(&dead_op.op_id));
    assert!(!predicted_removed.contains(&idle_op.op_id), "active ws latest protected");
    assert!(
        !predicted_removed.contains(&cp_undone_target.op_id),
        "undo target protected by closure"
    );
    assert!(!predicted_removed.contains(&undo_recent.op_id));
    assert!(
        repo.layout
            .oplog_dir()
            .join("ops")
            .join(format!("{}.json", cp_old_1.op_id))
            .is_file(),
        "dry-run must not delete files"
    );

    // Real GC run (default `--oplog-days 30`).
    let gc = run_oz(&repo_dir, &["gc", "--json"]);
    assert_eq!(gc.code, 0, "gc failed: {}\n{}", gc.stdout, gc.stderr);
    let gj = gc.json.expect("gc json");
    assert_oz_v1_envelope(&gj, true);
    assert_eq!(gj["data"]["dry_run"], false);
    assert_eq!(gj["data"]["oplog_days"], 30);
    assert!(gj["data"]["reclaimed_bytes"].as_u64().unwrap() > 0);
    assert!(gj["data"]["relinked_entries"].as_u64().unwrap() >= 1);

    // Expired entries & their `op-checkpoints` refs removed.
    assert!(
        !repo
            .layout
            .oplog_dir()
            .join("ops")
            .join(format!("{}.json", cp_old_1.op_id))
            .is_file()
    );
    assert!(
        !repo
            .layout
            .oplog_dir()
            .join("ops")
            .join(format!("{}.json", dead_op.op_id))
            .is_file()
    );

    // Protected entries intact.
    assert!(load_entry(&repo.layout, &cp_old_2.op_id).is_ok());
    assert!(load_entry(&repo.layout, &idle_op.op_id).is_ok());
    assert!(load_entry(&repo.layout, &cp_undone_target.op_id).is_ok());
    assert!(load_entry(&repo.layout, &undo_recent.op_id).is_ok());
    assert_eq!(
        read_head_op_id(&repo.layout).unwrap().as_deref(),
        Some(undo_recent.op_id.as_str())
    );

    // Surviving DAG has zero dangling parent pointers and connects back from HEAD.
    let surviving = list_entries(&repo.layout).expect("list surviving");
    let surviving_ids: std::collections::HashSet<String> =
        surviving.iter().map(|e| e.op_id.clone()).collect();
    for entry in &surviving {
        for parent in &entry.parents {
            assert!(
                surviving_ids.contains(parent),
                "surviving op {} has dangling parent {}",
                entry.op_id,
                parent
            );
        }
    }

    // `oz redo` on `undo_recent` STILL WORKS cleanly after GC!
    let redone = redo(&repo, actor_default.clone()).expect("redo after gc");
    assert_eq!(redone.redone_undo_op_id, undo_recent.op_id);
    assert_eq!(
        fs::read_to_string(repo_dir.join("lib.rs")).unwrap(),
        "pub fn v() -> i32 { 4 }\n"
    );

    // Undoing back across `cp_undone_target` and `cp_old_2` STILL WORKS after `cp_old_1` was GC'd!
    undo(&repo, UndoTarget::Steps(1), actor_default.clone()).expect("undo cp_undone_target after gc");
    assert_eq!(
        fs::read_to_string(repo_dir.join("lib.rs")).unwrap(),
        "pub fn v() -> i32 { 3 }\n"
    );
    undo(&repo, UndoTarget::Steps(1), actor_default).expect("undo cp_old_2 after gc");
    assert_eq!(
        fs::read_to_string(repo_dir.join("lib.rs")).unwrap(),
        "pub fn v() -> i32 { 2 }\n"
    );

    // Active `refs/ozone/checkpoints/default` is preserved.
    let cp_ref_oid = run_git(&repo_dir, &["rev-parse", "refs/ozone/checkpoints/default"]);
    assert!(!cp_ref_oid.is_empty());

    // Both `oz fsck --json` and `git fsck --full` pass!
    let fsck = run_oz(&repo_dir, &["fsck", "--json"]);
    assert_eq!(fsck.code, 0, "fsck after gc failed: {}\n{}", fsck.stdout, fsck.stderr);
    assert_oz_v1_envelope(&fsck.json.expect("fsck json"), true);
    assert_git_fsck_clean(&repo_dir);

    // Invalid `--oplog-days -1` returns exit 2 in `oz.v1` envelope.
    let bad = run_oz(&repo_dir, &["gc", "--oplog-days", "-1", "--json"]);
    assert_eq!(bad.code, 2);
    let bj = bad.json.expect("bad gc json");
    assert_oz_v1_envelope(&bj, false);
    assert_eq!(bj["error"]["code"], "invalid_usage");
}

#[test]
fn fsck_non_repo_and_clean_repo_checks() {
    let tmp = TempDir::new().expect("tempdir");
    let non_repo = tmp.path().join("not-a-repo");
    fs::create_dir_all(&non_repo).unwrap();

    let non_repo_out = run_oz(&non_repo, &["fsck", "--json"]);
    assert_eq!(non_repo_out.code, 2);
    let nrj = non_repo_out.json.expect("non-repo json");
    assert_oz_v1_envelope(&nrj, false);
    assert_eq!(nrj["error"]["code"], "not_a_repository");

    let repo_dir = tmp.path().join("clean-repo");
    init_test_repo(&repo_dir);
    let clean_repo = OzoneRepo::discover(Some(&repo_dir)).expect("discover clean");
    clean_repo.layout.ensure_initialized().expect("init clean ozone");
    let cp_out = run_oz(&repo_dir, &["checkpoint", "-m", "clean cp", "--json"]);
    assert_eq!(
        cp_out.code, 0,
        "checkpoint failed: {}\n{}",
        cp_out.stdout, cp_out.stderr
    );

    let clean = run_oz(&repo_dir, &["fsck", "--json"]);
    assert_eq!(clean.code, 0, "clean fsck failed: {}\n{}", clean.stdout, clean.stderr);
    let cj = clean.json.expect("clean json");
    assert_oz_v1_envelope(&cj, true);
    assert_eq!(cj["data"]["healthy"], true);
    assert_eq!(cj["data"]["git_fsck_passed"], true);
    assert_eq!(cj["data"]["unresolved_issue_count"], 0);
    assert!(cj["data"]["oplog_entries_checked"].as_u64().unwrap() >= 1);
    assert!(cj["data"]["snapshot_objects_checked"].as_u64().unwrap() >= 1);
}

#[test]
fn fsck_detects_and_repairs_broken_oplog_interrupted_txns_and_orphans_without_deleting_user_files()
{
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("repair-repo");
    let head_oid = init_test_repo(&repo_dir);
    let repo = OzoneRepo::discover(Some(&repo_dir)).expect("discover");
    repo.layout.ensure_initialized().expect("init ozone");

    // User working-tree files (tracked edit + untracked unicode + symlink + binary) that MUST survive!
    fs::write(repo_dir.join("lib.rs"), "pub fn v() -> i32 { 99 } // user edit\n").unwrap();
    fs::create_dir_all(repo_dir.join("café")).unwrap();
    fs::write(repo_dir.join("café/notes.txt"), "important user notes\n").unwrap();
    fs::write(repo_dir.join("blob.bin"), [0u8, 1, 2, 255, 0, 42]).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("../README.md", repo_dir.join("café/link_readme")).unwrap();

    // Create 2 checkpoints, then break oplog/HEAD + add unlinked entry + corrupt entry.
    let actor = ActorInfo {
        kind: ActorKind::Human,
        client: Some("maint-test".to_string()),
        workspace: None,
    };
    let cp1 = checkpoint(&repo, "oz checkpoint", Some("cp1"), actor.clone(), None).expect("cp1");
    fs::write(repo_dir.join("extra_dirty.txt"), "keep me\n").unwrap();
    let cp2 = checkpoint(&repo, "oz checkpoint", Some("cp2"), actor.clone(), None).expect("cp2");

    // Break `oplog/HEAD` to point to `cp1` so `cp2` becomes unlinked, and inject a corrupt op file.
    fs::write(repo.layout.oplog_dir().join("HEAD"), format!("{}\n", cp1.op_id)).unwrap();
    fs::write(
        repo.layout.oplog_dir().join("ops/op_01BAD000000000000000000000.json"),
        "{not valid json",
    )
    .unwrap();

    // Inject interrupted transaction files & staging dirs.
    fs::write(repo.layout.oplog_dir().join(".tmp.12345.0"), "partial").unwrap();
    fs::write(repo.layout.state_dir().join(".ghost.json.tmp.999"), "partial").unwrap();
    fs::create_dir_all(repo.layout.ozone_dir.join("integrate")).unwrap();
    fs::write(
        repo.layout.ozone_dir.join("integrate/.tmp_session_999.json"),
        "{}",
    )
    .unwrap();
    fs::create_dir_all(repo.layout.baselines_dir().join(".staging-aborted/root")).unwrap();

    // Inject orphaned op-checkpoint ref.
    run_git(
        &repo_dir,
        &[
            "update-ref",
            "refs/ozone/op-checkpoints/default/op_01ORPHAN0000000000000000",
            &cp1.dirty_tree,
        ],
    );

    // Inject orphaned workspace state + `.git/worktrees/dead-ws` (missing worktree on disk).
    let dead_ws_dir = tmp.path().join("repair-repo.oz/dead-ws");
    let dead_state = WorkspaceState {
        schema_version: WorkspaceState::CURRENT_SCHEMA_VERSION,
        name: "dead-ws".to_string(),
        branch: "feat/dead".to_string(),
        base_branch: Some("main".to_string()),
        base_commit: Some(head_oid.clone()),
        base_tree: head_oid.clone(),
        backend: BackendKind::CasHardlink,
        worktree_path: dead_ws_dir.clone(),
        git_worktree_dir: Some(repo.git_common_dir.join("worktrees/dead-ws")),
        baseline_id: None,
        dirty_paths: BTreeMap::new(),
        pinned_paths: Default::default(),
        scope: Vec::new(),
        deny: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        last_checkpoint_oid: None,
    };
    fs::write(
        repo.layout.workspace_state_path("dead-ws"),
        serde_json::to_string_pretty(&dead_state).unwrap(),
    )
    .unwrap();
    fs::create_dir_all(repo.git_common_dir.join("worktrees/dead-ws")).unwrap();
    fs::write(
        repo.git_common_dir.join("worktrees/dead-ws/gitdir"),
        format!("{}\n", dead_ws_dir.join(".git").display()),
    )
    .unwrap();

    // Inject stale mount.json (mounted: true, dead PID 999999) for a non-deleted workspace with files in upper/.
    let stale_ws_dir = tmp.path().join("repair-repo.oz/stale-ws");
    fs::create_dir_all(&stale_ws_dir).unwrap();
    fs::create_dir_all(repo.layout.ws_upper_dir("stale-ws")).unwrap();
    fs::write(
        repo.layout.ws_upper_dir("stale-ws").join("precious.txt"),
        "do not delete upper file\n",
    )
    .unwrap();
    let stale_state = WorkspaceState {
        schema_version: WorkspaceState::CURRENT_SCHEMA_VERSION,
        name: "stale-ws".to_string(),
        branch: "feat/stale".to_string(),
        base_branch: Some("main".to_string()),
        base_commit: Some(head_oid.clone()),
        base_tree: head_oid,
        backend: BackendKind::LinuxFuse3,
        worktree_path: stale_ws_dir.clone(),
        git_worktree_dir: None,
        baseline_id: None,
        dirty_paths: BTreeMap::new(),
        pinned_paths: Default::default(),
        scope: Vec::new(),
        deny: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        last_checkpoint_oid: None,
    };
    fs::write(
        repo.layout.workspace_state_path("stale-ws"),
        serde_json::to_string_pretty(&stale_state).unwrap(),
    )
    .unwrap();
    let stale_mount = MountRecord {
        schema_version: MountRecord::CURRENT_SCHEMA_VERSION,
        workspace: "stale-ws".to_string(),
        branch: "feat/stale".to_string(),
        backend: BackendKind::LinuxFuse3,
        repo_root: repo_dir.clone(),
        git_dir: repo.git_common_dir.clone(),
        mount_point: stale_ws_dir.clone(),
        upper_dir: Some(repo.layout.ws_upper_dir("stale-ws")),
        meta_dir: Some(repo.layout.ws_tombstone_dir("stale-ws")),
        base_tree: None,
        base_commit: None,
        socket_path: None,
        daemon_pid: Some(999_999),
        mounted: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    fs::write(
        repo.layout.ws_mount_record_path("stale-ws"),
        serde_json::to_string_pretty(&stale_mount).unwrap(),
    )
    .unwrap();

    // Inject orphaned warm baseline (commit OID does not exist in Git ODB).
    let orphan_bl = repo.layout.baselines_dir().join("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
    fs::create_dir_all(orphan_bl.join("root")).unwrap();
    fs::write(
        orphan_bl.join("metadata.json"),
        r#"{"commit_oid":"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"}"#,
    )
    .unwrap();

    // 1. Read-only `oz fsck --json` must detect issues and exit 1 without modifying disk.
    let pre = run_oz(&repo_dir, &["fsck", "--json"]);
    assert_eq!(pre.code, 1, "expected check_failed exit 1: {}", pre.stdout);
    let pj = pre.json.expect("pre fsck json");
    assert_oz_v1_envelope(&pj, false);
    assert_eq!(pj["error"]["code"], "check_failed");
    assert_eq!(pj["data"]["healthy"], false);
    assert_eq!(pj["data"]["git_fsck_passed"], true);
    let issue_codes: Vec<&str> = pj["data"]["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["code"].as_str().unwrap())
        .collect();
    for expected in [
        "interrupted_transaction",
        "interrupted_baseline",
        "corrupt_oplog_entry",
        "unlinked_oplog_entry",
        "orphaned_checkpoint_ref",
        "orphaned_workspace_state",
        "stale_mount_record",
        "orphaned_baseline",
    ] {
        assert!(
            issue_codes.contains(&expected),
            "expected issue '{expected}' in {issue_codes:?}"
        );
    }

    // 2. Run `oz fsck --repair --json`.
    let repaired = run_oz(&repo_dir, &["fsck", "--repair", "--json"]);
    assert_eq!(
        repaired.code, 0,
        "repair should succeed cleanly: {}\n{}",
        repaired.stdout, repaired.stderr
    );
    let rj = repaired.json.expect("repair json");
    assert_oz_v1_envelope(&rj, true);
    assert_eq!(rj["data"]["healthy"], true);
    assert_eq!(rj["data"]["unresolved_issue_count"], 0);
    assert!(rj["data"]["repaired_count"].as_u64().unwrap() >= 8);

    // 3. Verify `oz fsck --json` now passes cleanly!
    let post = run_oz(&repo_dir, &["fsck", "--json"]);
    assert_eq!(post.code, 0, "post-repair fsck failed: {}\n{}", post.stdout, post.stderr);
    let post_j = post.json.expect("post json");
    assert_oz_v1_envelope(&post_j, true);
    assert_eq!(post_j["data"]["healthy"], true);

    // 4. Verify `oplog/HEAD` was recovered to `cp2` and `git fsck --full` passes!
    assert_eq!(
        read_head_op_id(&repo.layout).unwrap().as_deref(),
        Some(cp2.op_id.as_str())
    );
    assert_git_fsck_clean(&repo_dir);

    // 5. CRITICAL: Verify NO user working-tree files or upper/ files were deleted or altered!
    assert_eq!(
        fs::read_to_string(repo_dir.join("lib.rs")).unwrap(),
        "pub fn v() -> i32 { 99 } // user edit\n"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("café/notes.txt")).unwrap(),
        "important user notes\n"
    );
    assert_eq!(
        fs::read(repo_dir.join("blob.bin")).unwrap(),
        vec![0u8, 1, 2, 255, 0, 42]
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("extra_dirty.txt")).unwrap(),
        "keep me\n"
    );
    #[cfg(unix)]
    assert_eq!(
        fs::read_link(repo_dir.join("café/link_readme")).unwrap(),
        PathBuf::from("../README.md")
    );
    assert_eq!(
        fs::read_to_string(repo.layout.ws_upper_dir("stale-ws").join("precious.txt")).unwrap(),
        "do not delete upper file\n"
    );
    let updated_mount: MountRecord = serde_json::from_str(
        &fs::read_to_string(repo.layout.ws_mount_record_path("stale-ws")).unwrap(),
    )
    .unwrap();
    assert!(!updated_mount.mounted);
    assert_eq!(updated_mount.daemon_pid, None);
    assert!(!repo.layout.workspace_state_path("dead-ws").exists());
    assert!(!repo.git_common_dir.join("worktrees/dead-ws").exists());
    assert!(!orphan_bl.exists());
}

#[test]
fn fsck_detects_corrupt_git_object_via_git_fsck() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("corrupt-git");
    init_test_repo(&repo_dir);

    let blob_oid = run_git(&repo_dir, &["rev-parse", "HEAD:lib.rs"]);
    let (prefix, rest) = blob_oid.split_at(2);
    let obj_path = repo_dir.join(".git/objects").join(prefix).join(rest);
    assert!(obj_path.is_file(), "loose object must exist at {}", obj_path.display());

    // Make writable and truncate the loose Git blob object to simulate disk corruption.
    let mut perms = fs::metadata(&obj_path).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o644);
        fs::set_permissions(&obj_path, perms).unwrap();
    }
    fs::write(&obj_path, b"corrupted-zlib-payload").unwrap();

    let out = run_oz(&repo_dir, &["fsck", "--json"]);
    assert_eq!(out.code, 1, "corrupt git must exit 1: {}", out.stdout);
    let j = out.json.expect("fsck json");
    assert_oz_v1_envelope(&j, false);
    assert_eq!(j["error"]["code"], "check_failed");
    assert_eq!(j["data"]["git_fsck_passed"], false);
    assert_eq!(j["data"]["healthy"], false);
    let codes: Vec<&str> = j["data"]["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"git_fsck_failed"));

    // `--repair` must not silently clear non-repairable Git object corruption.
    let rep = run_oz(&repo_dir, &["fsck", "--repair", "--json"]);
    assert_eq!(rep.code, 1);
    let rj = rep.json.expect("repair json");
    assert_eq!(rj["data"]["git_fsck_passed"], false);
    assert_eq!(rj["data"]["healthy"], false);
}
