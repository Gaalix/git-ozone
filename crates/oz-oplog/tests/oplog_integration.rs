// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

#![allow(clippy::too_many_lines)]

use chrono::Utc;
use gix::hash::ObjectId;
use gix::objs::tree::EntryKind;
use oz_core::{
    BackendKind, ChangeKind, FileChange, MountRecord, OzError, OzoneLayout, OzoneRepo,
    WorkspaceState,
};
use oz_oplog::{
    ActorInfo, ActorKind, DirtyFileState, OpKind, OpLogEntry, UndoTarget, append_entry, checkpoint,
    list_entries, load_entry, new_op_id, read_head_op_id, record_mutation, redo, undo,
};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn sub10ms_budget() -> Duration {
    if std::env::var_os("CI").is_some() && cfg!(debug_assertions) {
        Duration::from_millis(500)
    } else {
        Duration::from_millis(10)
    }
}

fn agent_actor(ws: Option<&str>) -> ActorInfo {
    ActorInfo {
        kind: ActorKind::Agent,
        client: Some("oplog-test".to_string()),
        workspace: ws.map(str::to_string),
    }
}

fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: stdout={}, stderr={}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo_with_commit(dir: &Path, files: &[(&str, &str)]) -> (OzoneRepo, String) {
    fs::create_dir_all(dir).expect("mkdir repo");
    run_git(dir, &["init", "-b", "main"]);
    run_git(dir, &["config", "user.name", "Ozone Test"]);
    run_git(dir, &["config", "user.email", "test@ozone.dev"]);

    for (rel, content) in files {
        let full = dir.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("mkdir parent");
        }
        fs::write(&full, content).expect("write file");
    }
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-m", "initial commit"]);
    let head = run_git(dir, &["rev-parse", "HEAD"]);
    let oz_repo = OzoneRepo::discover(Some(dir)).expect("discover repo");
    (oz_repo, head)
}

/// Helper that creates a commit directly via `gix` (writing blobs, editing tree, advancing
/// `refs/heads/<branch>`, and updating working-tree files on disk).
fn commit_via_gix(
    repo_root: &Path,
    branch: &str,
    message: &str,
    writes: &[(&str, &str)],
    deletes: &[&str],
) -> ObjectId {
    let mut gix_repo = gix::open(repo_root).expect("open gix");
    gix_repo
        .committer_or_set_fallback("Ozone Test", "test@ozone.dev")
        .expect("committer");

    let ref_name = format!("refs/heads/{branch}");
    let (parent_commits, base_tree) =
        match gix_repo.try_find_reference(&ref_name).expect("find ref") {
            Some(mut r) => {
                let commit = r.peel_to_commit().expect("peel commit");
                let tree = commit.tree_id().expect("tree id").detach();
                (vec![commit.id], tree)
            }
            None => (Vec::new(), ObjectId::empty_tree(gix_repo.object_hash())),
        };

    let mut editor = gix_repo.edit_tree(base_tree).expect("edit tree");
    for (rel, content) in writes {
        let blob_id = gix_repo
            .write_blob(content.as_bytes())
            .expect("write blob")
            .detach();
        editor
            .upsert(*rel, EntryKind::Blob, blob_id)
            .expect("upsert");

        let disk_path = repo_root.join(rel);
        if let Some(parent) = disk_path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&disk_path, content).expect("write disk");
    }
    for rel in deletes {
        editor.remove(*rel).expect("remove");
        let _ = fs::remove_file(repo_root.join(rel));
    }

    let new_tree = editor.write().expect("write tree").detach();
    gix_repo
        .commit(ref_name.as_str(), message, new_tree, parent_commits)
        .expect("gix commit")
        .detach()
}

#[test]
fn checkpoint_then_dirty_edit_then_undo_and_redo_restores_exact_state() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    let (repo, head_oid) = init_repo_with_commit(
        &repo_dir,
        &[
            ("a.txt", "base-a\n"),
            ("sub/keep.txt", "keep\n"),
            ("sub/to_delete.txt", "delete-me-later\n"),
        ],
    );

    // Pre-checkpoint dirty state: modify `a.txt` and add `pre_dirty.txt`.
    fs::write(repo_dir.join("a.txt"), "checkpoint-v1\n").expect("write a");
    fs::write(repo_dir.join("pre_dirty.txt"), "created-before-cp\n").expect("write pre_dirty");

    let cp = checkpoint(
        &repo,
        "oz checkpoint -m pre-edit",
        Some("pre-edit"),
        agent_actor(None),
        None,
    )
    .expect("checkpoint");

    assert_eq!(cp.entry.kind, OpKind::Checkpoint);
    assert!(
        repo.layout
            .oplog_dir()
            .join("ops")
            .join(format!("{}.json", cp.op_id))
            .is_file()
    );
    assert_eq!(
        read_head_op_id(&repo.layout).expect("head").as_deref(),
        Some(cp.op_id.as_str())
    );

    // Verify `dirty_tree` is a real Git tree in `.git/objects/` and checkpoint refs point to it.
    let gix_repo = gix::open(&repo_dir).expect("open gix");
    let dirty_tree_id = ObjectId::from_hex(cp.dirty_tree.as_bytes()).expect("hex");
    let tree = gix_repo.find_tree(dirty_tree_id).expect("find dirty tree");
    assert!(
        tree.lookup_entry_by_path("pre_dirty.txt")
            .unwrap()
            .is_some()
    );
    assert_eq!(
        gix_repo
            .find_reference(&cp.checkpoint_ref)
            .expect("cp ref")
            .id()
            .detach(),
        dirty_tree_id
    );
    assert_eq!(
        gix_repo
            .find_reference(&cp.op_checkpoint_ref)
            .expect("op cp ref")
            .id()
            .detach(),
        dirty_tree_id
    );

    // Post-checkpoint dirty edits: overwrite `a.txt`, delete `sub/to_delete.txt` and `pre_dirty.txt`,
    // and add `nested/extra.txt`.
    fs::write(repo_dir.join("a.txt"), "broken-v2\n").expect("overwrite a");
    fs::remove_file(repo_dir.join("sub/to_delete.txt")).expect("delete tracked");
    fs::remove_file(repo_dir.join("pre_dirty.txt")).expect("delete pre_dirty");
    fs::create_dir_all(repo_dir.join("nested")).expect("mkdir nested");
    fs::write(repo_dir.join("nested/extra.txt"), "extra-v2\n").expect("write extra");

    // Undo rewinds back to checkpoint state!
    let undo_out = undo(&repo, UndoTarget::Steps(1), agent_actor(None)).expect("undo");
    assert_eq!(undo_out.entry.kind, OpKind::Undo);
    assert_eq!(
        undo_out.entry.inverse_of.as_deref(),
        Some(cp.op_id.as_str())
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("a.txt")).expect("read a"),
        "checkpoint-v1\n"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("pre_dirty.txt")).expect("read pre_dirty"),
        "created-before-cp\n"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("sub/to_delete.txt")).expect("read to_delete"),
        "delete-me-later\n"
    );
    assert!(!repo_dir.join("nested/extra.txt").exists());
    assert!(!repo_dir.join("nested").exists(), "empty parent dir pruned");
    assert_eq!(run_git(&repo_dir, &["rev-parse", "HEAD"]), head_oid);

    // Redo brings back all post-checkpoint edits!
    let redo_out = redo(&repo, agent_actor(None)).expect("redo");
    assert_eq!(redo_out.entry.kind, OpKind::Redo);
    assert_eq!(
        redo_out.entry.inverse_of.as_deref(),
        Some(undo_out.op_id.as_str())
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("a.txt")).expect("read a"),
        "broken-v2\n"
    );
    assert!(!repo_dir.join("pre_dirty.txt").exists());
    assert!(!repo_dir.join("sub/to_delete.txt").exists());
    assert_eq!(
        fs::read_to_string(repo_dir.join("nested/extra.txt")).expect("read extra"),
        "extra-v2\n"
    );

    // Second undo/redo cycle also works cleanly.
    undo(&repo, 1usize, agent_actor(None)).expect("second undo");
    assert_eq!(
        fs::read_to_string(repo_dir.join("a.txt")).expect("read a"),
        "checkpoint-v1\n"
    );
    assert!(repo_dir.join("sub/to_delete.txt").is_file());
    redo(&repo, agent_actor(None)).expect("second redo");
    assert_eq!(
        fs::read_to_string(repo_dir.join("a.txt")).expect("read a"),
        "broken-v2\n"
    );
    assert!(!repo_dir.join("sub/to_delete.txt").exists());
}

#[test]
fn chained_checkpoints_and_multistep_undo_redo() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    let (repo, _) = init_repo_with_commit(&repo_dir, &[("file.txt", "v0\n")]);

    let cp1 = checkpoint(&repo, "oz checkpoint 1", None, agent_actor(None), None).expect("cp1");
    fs::write(repo_dir.join("file.txt"), "v1\n").expect("write v1");
    let cp2 = checkpoint(&repo, "oz checkpoint 2", None, agent_actor(None), None).expect("cp2");
    fs::write(repo_dir.join("file.txt"), "v2\n").expect("write v2");

    // Step 1 undo rewinds un-checkpointed v2 -> cp2 (v1).
    undo(&repo, UndoTarget::Steps(1), agent_actor(None)).expect("undo to cp2");
    assert_eq!(
        fs::read_to_string(repo_dir.join("file.txt")).unwrap(),
        "v1\n"
    );

    // Step 2 undo undoes cp2 -> cp1 (v0).
    undo(&repo, UndoTarget::Steps(1), agent_actor(None)).expect("undo cp2 to cp1");
    assert_eq!(
        fs::read_to_string(repo_dir.join("file.txt")).unwrap(),
        "v0\n"
    );

    // Redo twice brings back v1 then v2.
    redo(&repo, agent_actor(None)).expect("redo 1");
    assert_eq!(
        fs::read_to_string(repo_dir.join("file.txt")).unwrap(),
        "v1\n"
    );
    redo(&repo, agent_actor(None)).expect("redo 2");
    assert_eq!(
        fs::read_to_string(repo_dir.join("file.txt")).unwrap(),
        "v2\n"
    );

    let entries = list_entries(&repo.layout).expect("list");
    assert_eq!(entries[0].op_id, cp1.op_id);
    assert_eq!(entries[1].op_id, cp2.op_id);
}

#[test]
fn commit_via_gix_record_mutation_undo_and_redo() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    let (repo, c1_hex) = init_repo_with_commit(
        &repo_dir,
        &[
            ("src/lib.rs", "pub fn v() -> i32 { 1 }\n"),
            ("old.txt", "to remove\n"),
        ],
    );

    let (c2_id, mutate_entry) = record_mutation(
        &repo,
        "oz commit -m feat: v2",
        agent_actor(None),
        None,
        || {
            Ok(commit_via_gix(
                &repo_dir,
                "main",
                "feat: v2",
                &[
                    ("src/lib.rs", "pub fn v() -> i32 { 2 }\n"),
                    ("src/extra.rs", "pub const X: i32 = 42;\n"),
                ],
                &["old.txt"],
            ))
        },
    )
    .expect("record commit mutation");

    let c2_hex = c2_id.to_hex().to_string();
    assert_eq!(mutate_entry.kind, OpKind::Mutate);
    assert_eq!(
        mutate_entry
            .ref_deltas
            .get("refs/heads/main")
            .expect("delta")
            .old
            .as_deref(),
        Some(c1_hex.as_str())
    );
    assert_eq!(
        mutate_entry
            .ref_deltas
            .get("refs/heads/main")
            .expect("delta")
            .new
            .as_deref(),
        Some(c2_hex.as_str())
    );
    assert_eq!(run_git(&repo_dir, &["rev-parse", "HEAD"]), c2_hex);

    // Undo commit: moves `refs/heads/main` back to C1 AND restores working-tree files!
    let undo_out =
        undo(&repo, mutate_entry.op_id.as_str(), agent_actor(None)).expect("undo commit");
    assert_eq!(run_git(&repo_dir, &["rev-parse", "HEAD"]), c1_hex);
    assert_eq!(
        fs::read_to_string(repo_dir.join("src/lib.rs")).unwrap(),
        "pub fn v() -> i32 { 1 }\n"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("old.txt")).unwrap(),
        "to remove\n"
    );
    assert!(!repo_dir.join("src/extra.rs").exists());
    assert!(undo_out.restored_files >= 3);

    // Redo commit: moves `refs/heads/main` back to C2 AND restores working-tree files!
    redo(&repo, agent_actor(None)).expect("redo commit");
    assert_eq!(run_git(&repo_dir, &["rev-parse", "HEAD"]), c2_hex);
    assert_eq!(
        fs::read_to_string(repo_dir.join("src/lib.rs")).unwrap(),
        "pub fn v() -> i32 { 2 }\n"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("src/extra.rs")).unwrap(),
        "pub const X: i32 = 42;\n"
    );
    assert!(!repo_dir.join("old.txt").exists());
}

#[test]
fn undo_detects_cas_ref_conflict_without_corrupting_files() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    let (repo, _) = init_repo_with_commit(&repo_dir, &[("a.txt", "v1\n")]);

    let (_, m1) = record_mutation(&repo, "oz commit v2", agent_actor(None), None, || {
        Ok(commit_via_gix(
            &repo_dir,
            "main",
            "v2",
            &[("a.txt", "v2\n")],
            &[],
        ))
    })
    .expect("m1");

    // External commit advances `refs/heads/main` outside OpLog.
    let c3 = commit_via_gix(&repo_dir, "main", "v3-external", &[("a.txt", "v3\n")], &[]);

    let err = undo(&repo, m1.op_id.as_str(), agent_actor(None)).expect_err("CAS conflict");
    assert!(matches!(err, OzError::Conflict { .. }), "got {err:?}");
    // Ref & working tree untouched.
    assert_eq!(
        run_git(&repo_dir, &["rev-parse", "HEAD"]),
        c3.to_hex().to_string()
    );
    assert_eq!(fs::read_to_string(repo_dir.join("a.txt")).unwrap(), "v3\n");
}

#[test]
fn unborn_initial_commit_undo_and_redo() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).expect("mkdir");
    run_git(&repo_dir, &["init", "-b", "main"]);
    run_git(&repo_dir, &["config", "user.name", "Ozone Test"]);
    run_git(&repo_dir, &["config", "user.email", "test@ozone.dev"]);
    run_git(&repo_dir, &["config", "core.autocrlf", "false"]);
    fs::write(repo_dir.join("hello.txt"), "before-commit\n").expect("write");

    let repo = OzoneRepo::discover(Some(&repo_dir)).expect("discover unborn");
    let (c1, m1) = record_mutation(&repo, "oz commit init", agent_actor(None), None, || {
        Ok(commit_via_gix(
            &repo_dir,
            "main",
            "init",
            &[("hello.txt", "committed\n"), ("new.txt", "added\n")],
            &[],
        ))
    })
    .expect("unborn commit");

    assert_eq!(
        m1.ref_deltas.get("refs/heads/main").expect("delta").old,
        None
    );
    assert_eq!(
        m1.ref_deltas
            .get("refs/heads/main")
            .expect("delta")
            .new
            .as_deref(),
        Some(c1.to_hex().to_string().as_str())
    );

    undo(&repo, 1usize, agent_actor(None)).expect("undo unborn commit");
    let rev_parse = Command::new("git")
        .args(["rev-parse", "--verify", "refs/heads/main"])
        .current_dir(&repo_dir)
        .output()
        .expect("rev-parse");
    assert!(
        !rev_parse.status.success(),
        "branch ref deleted back to unborn"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("hello.txt")).unwrap(),
        "before-commit\n"
    );
    assert!(!repo_dir.join("new.txt").exists());

    redo(&repo, agent_actor(None)).expect("redo unborn commit");
    assert_eq!(
        run_git(&repo_dir, &["rev-parse", "refs/heads/main"]),
        c1.to_hex().to_string()
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("hello.txt")).unwrap(),
        "committed\n"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("new.txt")).unwrap(),
        "added\n"
    );
}

#[test]
fn fuse_fast_path_and_workspace_state_sync_and_dirty_override() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("app");
    let (_, head_oid) = init_repo_with_commit(
        &repo_dir,
        &[
            (".gitignore", "*.log\n"),
            ("src/clean.rs", "clean\n"),
            ("src/mod.rs", "old\n"),
            ("src/deleted.rs", "gone\n"),
            ("opaque_dir/old_a.rs", "old-a\n"),
            ("opaque_dir/sub/old_b.rs", "old-b\n"),
        ],
    );
    run_git(&repo_dir, &["branch", "feat/ws"]);

    let layout = OzoneLayout::from_git_common_dir(repo_dir.join(".git"));
    layout.ensure_initialized().expect("init ozone");

    let ws_name = "agent-fuse";
    let ws_workdir = tmp.path().join("app.oz").join(ws_name);
    let upper_dir = layout.ws_upper_dir(ws_name);
    let meta_dir = layout.ws_tombstone_dir(ws_name);
    fs::create_dir_all(ws_workdir.join("src")).expect("mkdir ws");
    fs::create_dir_all(ws_workdir.join("opaque_dir")).expect("mkdir ws opaque");
    fs::create_dir_all(upper_dir.join("src")).expect("mkdir upper");
    fs::create_dir_all(upper_dir.join("opaque_dir")).expect("mkdir upper opaque");
    fs::create_dir_all(meta_dir.join("whiteouts/src")).expect("mkdir whiteouts");
    fs::create_dir_all(meta_dir.join("opaque")).expect("mkdir opaque");

    // Populate `upper_dir` with:
    // - clean file matching HEAD (must NOT be flagged dirty)
    // - ignored file `debug.log` (must NOT be flagged dirty)
    // - modified `src/mod.rs`
    // - added `src/added.rs`
    // - opaque replacement `opaque_dir/fresh.rs` + `meta/opaque/opaque_dir.oz_opaque`
    // - whiteout `meta/whiteouts/src/deleted.rs.oz_wh`
    fs::write(upper_dir.join("src/clean.rs"), "clean\n").expect("clean in upper");
    fs::write(upper_dir.join("debug.log"), "ignored log\n").expect("ignored in upper");
    fs::write(upper_dir.join("src/mod.rs"), "new\n").expect("mod in upper");
    fs::write(upper_dir.join("src/added.rs"), "added\n").expect("added in upper");
    fs::write(upper_dir.join("opaque_dir/fresh.rs"), "fresh\n").expect("opaque fresh");
    fs::write(meta_dir.join("whiteouts/src/deleted.rs.oz_wh"), b"").expect("whiteout");
    fs::write(meta_dir.join("opaque/opaque_dir.oz_opaque"), b"").expect("opaque marker");

    // Mirror workdir for restore test.
    fs::write(ws_workdir.join("src/mod.rs"), "new\n").expect("ws mod");
    fs::write(ws_workdir.join("src/added.rs"), "added\n").expect("ws added");
    fs::write(ws_workdir.join("opaque_dir/fresh.rs"), "fresh\n").expect("ws fresh");

    let mount_record = MountRecord {
        schema_version: MountRecord::CURRENT_SCHEMA_VERSION,
        workspace: ws_name.to_string(),
        branch: "feat/ws".to_string(),
        backend: BackendKind::LinuxFuse3,
        repo_root: repo_dir.clone(),
        git_dir: repo_dir.join(".git"),
        mount_point: ws_workdir.clone(),
        upper_dir: Some(upper_dir.clone()),
        meta_dir: Some(meta_dir.clone()),
        base_tree: None,
        base_commit: Some(head_oid.clone()),
        socket_path: None,
        daemon_pid: None,
        mounted: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    fs::create_dir_all(layout.ws_meta_dir(ws_name)).expect("mkdir ws meta");
    fs::write(
        layout.ws_mount_record_path(ws_name),
        serde_json::to_string_pretty(&mount_record).unwrap(),
    )
    .expect("write mount.json");

    let ws_state = WorkspaceState {
        schema_version: WorkspaceState::CURRENT_SCHEMA_VERSION,
        name: ws_name.to_string(),
        branch: "feat/ws".to_string(),
        base_branch: Some("main".to_string()),
        base_commit: Some(head_oid.clone()),
        base_tree: head_oid.clone(),
        backend: BackendKind::LinuxFuse3,
        worktree_path: ws_workdir.clone(),
        git_worktree_dir: None,
        baseline_id: None,
        dirty_paths: BTreeMap::new(),
        pinned_paths: BTreeSet::default(),
        scope: Vec::new(),
        deny: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        last_checkpoint_oid: None,
    };
    fs::write(
        layout.workspace_state_path(ws_name),
        serde_json::to_string_pretty(&ws_state).unwrap(),
    )
    .expect("write state");

    let ws_repo = OzoneRepo::discover(Some(&ws_workdir)).expect("discover ws");
    let cp = checkpoint(
        &ws_repo,
        "oz checkpoint",
        Some("fuse snap"),
        agent_actor(Some(ws_name)),
        None,
    )
    .expect("fuse checkpoint");

    let snap = &cp.entry.ws_snapshots[ws_name];
    let dirty_map: BTreeMap<&str, &DirtyFileState> = snap
        .dirty_files
        .iter()
        .map(|d| (d.path.as_str(), &d.state))
        .collect();
    assert!(
        !dirty_map.contains_key("src/clean.rs"),
        "clean upper file ignored"
    );
    assert!(
        !dirty_map.contains_key("debug.log"),
        "gitignored upper file ignored"
    );
    assert!(matches!(
        dirty_map.get("src/mod.rs"),
        Some(DirtyFileState::Present { .. })
    ));
    assert!(matches!(
        dirty_map.get("src/added.rs"),
        Some(DirtyFileState::Present { .. })
    ));
    assert!(matches!(
        dirty_map.get("opaque_dir/fresh.rs"),
        Some(DirtyFileState::Present { .. })
    ));
    assert_eq!(
        dirty_map.get("src/deleted.rs"),
        Some(&&DirtyFileState::Deleted)
    );
    assert_eq!(
        dirty_map.get("opaque_dir/old_a.rs"),
        Some(&&DirtyFileState::Deleted)
    );
    assert_eq!(
        dirty_map.get("opaque_dir/sub/old_b.rs"),
        Some(&&DirtyFileState::Deleted)
    );

    // Check `.git/ozone/state/<ws>.json` got updated with `last_checkpoint_oid` & `dirty_paths`.
    let updated_state: WorkspaceState =
        serde_json::from_str(&fs::read_to_string(layout.workspace_state_path(ws_name)).unwrap())
            .unwrap();
    assert_eq!(
        updated_state.last_checkpoint_oid.as_deref(),
        Some(cp.dirty_tree.as_str())
    );
    assert!(updated_state.dirty_paths.contains_key("src/mod.rs"));
    assert_eq!(
        updated_state.dirty_paths["src/deleted.rs"].kind,
        ChangeKind::Deleted
    );

    // Also test explicit `dirty_override` path.
    fs::write(ws_workdir.join("override.txt"), "via-override\n").expect("write override");
    let override_changes = vec![FileChange {
        id: "f1".to_string(),
        path: "override.txt".to_string(),
        old_path: None,
        kind: ChangeKind::Added,
        additions: 1,
        deletions: 0,
        is_binary: false,
        pinned: false,
        out_of_scope: false,
        hunks: Vec::new(),
    }];
    let cp_override = checkpoint(
        &ws_repo,
        "oz checkpoint override",
        None,
        agent_actor(Some(ws_name)),
        Some(&override_changes),
    )
    .expect("override checkpoint");
    assert_eq!(cp_override.dirty_file_count, 1);
    assert_eq!(
        cp_override.entry.ws_snapshots[ws_name].dirty_files[0].path,
        "override.txt"
    );
}

#[test]
fn workspace_isolation_undo_in_one_workspace_does_not_touch_another() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    let (main_repo, head_oid) = init_repo_with_commit(&repo_dir, &[("shared.txt", "base\n")]);
    run_git(&repo_dir, &["branch", "feat/a"]);
    run_git(&repo_dir, &["branch", "feat/b"]);

    let layout = &main_repo.layout;
    layout.ensure_initialized().expect("init");

    let first_ws_dir = tmp.path().join("repo.oz/ws-a");
    let second_ws_dir = tmp.path().join("repo.oz/ws-b");
    fs::create_dir_all(&first_ws_dir).unwrap();
    fs::create_dir_all(&second_ws_dir).unwrap();
    fs::write(first_ws_dir.join("shared.txt"), "base\n").unwrap();
    fs::write(second_ws_dir.join("shared.txt"), "base\n").unwrap();

    for (name, branch, dir) in [
        ("ws-a", "feat/a", &first_ws_dir),
        ("ws-b", "feat/b", &second_ws_dir),
    ] {
        let st = WorkspaceState {
            schema_version: WorkspaceState::CURRENT_SCHEMA_VERSION,
            name: name.to_string(),
            branch: branch.to_string(),
            base_branch: Some("main".to_string()),
            base_commit: Some(head_oid.clone()),
            base_tree: head_oid.clone(),
            backend: BackendKind::CasHardlink,
            worktree_path: dir.clone(),
            git_worktree_dir: None,
            baseline_id: None,
            dirty_paths: BTreeMap::new(),
            pinned_paths: BTreeSet::default(),
            scope: Vec::new(),
            deny: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_checkpoint_oid: None,
        };
        fs::write(
            layout.workspace_state_path(name),
            serde_json::to_string_pretty(&st).unwrap(),
        )
        .unwrap();
    }

    let repo_a = OzoneRepo::discover(Some(&first_ws_dir)).unwrap();
    let repo_b = OzoneRepo::discover(Some(&second_ws_dir)).unwrap();

    // Workspace A checkpoints base, then modifies shared.txt.
    checkpoint(&repo_a, "cp a", None, agent_actor(Some("ws-a")), None).unwrap();
    fs::write(first_ws_dir.join("shared.txt"), "edited-a\n").unwrap();

    // Workspace B checkpoints its own edit afterwards.
    fs::write(second_ws_dir.join("shared.txt"), "edited-b\n").unwrap();
    checkpoint(&repo_b, "cp b", None, agent_actor(Some("ws-b")), None).unwrap();

    // Undo in workspace A ONLY undoes workspace A, leaving workspace B untouched.
    undo(&repo_a, 1usize, agent_actor(Some("ws-a"))).unwrap();
    assert_eq!(
        fs::read_to_string(first_ws_dir.join("shared.txt")).unwrap(),
        "base\n"
    );
    assert_eq!(
        fs::read_to_string(second_ws_dir.join("shared.txt")).unwrap(),
        "edited-b\n"
    );
}

#[test]
fn sub_10ms_checkpoint_undo_redo_on_1500_file_repo() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("big-repo");
    fs::create_dir_all(&repo_dir).expect("mkdir");
    run_git(&repo_dir, &["init", "-b", "main"]);
    run_git(&repo_dir, &["config", "user.name", "Ozone Test"]);
    run_git(&repo_dir, &["config", "user.email", "test@ozone.dev"]);
    run_git(&repo_dir, &["config", "core.autocrlf", "false"]);

    // Create 1,500 tracked files across 30 directories.
    for d in 0..30 {
        let sub = repo_dir.join(format!("pkg/mod_{d:02}"));
        fs::create_dir_all(&sub).expect("mkdir mod");
        for f in 0..50 {
            let file_path = sub.join(format!("file_{f:02}.rs"));
            fs::write(
                &file_path,
                format!("pub const V_{d}_{f}: usize = {};\n", d * 50 + f),
            )
            .expect("write tracked");
        }
    }
    run_git(&repo_dir, &["add", "."]);
    run_git(&repo_dir, &["commit", "-m", "seed 1500 files"]);

    // Touch 3 files and check timing with `dirty_override` (O(dirty_files) CLI fast path)
    // AND check timing of `Mutate` + `undo` + `redo`!
    let repo = OzoneRepo::discover(Some(&repo_dir)).expect("discover big repo");
    fs::write(
        repo_dir.join("pkg/mod_05/file_10.rs"),
        "pub const EDITED: usize = 999;\n",
    )
    .expect("edit 1");
    fs::write(
        repo_dir.join("pkg/mod_12/new_file.rs"),
        "pub fn added() {}\n",
    )
    .expect("add 1");

    let dirty_override = vec![
        FileChange {
            id: "f1".to_string(),
            path: "pkg/mod_05/file_10.rs".to_string(),
            old_path: None,
            kind: ChangeKind::Modified,
            additions: 1,
            deletions: 1,
            is_binary: false,
            pinned: false,
            out_of_scope: false,
            hunks: Vec::new(),
        },
        FileChange {
            id: "f2".to_string(),
            path: "pkg/mod_12/new_file.rs".to_string(),
            old_path: None,
            kind: ChangeKind::Added,
            additions: 1,
            deletions: 0,
            is_binary: false,
            pinned: false,
            out_of_scope: false,
            hunks: Vec::new(),
        },
    ];

    let t_cp = Instant::now();
    let cp = checkpoint(
        &repo,
        "oz checkpoint",
        Some("fast"),
        agent_actor(None),
        Some(&dirty_override),
    )
    .expect("checkpoint");
    let cp_elapsed = t_cp.elapsed();

    // Commit 1 file via `gix`, record mutation with `dirty_override` / transaction, then time `undo` & `redo`!
    let tx = oz_oplog::MutationTransaction::begin(
        &repo,
        "oz commit -m fast",
        agent_actor(None),
        Some(&dirty_override),
    )
    .expect("begin tx");
    let _c2 = commit_via_gix(
        &repo_dir,
        "main",
        "fast commit",
        &[(
            "pkg/mod_05/file_10.rs",
            "pub const COMMITTED: usize = 777;\n",
        )],
        &[],
    );
    let remaining_dirty = vec![dirty_override[1].clone()];
    let m1 = tx.finish(&repo, Some(&remaining_dirty)).expect("finish tx");

    let t_undo = Instant::now();
    undo(&repo, m1.op_id.as_str(), agent_actor(None)).expect("undo");
    let undo_elapsed = t_undo.elapsed();

    let t_redo = Instant::now();
    redo(&repo, agent_actor(None)).expect("redo");
    let redo_elapsed = t_redo.elapsed();

    eprintln!(
        "1500-file timings: checkpoint={:?}, undo={:?}, redo={:?}, cp_op={}",
        cp_elapsed, undo_elapsed, redo_elapsed, cp.op_id
    );

    let budget = sub10ms_budget();
    assert!(
        cp_elapsed < budget,
        "checkpoint took {cp_elapsed:?} (>= {budget:?})"
    );
    assert!(
        undo_elapsed < budget,
        "undo took {undo_elapsed:?} (>= {budget:?})"
    );
    assert!(
        redo_elapsed < budget,
        "redo took {redo_elapsed:?} (>= {budget:?})"
    );

    // Verify self-contained FUSE fast-path checkpoint on 1,500-file base_tree (without `dirty_override`) is also < 10 ms!
    let ws_name = "fuse-1500";
    let upper_dir = repo.layout.ws_upper_dir(ws_name);
    let meta_dir = repo.layout.ws_tombstone_dir(ws_name);
    let ws_workdir = tmp.path().join("big-repo.oz").join(ws_name);
    fs::create_dir_all(upper_dir.join("pkg/mod_03")).unwrap();
    fs::create_dir_all(meta_dir.join("whiteouts")).unwrap();
    fs::create_dir_all(&ws_workdir).unwrap();
    fs::write(upper_dir.join("pkg/mod_03/file_01.rs"), "dirty fuse\n").unwrap();
    let head_hex = run_git(&repo_dir, &["rev-parse", "HEAD"]);
    let mr = MountRecord {
        schema_version: MountRecord::CURRENT_SCHEMA_VERSION,
        workspace: ws_name.to_string(),
        branch: "main".to_string(),
        backend: BackendKind::LinuxFuse3,
        repo_root: repo_dir.clone(),
        git_dir: repo_dir.join(".git"),
        mount_point: ws_workdir.clone(),
        upper_dir: Some(upper_dir),
        meta_dir: Some(meta_dir),
        base_tree: None,
        base_commit: Some(head_hex),
        socket_path: None,
        daemon_pid: None,
        mounted: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    fs::create_dir_all(repo.layout.ws_meta_dir(ws_name)).unwrap();
    fs::write(
        repo.layout.ws_mount_record_path(ws_name),
        serde_json::to_string_pretty(&mr).unwrap(),
    )
    .unwrap();
    let fuse_repo = OzoneRepo::discover(Some(&ws_workdir)).unwrap();

    let t_fuse_cp = Instant::now();
    let fuse_cp = checkpoint(
        &fuse_repo,
        "oz checkpoint",
        None,
        agent_actor(Some(ws_name)),
        None,
    )
    .expect("fuse cp");
    let fuse_cp_elapsed = t_fuse_cp.elapsed();
    assert_eq!(fuse_cp.dirty_file_count, 1);

    // Edit `upper_dir` (simulating FUSE write after checkpoint) and verify FUSE undo/redo < 10 ms.
    let fuse_upper = repo.layout.ws_upper_dir(ws_name);
    fs::write(fuse_upper.join("pkg/mod_03/file_01.rs"), "dirty fuse v2\n").unwrap();

    let t_fuse_undo = Instant::now();
    undo(&fuse_repo, 1usize, agent_actor(Some(ws_name))).expect("fuse undo");
    let fuse_undo_elapsed = t_fuse_undo.elapsed();
    assert_eq!(
        fs::read_to_string(ws_workdir.join("pkg/mod_03/file_01.rs")).unwrap(),
        "dirty fuse\n"
    );

    // Sync upper_dir to match restored workdir before redo test.
    fs::write(fuse_upper.join("pkg/mod_03/file_01.rs"), "dirty fuse\n").unwrap();
    let t_fuse_redo = Instant::now();
    redo(&fuse_repo, agent_actor(Some(ws_name))).expect("fuse redo");
    let fuse_redo_elapsed = t_fuse_redo.elapsed();
    assert_eq!(
        fs::read_to_string(ws_workdir.join("pkg/mod_03/file_01.rs")).unwrap(),
        "dirty fuse v2\n"
    );

    eprintln!(
        "1500-file FUSE fast-path: checkpoint={fuse_cp_elapsed:?}, undo={fuse_undo_elapsed:?}, redo={fuse_redo_elapsed:?}"
    );
    assert!(
        fuse_cp_elapsed < budget,
        "FUSE fast-path checkpoint took {fuse_cp_elapsed:?} (>= {budget:?})"
    );
    assert!(
        fuse_undo_elapsed < budget,
        "FUSE fast-path undo took {fuse_undo_elapsed:?} (>= {budget:?})"
    );
    assert!(
        fuse_redo_elapsed < budget,
        "FUSE fast-path redo took {fuse_redo_elapsed:?} (>= {budget:?})"
    );
}

#[test]
fn eight_thread_concurrent_oplog_append_stress_test() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("concurrent");
    let (repo, _) = init_repo_with_commit(&repo_dir, &[("README.md", "hi\n")]);
    repo.layout.ensure_initialized().expect("init ozone");

    let ops_per_thread = 10usize;
    let thread_count = 8usize;
    let total = thread_count * ops_per_thread;

    let created_ids: Vec<String> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(thread_count);
        for t in 0..thread_count {
            let layout = repo.layout.clone();
            handles.push(scope.spawn(move || {
                let mut ids = Vec::with_capacity(ops_per_thread);
                for i in 0..ops_per_thread {
                    let op_id = new_op_id();
                    let entry = OpLogEntry {
                        op_id: op_id.clone(),
                        parents: Vec::new(),
                        timestamp: Utc::now(),
                        actor: ActorInfo {
                            kind: ActorKind::Agent,
                            client: Some("stress".to_string()),
                            workspace: Some(format!("ws-{t}")),
                        },
                        kind: OpKind::Mutate,
                        command: format!("oz commit --worker {t} --step {i}"),
                        message: None,
                        inverse_of: None,
                        undone_ops: Vec::new(),
                        rewound_uncheckpointed: false,
                        ref_deltas: BTreeMap::new(),
                        ws_snapshots: BTreeMap::new(),
                    };
                    let stored = append_entry(&layout, entry).expect("0 lock errors");
                    ids.push(stored.op_id);
                }
                ids
            }));
        }
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("thread join"))
            .collect()
    });

    assert_eq!(created_ids.len(), total);
    let unique_created: HashSet<&str> = created_ids.iter().map(String::as_str).collect();
    assert_eq!(unique_created.len(), total, "all op_ids unique");

    let listed = list_entries(&repo.layout).expect("list entries");
    assert_eq!(
        listed.len(),
        total,
        "all {total} concurrent entries discoverable from HEAD"
    );

    let head = read_head_op_id(&repo.layout)
        .expect("read head")
        .expect("head present");
    assert_eq!(listed.last().expect("last").op_id, head);

    for id in &created_ids {
        let loaded = load_entry(&repo.layout, id).expect("load entry");
        assert_eq!(&loaded.op_id, id);
    }
}

#[test]
fn eight_thread_concurrent_checkpoints_across_workspaces_zero_errors() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("multi-cp");
    let (main_repo, head_oid) = init_repo_with_commit(&repo_dir, &[("README.md", "hi\n")]);
    main_repo.layout.ensure_initialized().expect("init ozone");

    let mut repos = Vec::with_capacity(8);
    for t in 0..8 {
        let ws = format!("agent-{t}");
        let branch = format!("feat/agent-{t}");
        run_git(&repo_dir, &["branch", &branch]);
        let ws_dir = tmp.path().join("multi-cp.oz").join(&ws);
        fs::create_dir_all(&ws_dir).unwrap();
        fs::write(ws_dir.join("README.md"), format!("hi from {ws}\n")).unwrap();
        let st = WorkspaceState {
            schema_version: WorkspaceState::CURRENT_SCHEMA_VERSION,
            name: ws.clone(),
            branch,
            base_branch: Some("main".to_string()),
            base_commit: Some(head_oid.clone()),
            base_tree: head_oid.clone(),
            backend: BackendKind::CasHardlink,
            worktree_path: ws_dir.clone(),
            git_worktree_dir: None,
            baseline_id: None,
            dirty_paths: BTreeMap::new(),
            pinned_paths: BTreeSet::default(),
            scope: Vec::new(),
            deny: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_checkpoint_oid: None,
        };
        fs::write(
            main_repo.layout.workspace_state_path(&ws),
            serde_json::to_string_pretty(&st).unwrap(),
        )
        .unwrap();
        repos.push((ws, OzoneRepo::discover(Some(&ws_dir)).unwrap()));
    }

    let op_ids: Vec<String> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(8);
        for (ws, ws_repo) in &repos {
            handles.push(scope.spawn(move || {
                let outcome = checkpoint(
                    ws_repo,
                    "oz checkpoint",
                    Some("parallel"),
                    agent_actor(Some(ws)),
                    None,
                )
                .expect("concurrent checkpoint succeeds");
                outcome.op_id
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect()
    });

    assert_eq!(op_ids.len(), 8);
    let listed = list_entries(&main_repo.layout).expect("list entries");
    assert_eq!(listed.len(), 8);
    let listed_ids: HashSet<&str> = listed.iter().map(|e| e.op_id.as_str()).collect();
    for id in &op_ids {
        assert!(listed_ids.contains(id.as_str()));
    }
}

#[cfg(unix)]
#[test]
fn preserves_executable_modes_and_symlinks_across_checkpoint_undo_redo() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("perms");
    let (repo, _) = init_repo_with_commit(&repo_dir, &[("run.sh", "#!/bin/sh\necho 1\n")]);

    let script = repo_dir.join("run.sh");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let link_path = repo_dir.join("current.sh");
    symlink("run.sh", &link_path).unwrap();

    checkpoint(&repo, "oz checkpoint", None, agent_actor(None), None).expect("cp");

    // Mutate permissions + replace symlink with regular file.
    fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
    fs::remove_file(&link_path).unwrap();
    fs::write(&link_path, "not a symlink").unwrap();

    undo(&repo, 1usize, agent_actor(None)).expect("undo");
    assert_eq!(
        fs::metadata(&script).unwrap().permissions().mode() & 0o111,
        0o111
    );
    let link_meta = fs::symlink_metadata(&link_path).unwrap();
    assert!(link_meta.file_type().is_symlink());
    assert_eq!(fs::read_link(&link_path).unwrap(), Path::new("run.sh"));

    redo(&repo, agent_actor(None)).expect("redo");
    assert_eq!(
        fs::metadata(&script).unwrap().permissions().mode() & 0o111,
        0
    );
    assert!(
        !fs::symlink_metadata(&link_path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&link_path).unwrap(), "not a symlink");
}

#[test]
fn error_paths_empty_oplog_enotconn_and_nothing_to_redo() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("errs");
    let (mut repo, _) = init_repo_with_commit(&repo_dir, &[("a.txt", "a\n")]);

    let err_empty = undo(&repo, 1usize, agent_actor(None)).expect_err("empty undo");
    assert!(matches!(err_empty, OzError::InvalidUsage { .. }));

    let err_redo = redo(&repo, agent_actor(None)).expect_err("empty redo");
    assert!(matches!(err_redo, OzError::InvalidUsage { .. }));

    let err_zero = undo(&repo, UndoTarget::Steps(0), agent_actor(None)).expect_err("steps 0");
    assert!(matches!(err_zero, OzError::InvalidUsage { .. }));

    repo.mount_health = oz_core::MountHealth::DisconnectedEnotconn;
    let err_mount = checkpoint(&repo, "oz checkpoint", None, agent_actor(None), None)
        .expect_err("enotconn checkpoint");
    assert!(matches!(err_mount, OzError::MountUnavailable { .. }));
}

#[test]
fn concurrent_record_mutation_isolates_ref_deltas_per_workspace() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("iso-refs");
    let (main_repo, c0_hex) = init_repo_with_commit(&repo_dir, &[("shared.txt", "base\n")]);
    main_repo.layout.ensure_initialized().expect("init ozone");

    run_git(&repo_dir, &["branch", "feat/a"]);
    run_git(&repo_dir, &["branch", "feat/b"]);

    let first_ws_dir = tmp.path().join("iso-refs.oz/ws-a");
    let second_ws_dir = tmp.path().join("iso-refs.oz/ws-b");
    fs::create_dir_all(&first_ws_dir).unwrap();
    fs::create_dir_all(&second_ws_dir).unwrap();
    fs::write(first_ws_dir.join("shared.txt"), "base\n").unwrap();
    fs::write(second_ws_dir.join("shared.txt"), "base\n").unwrap();

    for (name, branch, dir) in [
        ("ws-a", "feat/a", &first_ws_dir),
        ("ws-b", "feat/b", &second_ws_dir),
    ] {
        let st = WorkspaceState {
            schema_version: WorkspaceState::CURRENT_SCHEMA_VERSION,
            name: name.to_string(),
            branch: branch.to_string(),
            base_branch: Some("main".to_string()),
            base_commit: Some(c0_hex.clone()),
            base_tree: c0_hex.clone(),
            backend: BackendKind::CasHardlink,
            worktree_path: dir.clone(),
            git_worktree_dir: None,
            baseline_id: None,
            dirty_paths: BTreeMap::new(),
            pinned_paths: BTreeSet::default(),
            scope: Vec::new(),
            deny: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_checkpoint_oid: None,
        };
        fs::write(
            main_repo.layout.workspace_state_path(name),
            serde_json::to_string_pretty(&st).unwrap(),
        )
        .unwrap();
    }

    let repo_a = OzoneRepo::discover(Some(&first_ws_dir)).unwrap();
    let repo_b = OzoneRepo::discover(Some(&second_ws_dir)).unwrap();

    // Workspace A opens a mutation transaction.
    let tx_a = oz_oplog::MutationTransaction::begin(
        &repo_a,
        "oz commit -m feat-a",
        agent_actor(Some("ws-a")),
        None,
    )
    .expect("begin tx_a");

    // While ws-a is in-flight, ws-b concurrently commits on feat/b!
    let (cb1_id, entry_b) = record_mutation(
        &repo_b,
        "oz commit -m feat-b",
        agent_actor(Some("ws-b")),
        None,
        || {
            let oid = commit_via_gix(&repo_dir, "feat/b", "feat-b", &[("b.txt", "b1\n")], &[]);
            fs::write(second_ws_dir.join("b.txt"), "b1\n").unwrap();
            Ok(oid)
        },
    )
    .expect("record ws-b mutation");
    let cb1_hex = cb1_id.to_hex().to_string();
    assert!(entry_b.ref_deltas.contains_key("refs/heads/feat/b"));
    assert!(!entry_b.ref_deltas.contains_key("refs/heads/feat/a"));

    // Now ws-a commits on feat/a and finishes its transaction.
    let feat_a_oid = commit_via_gix(&repo_dir, "feat/a", "feat-a", &[("a.txt", "a1\n")], &[]);
    fs::write(first_ws_dir.join("a.txt"), "a1\n").unwrap();
    let entry_a = tx_a.finish(&repo_a, None).expect("finish tx_a");

    assert_eq!(
        entry_a.ref_deltas.keys().collect::<Vec<_>>(),
        vec!["refs/heads/feat/a"],
        "ws-a ref_deltas must NOT include refs/heads/feat/b"
    );
    assert_eq!(
        entry_a.ref_deltas["refs/heads/feat/a"].old.as_deref(),
        Some(c0_hex.as_str())
    );
    assert_eq!(
        entry_a.ref_deltas["refs/heads/feat/a"].new.as_deref(),
        Some(feat_a_oid.to_hex().to_string().as_str())
    );

    // Undoing ws-a rolls back feat/a without touching or conflicting with feat/b!
    undo(&repo_a, 1usize, agent_actor(Some("ws-a"))).expect("undo ws-a");
    assert_eq!(
        run_git(&repo_dir, &["rev-parse", "refs/heads/feat/a"]),
        c0_hex
    );
    assert_eq!(
        run_git(&repo_dir, &["rev-parse", "refs/heads/feat/b"]),
        cb1_hex
    );
    assert!(!first_ws_dir.join("a.txt").exists());
    assert_eq!(
        fs::read_to_string(second_ws_dir.join("b.txt")).unwrap(),
        "b1\n"
    );
}

#[test]
fn fuse_fast_path_prunes_ignored_dir_with_many_files_under_10ms() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("fuse-ignore");
    let (_, head_oid) = init_repo_with_commit(
        &repo_dir,
        &[
            (".gitignore", "target/\n*.log\n"),
            ("src/lib.rs", "pub fn ok() -> bool { true }\n"),
        ],
    );
    run_git(&repo_dir, &["branch", "feat/fuse-prune"]);

    let layout = OzoneLayout::from_git_common_dir(repo_dir.join(".git"));
    layout.ensure_initialized().expect("init ozone");

    let ws_name = "agent-prune";
    let ws_workdir = tmp.path().join("fuse-ignore.oz").join(ws_name);
    let upper_dir = layout.ws_upper_dir(ws_name);
    let meta_dir = layout.ws_tombstone_dir(ws_name);

    fs::create_dir_all(&ws_workdir).unwrap();
    fs::create_dir_all(upper_dir.join("src")).unwrap();
    fs::create_dir_all(&meta_dir).unwrap();

    // Populate 2,000 ignored build artifacts inside `upper/target/debug/deps/`.
    let target_deps = upper_dir.join("target/debug/deps");
    fs::create_dir_all(&target_deps).unwrap();
    for i in 0..2000 {
        fs::write(target_deps.join(format!("crate_{i:04}.rlib")), "artifact").unwrap();
    }
    fs::write(
        upper_dir.join("src/lib.rs"),
        "pub fn ok() -> bool { false }\n",
    )
    .unwrap();

    let mr = MountRecord {
        schema_version: MountRecord::CURRENT_SCHEMA_VERSION,
        workspace: ws_name.to_string(),
        branch: "feat/fuse-prune".to_string(),
        backend: BackendKind::LinuxFuse3,
        repo_root: repo_dir.clone(),
        git_dir: repo_dir.join(".git"),
        mount_point: ws_workdir.clone(),
        upper_dir: Some(upper_dir),
        meta_dir: Some(meta_dir),
        base_tree: None,
        base_commit: Some(head_oid),
        socket_path: None,
        daemon_pid: None,
        mounted: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    fs::create_dir_all(layout.ws_meta_dir(ws_name)).unwrap();
    fs::write(
        layout.ws_mount_record_path(ws_name),
        serde_json::to_string_pretty(&mr).unwrap(),
    )
    .unwrap();

    let fuse_repo = OzoneRepo::discover(Some(&ws_workdir)).unwrap();

    let t0 = Instant::now();
    let cp = checkpoint(
        &fuse_repo,
        "oz checkpoint",
        None,
        agent_actor(Some(ws_name)),
        None,
    )
    .expect("checkpoint");
    let elapsed = t0.elapsed();

    eprintln!("FUSE checkpoint with 2000 ignored target/ files: {elapsed:?}");
    assert_eq!(cp.dirty_file_count, 1);
    assert_eq!(
        cp.entry.ws_snapshots[ws_name].dirty_files[0].path,
        "src/lib.rs"
    );
    let budget = sub10ms_budget();
    assert!(
        elapsed < budget,
        "FUSE checkpoint with 2000 ignored files took {elapsed:?} (>= {budget:?})"
    );
}

#[test]
fn windows_projfs_uses_daemon_fast_path_and_reset_hard_clean_undo_sub_10ms() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("win-projfs-oplog");
    fs::create_dir_all(&repo_dir).expect("mkdir");
    run_git(&repo_dir, &["init", "-b", "main"]);
    run_git(&repo_dir, &["config", "user.name", "Ozone Test"]);
    run_git(&repo_dir, &["config", "user.email", "test@ozone.dev"]);
    run_git(&repo_dir, &["config", "core.autocrlf", "false"]);

    // 1. Create 1,200 files for <10ms reset --hard + undo benchmark.
    fs::write(repo_dir.join(".gitignore"), "*.log\n").unwrap();
    for d in 0..24 {
        let sub = repo_dir.join(format!("mod_{d:02}"));
        fs::create_dir_all(&sub).unwrap();
        for f in 0..50 {
            fs::write(sub.join(format!("f_{f:02}.rs")), format!("// {d} {f}\n")).unwrap();
        }
    }
    run_git(&repo_dir, &["add", "."]);
    run_git(&repo_dir, &["commit", "-m", "c1"]);

    // Commit C2 modifying 1 file and adding 1 file.
    fs::write(repo_dir.join("mod_00/f_00.rs"), "// c2\n").unwrap();
    fs::write(repo_dir.join("added_in_c2.rs"), "pub fn c2() {}\n").unwrap();
    run_git(&repo_dir, &["add", "."]);
    run_git(&repo_dir, &["commit", "-m", "c2"]);
    let c2_oid = run_git(&repo_dir, &["rev-parse", "HEAD"]);

    // Dirty modified tracked file + untracked non-ignored files + ignored file.
    fs::write(repo_dir.join("mod_01/f_05.rs"), "// uncommitted edit\n").unwrap();
    fs::write(repo_dir.join("untracked_wip.txt"), "wip data\n").unwrap();
    fs::create_dir_all(repo_dir.join("untracked_dir")).unwrap();
    fs::write(repo_dir.join("untracked_dir/nested.txt"), "nested wip\n").unwrap();
    fs::write(repo_dir.join("keep.log"), "ignored log\n").unwrap();

    let repo = OzoneRepo::discover(Some(&repo_dir)).expect("discover");
    let tx = oz_oplog::MutationTransaction::begin_watching_all_branches(
        &repo,
        "oz git reset --hard HEAD~1",
        agent_actor(None),
        None,
    )
    .expect("begin tx");

    run_git(&repo_dir, &["reset", "--hard", "HEAD~1"]);
    run_git(&repo_dir, &["clean", "-fd"]);
    tx.finish(&repo, None).expect("finish tx");

    assert_ne!(run_git(&repo_dir, &["rev-parse", "HEAD"]), c2_oid);
    assert!(!repo_dir.join("added_in_c2.rs").exists());
    assert!(!repo_dir.join("untracked_wip.txt").exists());
    assert!(!repo_dir.join("untracked_dir/nested.txt").exists());
    assert_eq!(
        fs::read_to_string(repo_dir.join("mod_01/f_05.rs")).unwrap(),
        "// 1 5\n"
    );

    let t0 = Instant::now();
    undo(&repo, 1usize, agent_actor(None)).expect("undo reset+clean");
    let elapsed = t0.elapsed();
    eprintln!("1200-file reset --hard + clean -fd undo took {elapsed:?}");

    assert_eq!(run_git(&repo_dir, &["rev-parse", "HEAD"]), c2_oid);
    assert_eq!(
        fs::read_to_string(repo_dir.join("added_in_c2.rs")).unwrap(),
        "pub fn c2() {}\n"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("mod_01/f_05.rs")).unwrap(),
        "// uncommitted edit\n"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("untracked_wip.txt")).unwrap(),
        "wip data\n"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("untracked_dir/nested.txt")).unwrap(),
        "nested wip\n"
    );
    assert!(repo_dir.join("keep.log").is_file());
    let budget = sub10ms_budget();
    assert!(elapsed < budget, "undo took {elapsed:?} (>= {budget:?})");

    // 2. Verify WindowsProjFs WorkspaceState without mount.json still triggers O(dirty) fast path!
    let ws_name = "projfs-fast";
    let upper_dir = repo.layout.ws_upper_dir(ws_name);
    let meta_dir = repo.layout.ws_tombstone_dir(ws_name);
    let ws_workdir = tmp.path().join("win-projfs-oplog.oz").join(ws_name);
    fs::create_dir_all(upper_dir.join("mod_00")).unwrap();
    fs::create_dir_all(meta_dir.join("whiteouts")).unwrap();
    fs::create_dir_all(&ws_workdir).unwrap();
    fs::write(upper_dir.join("mod_00/f_01.rs"), "// projfs edit\n").unwrap();

    run_git(&repo_dir, &["branch", "feat/projfs"]);
    let st = WorkspaceState {
        schema_version: WorkspaceState::CURRENT_SCHEMA_VERSION,
        name: ws_name.to_string(),
        branch: "feat/projfs".to_string(),
        base_branch: Some("main".to_string()),
        base_commit: Some(c2_oid.clone()),
        base_tree: c2_oid,
        backend: BackendKind::WindowsProjFs,
        worktree_path: ws_workdir.clone(),
        git_worktree_dir: None,
        baseline_id: None,
        dirty_paths: BTreeMap::new(),
        pinned_paths: BTreeSet::default(),
        scope: Vec::new(),
        deny: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        last_checkpoint_oid: None,
    };
    fs::write(
        repo.layout.workspace_state_path(ws_name),
        serde_json::to_string_pretty(&st).unwrap(),
    )
    .unwrap();

    let projfs_repo = OzoneRepo::discover(Some(&ws_workdir)).unwrap();
    let cp = checkpoint(
        &projfs_repo,
        "oz checkpoint",
        None,
        agent_actor(Some(ws_name)),
        None,
    )
    .expect("projfs checkpoint");
    assert_eq!(cp.dirty_file_count, 1);
    assert_eq!(
        cp.entry.ws_snapshots[ws_name].dirty_files[0].path,
        "mod_00/f_01.rs"
    );

    // 3. Verify branch switch + remote ref snapshot undo/redo roundtrip.
    let sw_tx = oz_oplog::MutationTransaction::begin_watching_all_refs(
        &repo,
        "oz git switch -c feat/undo-sw",
        agent_actor(None),
        None,
    )
    .expect("begin switch tx");
    run_git(&repo_dir, &["checkout", "-b", "feat/undo-sw"]);
    sw_tx.finish(&repo, None).expect("finish switch tx");
    assert_eq!(
        run_git(&repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat/undo-sw"
    );

    undo(&repo, 1usize, agent_actor(None)).expect("undo switch -c");
    assert_eq!(
        run_git(&repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main"
    );
    let show_feat = Command::new("git")
        .args(["rev-parse", "--verify", "refs/heads/feat/undo-sw"])
        .current_dir(&repo_dir)
        .output()
        .unwrap();
    assert!(
        !show_feat.status.success(),
        "created branch should be deleted on undo"
    );

    redo(&repo, agent_actor(None)).expect("redo switch -c");
    assert_eq!(
        run_git(&repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat/undo-sw"
    );
}

fn backdate_file(path: &Path, secs_ago: u64) {
    let target = std::time::SystemTime::now() - Duration::from_secs(secs_ago);
    let file = fs::File::options()
        .write(true)
        .open(path)
        .expect("open file for backdate");
    file.set_modified(target).expect("set_modified");
}

fn git_status_porcelain(dir: &Path) -> String {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output()
        .expect("git status --porcelain");
    assert!(
        out.status.success(),
        "git status --porcelain failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn oz_commit_with_oplog(
    repo: &OzoneRepo,
    opts: &oz_core::CommitOptions,
) -> (oz_core::CommitOutcome, OpLogEntry) {
    let pre_status = oz_core::compute_status(repo).expect("pre status");
    let label = if opts.amend {
        "oz commit --amend"
    } else {
        "oz commit"
    };
    let tx = oz_oplog::MutationTransaction::begin(
        repo,
        label,
        agent_actor(repo.workspace.as_deref()),
        Some(&pre_status.changes),
    )
    .expect("begin commit tx");
    let outcome = oz_core::commit_changes(repo, opts).expect("commit_changes");
    let entry = if outcome.remaining_dirty_count == 0 {
        tx.finish_with_message(repo, Some(&outcome.message), Some(&[]))
            .expect("finish clean commit tx")
    } else {
        let post_status = oz_core::compute_status(repo).expect("post status");
        tx.finish_with_message(repo, Some(&outcome.message), Some(&post_status.changes))
            .expect("finish partial commit tx")
    };
    (outcome, entry)
}

#[test]
fn undo_after_commit_preserves_dirty_visibility_for_same_and_diff_size_edits_and_redo() {
    use gix::bstr::ByteSlice as _;

    for (label, v2_content) in [
        ("same-size", "world\n"),
        ("different-size", "world-much-longer-edited-content\n"),
    ] {
        let tmp = TempDir::new().expect("tempdir");
        let repo_dir = tmp.path().join(format!("repo-{label}"));
        fs::create_dir_all(&repo_dir).expect("mkdir");
        run_git(&repo_dir, &["init", "-b", "main"]);
        run_git(&repo_dir, &["config", "user.name", "Ozone Test"]);
        run_git(&repo_dir, &["config", "user.email", "test@ozone.dev"]);
        run_git(&repo_dir, &["config", "core.autocrlf", "false"]);

        let repo = OzoneRepo::discover(Some(&repo_dir)).expect("discover unborn");

        // Step a: create `a.txt` and commit via `oz commit` flow.
        fs::write(repo_dir.join("a.txt"), "hello\n").expect("write v1");
        backdate_file(&repo_dir.join("a.txt"), 20);
        let (c1, _) = oz_commit_with_oplog(&repo, &oz_core::CommitOptions::new("c1"));
        let head_c1_blob = run_git(&repo_dir, &["rev-parse", "HEAD:a.txt"]);

        // Step b: edit `a.txt`, backdate mtime to stay strictly older than `.git/index`
        // (bypassing racy-git just like real user edits), and commit again.
        fs::write(repo_dir.join("a.txt"), v2_content).expect("write v2");
        backdate_file(&repo_dir.join("a.txt"), 10);
        let (c2, _) = oz_commit_with_oplog(&repo, &oz_core::CommitOptions::new("c2"));
        let head_c2_blob = run_git(&repo_dir, &["rev-parse", "HEAD:a.txt"]);
        assert_ne!(head_c1_blob, head_c2_blob, "{label}: blobs must differ");

        // Step c: run `oz undo`.
        let undo_out = undo(&repo, 1usize, agent_actor(None)).expect("undo c2");
        assert_eq!(
            undo_out.restored_files, 0,
            "{label}: disk already matches pre-commit edit so 0 files needed rewriting"
        );
        assert_eq!(run_git(&repo_dir, &["rev-parse", "HEAD"]), c1.commit_oid);
        assert_eq!(
            fs::read_to_string(repo_dir.join("a.txt")).unwrap(),
            v2_content,
            "{label}: working tree keeps pre-commit edit"
        );
        assert_eq!(
            run_git(&repo_dir, &["show", "HEAD:a.txt"]),
            "hello",
            "{label}: HEAD restored to c1"
        );

        // Verify `.git/index` entry has old blob OID with ZEROED stat (not edited file's stat!).
        let gix_repo = gix::open(&repo_dir).expect("open gix");
        let idx = gix_repo.open_index().expect("open index");
        let entry = idx
            .entry_by_path("a.txt".as_bytes().as_bstr())
            .expect("a.txt in index");
        assert_eq!(entry.id.to_hex().to_string(), head_c1_blob);
        assert_eq!(
            entry.stat,
            gix::index::entry::Stat::default(),
            "{label}: .git/index must not pair old blob with edited file's stat"
        );
        let ls_debug = run_git(&repo_dir, &["ls-files", "--debug", "-s", "a.txt"]);
        assert!(
            ls_debug.contains(&head_c1_blob) && ls_debug.contains("size: 0"),
            "{label}: unexpected git ls-files --debug output:\n{ls_debug}"
        );

        // BOTH `oz status` and `git status --porcelain` MUST report `a.txt` as modified!
        let oz_st = oz_core::compute_status(&repo).expect("oz status after undo");
        assert_eq!(
            oz_st.changes.len(),
            1,
            "{label}: oz status hid modified a.txt after oz undo: {oz_st:?}"
        );
        assert_eq!(oz_st.changes[0].path, "a.txt");
        assert_eq!(oz_st.changes[0].kind, ChangeKind::Modified);

        assert_eq!(
            git_status_porcelain(&repo_dir),
            " M a.txt\n",
            "{label}: git status --porcelain disagreed after oz undo"
        );
        assert_eq!(
            run_git(&repo_dir, &["diff", "--cached", "--name-only"]),
            "",
            "{label}: nothing should be staged after oz undo"
        );

        // Step d: `oz redo` restores commit c2 cleanly and both statuses agree on clean!
        redo(&repo, agent_actor(None)).expect("redo c2");
        assert_eq!(run_git(&repo_dir, &["rev-parse", "HEAD"]), c2.commit_oid);
        assert_eq!(
            fs::read_to_string(repo_dir.join("a.txt")).unwrap(),
            v2_content
        );
        let oz_st_redo = oz_core::compute_status(&repo).expect("oz status after redo");
        assert!(
            oz_st_redo.is_clean(),
            "{label}: expected clean oz status after redo: {oz_st_redo:?}"
        );
        assert_eq!(
            git_status_porcelain(&repo_dir),
            "",
            "{label}: expected clean git status after redo"
        );
        let idx_redo = gix_repo.open_index().expect("open index after redo");
        let entry_redo = idx_redo
            .entry_by_path("a.txt".as_bytes().as_bstr())
            .expect("a.txt in index after redo");
        assert_eq!(entry_redo.id.to_hex().to_string(), head_c2_blob);
        assert_eq!(entry_redo.stat.size, u32::try_from(v2_content.len()).unwrap());

        // Step e: second `oz undo` followed by re-committing via `oz commit` must work cleanly!
        undo(&repo, 1usize, agent_actor(None)).expect("second undo");
        let oz_st_undo2 = oz_core::compute_status(&repo).expect("oz status after second undo");
        assert_eq!(oz_st_undo2.changes.len(), 1);
        assert_eq!(oz_st_undo2.changes[0].path, "a.txt");
        assert_eq!(git_status_porcelain(&repo_dir), " M a.txt\n");

        let (c2_retry, _) = oz_commit_with_oplog(&repo, &oz_core::CommitOptions::new("c2-retry"));
        assert_eq!(
            run_git(&repo_dir, &["rev-parse", "HEAD"]),
            c2_retry.commit_oid
        );
        assert!(
            oz_core::compute_status(&repo)
                .expect("final oz status")
                .is_clean()
        );
        assert_eq!(git_status_porcelain(&repo_dir), "");
    }
}

#[test]
fn undo_and_redo_after_partial_hunk_commit_and_amend_keep_index_accurate() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("repo-partial-amend");
    let orig_lines = (1..=20).fold(String::new(), |mut acc, i| {
        use std::fmt::Write as _;
        let _ = writeln!(acc, "line {i}");
        acc
    });
    let (repo, c1_hex) = init_repo_with_commit(&repo_dir, &[("a.txt", &orig_lines)]);

    // Edit two separated hunks (h1a and h1b) and backdate mtime.
    let mut edited: Vec<String> = (1..=20).map(|i| format!("line {i}\n")).collect();
    edited[1] = "line 2 hunk A modified\n".to_string();
    edited[17] = "line 18 hunk B modified\n".to_string();
    let edited_content = edited.concat();
    fs::write(repo_dir.join("a.txt"), &edited_content).expect("write two hunks");
    backdate_file(&repo_dir.join("a.txt"), 10);

    // Partial hunk commit (`--pick h1a`) leaves `h1b` dirty on disk.
    let partial_opts = oz_core::CommitOptions::new("partial h1a").with_pick(["h1a"]);
    let (c2, _) = oz_commit_with_oplog(&repo, &partial_opts);
    assert_eq!(c2.remaining_dirty_count, 1);

    let st_after_partial = oz_core::compute_status(&repo).expect("status after partial");
    assert_eq!(st_after_partial.changes.len(), 1);
    assert_eq!(st_after_partial.changes[0].hunks.len(), 1);
    assert_eq!(git_status_porcelain(&repo_dir), " M a.txt\n");

    // Undo partial commit: HEAD back to c1, BOTH hunks visible in `oz status` and `git status`!
    undo(&repo, 1usize, agent_actor(None)).expect("undo partial commit");
    assert_eq!(run_git(&repo_dir, &["rev-parse", "HEAD"]), c1_hex);
    let st_after_undo = oz_core::compute_status(&repo).expect("status after undo partial");
    assert_eq!(st_after_undo.changes.len(), 1);
    assert_eq!(
        st_after_undo.changes[0].hunks.len(),
        2,
        "both hunks must be visible after undoing partial commit"
    );
    assert_eq!(git_status_porcelain(&repo_dir), " M a.txt\n");

    // Redo partial commit: HEAD back to c2, remaining hunk `h1b` MUST STILL be visible!
    redo(&repo, agent_actor(None)).expect("redo partial commit");
    assert_eq!(run_git(&repo_dir, &["rev-parse", "HEAD"]), c2.commit_oid);
    let st_after_redo = oz_core::compute_status(&repo).expect("status after redo partial");
    assert_eq!(st_after_redo.changes.len(), 1);
    assert_eq!(
        st_after_redo.changes[0].hunks.len(),
        1,
        "remaining uncommitted hunk must not be hidden after redoing partial commit"
    );
    assert_eq!(git_status_porcelain(&repo_dir), " M a.txt\n");

    // Amend c2 to include `h1b`, then undo and redo the amend.
    backdate_file(&repo_dir.join("a.txt"), 10);
    let amend_opts = oz_core::CommitOptions::new("amend full").with_amend(true);
    let (c2_amended, _) = oz_commit_with_oplog(&repo, &amend_opts);
    assert!(oz_core::compute_status(&repo).unwrap().is_clean());
    assert_eq!(git_status_porcelain(&repo_dir), "");

    undo(&repo, 1usize, agent_actor(None)).expect("undo amend");
    assert_eq!(run_git(&repo_dir, &["rev-parse", "HEAD"]), c2.commit_oid);
    let st_undo_amend = oz_core::compute_status(&repo).expect("status after undo amend");
    assert_eq!(st_undo_amend.changes.len(), 1);
    assert_eq!(st_undo_amend.changes[0].hunks.len(), 1);
    assert_eq!(git_status_porcelain(&repo_dir), " M a.txt\n");

    redo(&repo, agent_actor(None)).expect("redo amend");
    assert_eq!(
        run_git(&repo_dir, &["rev-parse", "HEAD"]),
        c2_amended.commit_oid
    );
    assert!(oz_core::compute_status(&repo).unwrap().is_clean());
    assert_eq!(git_status_porcelain(&repo_dir), "");
}

#[test]
fn undo_after_switch_force_reset_hard_and_mode_commit_keep_index_accurate() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("repo-other-ops");
    let (repo, c1_hex) = init_repo_with_commit(&repo_dir, &[("a.txt", "base-v1\n")]);

    // Create branch `feat` with different `a.txt`, then switch back to `main`.
    run_git(&repo_dir, &["checkout", "-b", "feat"]);
    fs::write(repo_dir.join("a.txt"), "feat-v2-longer\n").unwrap();
    backdate_file(&repo_dir.join("a.txt"), 20);
    run_git(&repo_dir, &["add", "a.txt"]);
    run_git(&repo_dir, &["commit", "-m", "feat c2"]);
    run_git(&repo_dir, &["checkout", "main"]);

    // 1. Dirty `a.txt` on `main`, force-switch to `feat` via `oz_core::switch_branch` + OpLog,
    //    then `oz undo` -> dirty edit MUST remain visible in both `oz status` and `git status`!
    fs::write(repo_dir.join("a.txt"), "local-dirty-work-before-switch\n").unwrap();
    backdate_file(&repo_dir.join("a.txt"), 10);
    let sw_pre = oz_core::compute_status(&repo).expect("pre switch status");
    let sw_tx = oz_oplog::MutationTransaction::begin_watching_all_branches(
        &repo,
        "oz switch -f feat",
        agent_actor(None),
        Some(&sw_pre.changes),
    )
    .expect("begin switch tx");
    let sw_opts = oz_core::SwitchOptions {
        target: Some("feat".to_string()),
        force: true,
        ..oz_core::SwitchOptions::default()
    };
    oz_core::switch_branch(&repo, &sw_opts).expect("switch -f feat");
    sw_tx.finish(&repo, None).expect("finish switch tx");
    assert_eq!(
        fs::read_to_string(repo_dir.join("a.txt")).unwrap(),
        "feat-v2-longer\n"
    );

    undo(&repo, 1usize, agent_actor(None)).expect("undo switch -f");
    assert_eq!(run_git(&repo_dir, &["rev-parse", "HEAD"]), c1_hex);
    assert_eq!(
        fs::read_to_string(repo_dir.join("a.txt")).unwrap(),
        "local-dirty-work-before-switch\n"
    );
    // Backdate restored file to ensure non-racy stat check cannot hide it.
    backdate_file(&repo_dir.join("a.txt"), 10);
    let st_after_sw_undo = oz_core::compute_status(&repo).expect("status after switch undo");
    assert_eq!(st_after_sw_undo.changes.len(), 1);
    assert_eq!(st_after_sw_undo.changes[0].path, "a.txt");
    assert_eq!(git_status_porcelain(&repo_dir), " M a.txt\n");

    redo(&repo, agent_actor(None)).expect("redo switch -f");
    let repo_feat = OzoneRepo::discover(Some(&repo_dir)).expect("discover after switch redo");
    assert_eq!(repo_feat.branch.as_deref(), Some("feat"));
    assert!(oz_core::compute_status(&repo_feat).unwrap().is_clean());
    assert_eq!(git_status_porcelain(&repo_dir), "");

    // 2. Dirty `a.txt` on `feat`, run `git reset --hard HEAD~1` wrapped in OpLog, then `oz undo`!
    fs::write(repo_dir.join("a.txt"), "wip-before-reset-hard\n").unwrap();
    backdate_file(&repo_dir.join("a.txt"), 10);
    let rst_tx = oz_oplog::MutationTransaction::begin_watching_all_branches(
        &repo_feat,
        "oz git reset --hard HEAD~1",
        agent_actor(None),
        None,
    )
    .expect("begin reset tx");
    run_git(&repo_dir, &["reset", "--hard", "HEAD~1"]);
    rst_tx.finish(&repo_feat, None).expect("finish reset tx");

    undo(&repo_feat, 1usize, agent_actor(None)).expect("undo reset --hard");
    assert_eq!(
        fs::read_to_string(repo_dir.join("a.txt")).unwrap(),
        "wip-before-reset-hard\n"
    );
    backdate_file(&repo_dir.join("a.txt"), 10);
    let st_after_rst_undo = oz_core::compute_status(&repo_feat).expect("status after reset undo");
    assert_eq!(st_after_rst_undo.changes.len(), 1);
    assert_eq!(st_after_rst_undo.changes[0].path, "a.txt");
    assert_eq!(git_status_porcelain(&repo_dir), " M a.txt\n");

    // 3. Unix executable-bit-only commit -> `oz undo` -> `oz redo`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let script = repo_dir.join("tool.sh");
        fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
        backdate_file(&script, 20);
        let _ = oz_commit_with_oplog(&repo_feat, &oz_core::CommitOptions::new("add tool.sh"));

        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        backdate_file(&script, 10);
        let _ = oz_commit_with_oplog(&repo_feat, &oz_core::CommitOptions::new("chmod +x tool.sh"));

        undo(&repo_feat, 1usize, agent_actor(None)).expect("undo chmod commit");
        let st_mode_undo = oz_core::compute_status(&repo_feat).expect("status after mode undo");
        assert_eq!(st_mode_undo.changes.len(), 1);
        assert_eq!(st_mode_undo.changes[0].path, "tool.sh");
        assert_eq!(git_status_porcelain(&repo_dir), " M tool.sh\n");

        redo(&repo_feat, agent_actor(None)).expect("redo chmod commit");
        assert!(oz_core::compute_status(&repo_feat).unwrap().is_clean());
        assert_eq!(git_status_porcelain(&repo_dir), "");
    }
}

#[test]
fn inspect_oplog_reports_cursors_statuses_limits_and_is_read_only() {
    use oz_oplog::{OpLogEntryStatus, OpLogListOptions, inspect_oplog};

    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    let (repo, _head) = init_repo_with_commit(&repo_dir, &[("a.txt", "v0\n")]);

    // 1. Fresh repo without `.git/ozone/oplog`: read-only inspection returns empty without creating files.
    assert!(!repo.layout.oplog_dir().exists());
    let empty_report = inspect_oplog(&repo, OpLogListOptions::default()).expect("inspect empty");
    assert_eq!(empty_report.entries.len(), 0);
    assert_eq!(empty_report.total_entries, 0);
    assert_eq!(empty_report.active_count, 0);
    assert_eq!(empty_report.redo_depth, 0);
    assert_eq!(empty_report.head_op_id, None);
    assert_eq!(empty_report.undo_cursor_op_id, None);
    assert_eq!(empty_report.redo_cursor_op_id, None);
    assert!(!empty_report.truncated);
    assert!(
        !repo.layout.oplog_dir().exists(),
        "inspect_oplog must not create .git/ozone/oplog on disk"
    );

    // 2. Reject limit == 0 cleanly.
    let zero_err = inspect_oplog(
        &repo,
        OpLogListOptions {
            limit: 0,
            all_workspaces: false,
        },
    )
    .expect_err("limit 0 should fail");
    assert!(matches!(zero_err, OzError::InvalidUsage { .. }));

    // 3. Record commit1 -> checkpoint1 -> commit2.
    fs::write(repo_dir.join("a.txt"), "v1\n").unwrap();
    let op_c1 = oz_commit_with_oplog(&repo, &oz_core::CommitOptions::new("feat: first commit"))
        .1
        .op_id;

    fs::write(repo_dir.join("a.txt"), "v1-wip\n").unwrap();
    let cp1 = checkpoint(
        &repo,
        "oz checkpoint",
        Some("before second"),
        agent_actor(None),
        None,
    )
    .expect("checkpoint");

    fs::write(repo_dir.join("a.txt"), "v2\n").unwrap();
    let op_c2 = oz_commit_with_oplog(&repo, &oz_core::CommitOptions::new("feat: second commit"))
        .1
        .op_id;

    let r1 = inspect_oplog(&repo, OpLogListOptions::default()).expect("inspect 3 ops");
    assert_eq!(r1.total_entries, 3);
    assert_eq!(r1.entries.len(), 3);
    assert_eq!(r1.active_count, 3);
    assert_eq!(r1.redo_depth, 0);
    assert!(!r1.truncated);
    assert_eq!(r1.head_op_id.as_deref(), Some(op_c2.as_str()));
    assert_eq!(r1.undo_cursor_op_id.as_deref(), Some(op_c2.as_str()));
    assert_eq!(r1.redo_cursor_op_id, None);
    // Newest first order: op_c2, cp1, op_c1.
    assert_eq!(r1.entries[0].op_id, op_c2);
    assert_eq!(r1.entries[0].status, OpLogEntryStatus::Active);
    assert!(r1.entries[0].is_head);
    assert!(r1.entries[0].is_next_undo);
    assert!(!r1.entries[0].is_next_redo);
    assert!(r1.entries[0].summary.contains("feat: second commit"));

    assert_eq!(r1.entries[1].op_id, cp1.op_id);
    assert_eq!(r1.entries[1].kind, OpKind::Checkpoint);
    assert_eq!(r1.entries[1].status, OpLogEntryStatus::Active);
    assert_eq!(r1.entries[1].summary, "before second");
    assert!(!r1.entries[1].is_next_undo);

    assert_eq!(r1.entries[2].op_id, op_c1);
    assert_eq!(r1.entries[2].status, OpLogEntryStatus::Active);

    // 4. Undo 1 step -> op_c2 becomes Undone, undo1 becomes Redoable (HEAD + next_redo), cp1 becomes next_undo.
    let u1 = undo(&repo, 1usize, agent_actor(None)).expect("undo 1");
    let r2 = inspect_oplog(&repo, OpLogListOptions::default()).expect("inspect after undo");
    assert_eq!(r2.total_entries, 4);
    assert_eq!(r2.active_count, 2);
    assert_eq!(r2.redo_depth, 1);
    assert_eq!(r2.head_op_id.as_deref(), Some(u1.op_id.as_str()));
    assert_eq!(r2.undo_cursor_op_id.as_deref(), Some(cp1.op_id.as_str()));
    assert_eq!(r2.redo_cursor_op_id.as_deref(), Some(u1.op_id.as_str()));

    assert_eq!(r2.entries[0].op_id, u1.op_id);
    assert_eq!(r2.entries[0].kind, OpKind::Undo);
    assert_eq!(r2.entries[0].status, OpLogEntryStatus::Redoable);
    assert!(r2.entries[0].is_head);
    assert!(r2.entries[0].is_next_redo);
    assert!(!r2.entries[0].is_next_undo);

    assert_eq!(r2.entries[1].op_id, op_c2);
    assert_eq!(r2.entries[1].status, OpLogEntryStatus::Undone);
    assert!(!r2.entries[1].is_next_undo);

    assert_eq!(r2.entries[2].op_id, cp1.op_id);
    assert_eq!(r2.entries[2].status, OpLogEntryStatus::Active);
    assert!(r2.entries[2].is_next_undo);

    // 5. Redo -> u1 becomes Redone, redo1 is Applied (HEAD), op_c2 is Active & next_undo again.
    let rd1 = redo(&repo, agent_actor(None)).expect("redo 1");
    let r3 = inspect_oplog(&repo, OpLogListOptions::default()).expect("inspect after redo");
    assert_eq!(r3.active_count, 3);
    assert_eq!(r3.redo_depth, 0);
    assert_eq!(r3.undo_cursor_op_id.as_deref(), Some(op_c2.as_str()));
    assert_eq!(r3.redo_cursor_op_id, None);
    assert_eq!(r3.entries[0].op_id, rd1.op_id);
    assert_eq!(r3.entries[0].status, OpLogEntryStatus::Applied);
    assert!(r3.entries[0].is_head);
    assert_eq!(r3.entries[1].op_id, u1.op_id);
    assert_eq!(r3.entries[1].status, OpLogEntryStatus::Redone);
    assert_eq!(r3.entries[2].op_id, op_c2);
    assert_eq!(r3.entries[2].status, OpLogEntryStatus::Active);
    assert!(r3.entries[2].is_next_undo);

    // 6. Undo then record a new forward mutation -> that undo becomes Superseded.
    let u2 = undo(&repo, 1usize, agent_actor(None)).expect("undo 2");
    fs::write(repo_dir.join("a.txt"), "v3\n").unwrap();
    let op_c3 = oz_commit_with_oplog(&repo, &oz_core::CommitOptions::new("feat: third commit"))
        .1
        .op_id;

    let r4 = inspect_oplog(&repo, OpLogListOptions::default()).expect("inspect after supersede");
    assert_eq!(r4.redo_depth, 0);
    assert_eq!(r4.redo_cursor_op_id, None);
    assert_eq!(r4.undo_cursor_op_id.as_deref(), Some(op_c3.as_str()));
    let u2_item = r4
        .entries
        .iter()
        .find(|e| e.op_id == u2.op_id)
        .expect("u2 item");
    assert_eq!(u2_item.status, OpLogEntryStatus::Superseded);

    // 7. Limit & truncation.
    let limited = inspect_oplog(
        &repo,
        OpLogListOptions {
            limit: 2,
            all_workspaces: false,
        },
    )
    .expect("inspect limited");
    assert_eq!(limited.entries.len(), 2);
    assert!(limited.truncated);
    assert_eq!(limited.total_entries, r4.total_entries);
    assert_eq!(limited.entries[0].op_id, op_c3);
    assert_eq!(limited.entries[1].op_id, u2.op_id);

    // 8. Workspace filtering vs `--all`.
    let other_op_id = new_op_id();
    let other_entry = OpLogEntry {
        op_id: other_op_id.clone(),
        parents: Vec::new(),
        timestamp: Utc::now(),
        actor: agent_actor(Some("agent-other")),
        kind: OpKind::Checkpoint,
        command: "oz checkpoint".to_string(),
        message: Some("other ws checkpoint".to_string()),
        inverse_of: None,
        undone_ops: Vec::new(),
        rewound_uncheckpointed: false,
        ref_deltas: BTreeMap::new(),
        ws_snapshots: BTreeMap::new(),
    };
    append_entry(&repo.layout, other_entry).expect("append other ws entry");

    let default_only = inspect_oplog(
        &repo,
        OpLogListOptions {
            limit: 20,
            all_workspaces: false,
        },
    )
    .expect("inspect default ws");
    assert!(!default_only.entries.iter().any(|e| e.op_id == other_op_id));

    let all_ws = inspect_oplog(
        &repo,
        OpLogListOptions {
            limit: 20,
            all_workspaces: true,
        },
    )
    .expect("inspect all ws");
    let other_item = all_ws
        .entries
        .iter()
        .find(|e| e.op_id == other_op_id)
        .expect("other ws entry included with all_workspaces");
    assert_eq!(other_item.workspace.as_deref(), Some("agent-other"));
    assert_eq!(other_item.status, OpLogEntryStatus::Active);
    assert!(!other_item.is_next_undo);
    assert!(other_item.is_head);
}
