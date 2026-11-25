// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Stateful property-based tests for `oz checkpoint`, `oz commit` (`record_mutation`),
//! `oz undo` / `oz redo`, OpLog compaction (`compact_oplog`), and `fsck_repo` (`--repair`)
//! across unicode paths, symlinks, executable bits, renames, binary files, and nested directories.

use chrono::{Duration, Utc};
use oz_core::{
    BackendKind, CommitOptions, OzError, OzoneLayout, OzoneRepo, WorkspaceState, commit_changes,
};
use oz_oplog::{
    ActorInfo, ActorKind, FsckOptions, OpKind, OpLogEntry, OpLogGcOptions, UndoTarget,
    append_entry, checkpoint, compact_oplog, fsck_repo, list_entries, new_op_id, read_head_op_id,
    record_mutation, redo, undo,
};
use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, FileFailurePersistence};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

#[derive(Debug, Clone, PartialEq, Eq)]
enum TreeEntry {
    File { bytes: Vec<u8>, executable: bool },
    Symlink { target: String },
}

#[derive(Debug, Clone)]
enum FileEditOp {
    WriteText {
        path: String,
        content: String,
        executable: bool,
    },
    WriteBinary {
        path: String,
        bytes: Vec<u8>,
    },
    SetSymlink {
        path: String,
        target: String,
    },
    Delete {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
}

#[derive(Debug, Clone)]
enum MachineStep {
    DirtyEdit(FileEditOp),
    Checkpoint { message: String },
    CommitFull { message: String },
    CommitExcludeOne { message: String },
    CommitAmend { message: String },
    UndoStep,
    RedoStep,
    RunGcCompact { age_days: i64 },
}

#[derive(Debug, Clone)]
struct StateMachineCase {
    base_tok: String,
    base_bin: Vec<u8>,
    base_exec: bool,
    base_link: String,
    steps: Vec<MachineStep>,
}

#[derive(Debug, Clone)]
struct MultiWorkspaceCase {
    main_tok: String,
    ws_tok: String,
    ws_bin: Vec<u8>,
    ws_link: String,
    ws_exec: bool,
    main_edits_after_cp: String,
    ws_edits_after_cp: String,
    corrupt_head: bool,
    add_interrupted_txn: bool,
}

#[derive(Debug, Clone)]
struct RecordedTransition {
    op_id: String,
    is_checkpoint: bool,
    is_rewind_only: bool,
    can_undo_self: bool,
    pre_head: String,
    pre_tree: String,
    pre_disk: BTreeMap<String, TreeEntry>,
    post_head: String,
    post_tree: String,
    post_disk: BTreeMap<String, TreeEntry>,
    changed_paths: BTreeSet<String>,
}

struct OracleModel {
    current_head: String,
    current_tree: String,
    current_disk: BTreeMap<String, TreeEntry>,
    latest_recorded_ws_state: Option<(String, String, BTreeMap<String, TreeEntry>)>,
    active: Vec<RecordedTransition>,
    redo_stack: Vec<RecordedTransition>,
}

impl OracleModel {
    fn new(head: String, tree: String, disk: BTreeMap<String, TreeEntry>) -> Self {
        Self {
            current_head: head,
            current_tree: tree,
            current_disk: disk,
            latest_recorded_ws_state: None,
            active: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    fn apply_dirty(&mut self, disk: BTreeMap<String, TreeEntry>) {
        self.current_disk = disk;
    }

    fn push_checkpoint(
        &mut self,
        op_id: String,
        post_head: String,
        post_tree: String,
        post_disk: BTreeMap<String, TreeEntry>,
        fallback_base: &(String, String, BTreeMap<String, TreeEntry>),
    ) {
        let can_undo_self = self.latest_recorded_ws_state.is_some();
        let (pre_head, pre_tree, pre_disk) = self
            .latest_recorded_ws_state
            .clone()
            .unwrap_or_else(|| fallback_base.clone());
        let changed_paths = diff_snapshots(&pre_disk, &post_disk);
        self.current_head.clone_from(&post_head);
        self.current_tree.clone_from(&post_tree);
        self.current_disk = post_disk.clone();
        self.latest_recorded_ws_state =
            Some((post_head.clone(), post_tree.clone(), post_disk.clone()));
        self.active.push(RecordedTransition {
            op_id,
            is_checkpoint: true,
            is_rewind_only: false,
            can_undo_self,
            pre_head,
            pre_tree,
            pre_disk,
            post_head,
            post_tree,
            post_disk,
            changed_paths,
        });
        self.redo_stack.clear();
    }

    #[allow(clippy::too_many_arguments)]
    fn push_commit(
        &mut self,
        repo: &OzoneRepo,
        op_id: String,
        pre_head: String,
        pre_tree: String,
        pre_disk: BTreeMap<String, TreeEntry>,
        post_head: String,
        post_tree: String,
        post_disk: BTreeMap<String, TreeEntry>,
    ) {
        let pre_tree_snap = snapshot_gix_tree(repo, &pre_tree);
        let post_tree_snap = snapshot_gix_tree(repo, &post_tree);
        let mut changed_paths = diff_snapshots(&pre_tree_snap, &post_tree_snap);
        changed_paths.extend(diff_snapshots(&pre_tree_snap, &pre_disk));
        changed_paths.extend(diff_snapshots(&post_tree_snap, &post_disk));
        self.current_head.clone_from(&post_head);
        self.current_tree.clone_from(&post_tree);
        self.current_disk = post_disk.clone();
        self.latest_recorded_ws_state =
            Some((post_head.clone(), post_tree.clone(), post_disk.clone()));
        self.active.push(RecordedTransition {
            op_id,
            is_checkpoint: false,
            is_rewind_only: false,
            can_undo_self: true,
            pre_head,
            pre_tree,
            pre_disk,
            post_head,
            post_tree,
            post_disk,
            changed_paths,
        });
        self.redo_stack.clear();
    }

    fn can_undo_now(&self) -> bool {
        let Some(top) = self.active.last() else {
            return false;
        };
        if top.is_checkpoint && self.current_disk != top.post_disk {
            true
        } else {
            top.can_undo_self
        }
    }

    fn undo_step(&mut self) -> bool {
        let Some(top) = self.active.last().cloned() else {
            return false;
        };
        if top.is_checkpoint && self.current_disk != top.post_disk {
            let live_disk = self.current_disk.clone();
            let changed_paths = diff_snapshots(&top.post_disk, &live_disk);
            self.current_head.clone_from(&top.post_head);
            self.current_tree.clone_from(&top.post_tree);
            self.current_disk = top.post_disk.clone();
            self.latest_recorded_ws_state = Some((
                self.current_head.clone(),
                self.current_tree.clone(),
                self.current_disk.clone(),
            ));
            self.redo_stack.push(RecordedTransition {
                op_id: format!("rewind-{}", top.op_id),
                is_checkpoint: false,
                is_rewind_only: true,
                can_undo_self: true,
                pre_head: top.post_head,
                pre_tree: top.post_tree,
                pre_disk: top.post_disk,
                post_head: self.current_head.clone(),
                post_tree: self.current_tree.clone(),
                post_disk: live_disk,
                changed_paths,
            });
            return true;
        }
        if !top.can_undo_self {
            return false;
        }

        let popped = self.active.pop().expect("active top");
        if self.current_disk == popped.post_disk
            && self.current_head == popped.post_head
            && self.current_tree == popped.post_tree
        {
            self.current_disk = popped.pre_disk.clone();
        } else {
            // Uncheckpointed edits on paths NOT touched by `popped` remain preserved by `restore_working_tree`.
            for path in &popped.changed_paths {
                match popped.pre_disk.get(path) {
                    Some(entry) => {
                        self.current_disk.insert(path.clone(), entry.clone());
                    }
                    None => {
                        self.current_disk.remove(path);
                    }
                }
            }
        }
        self.current_head.clone_from(&popped.pre_head);
        self.current_tree.clone_from(&popped.pre_tree);
        self.latest_recorded_ws_state = Some((
            self.current_head.clone(),
            self.current_tree.clone(),
            popped.pre_disk.clone(),
        ));
        self.redo_stack.push(popped);
        true
    }

    fn redo_step(&mut self) -> bool {
        let Some(top) = self.redo_stack.pop() else {
            return false;
        };
        self.current_head.clone_from(&top.post_head);
        self.current_tree.clone_from(&top.post_tree);
        if self.current_disk == top.pre_disk {
            self.current_disk = top.post_disk.clone();
        } else {
            // Uncheckpointed edits on paths NOT touched by `top` remain preserved by `restore_working_tree`.
            for path in &top.changed_paths {
                match top.post_disk.get(path) {
                    Some(entry) => {
                        self.current_disk.insert(path.clone(), entry.clone());
                    }
                    None => {
                        self.current_disk.remove(path);
                    }
                }
            }
        }
        let recorded_disk = top.post_disk.clone();
        if !top.is_rewind_only {
            self.active.push(top);
        }
        self.latest_recorded_ws_state = Some((
            self.current_head.clone(),
            self.current_tree.clone(),
            recorded_disk,
        ));
        true
    }

    fn sync_surviving_after_gc(&mut self, surviving_ids: &BTreeSet<String>) {
        self.active.retain(|t| surviving_ids.contains(&t.op_id));
    }
}

fn snapshot_gix_tree(repo: &OzoneRepo, tree_oid_hex: &str) -> BTreeMap<String, TreeEntry> {
    let gix_repo = repo.open_gix().expect("open gix");
    let tree_oid = gix::ObjectId::from_hex(tree_oid_hex.as_bytes()).expect("parse tree hex");
    let tree = gix_repo.find_tree(tree_oid).expect("find tree");
    let files = tree.traverse().breadthfirst.files().expect("traverse tree");
    let mut out = BTreeMap::new();
    for entry in files {
        if entry.mode.is_tree() {
            continue;
        }
        let rel = String::from_utf8_lossy(entry.filepath.as_ref()).into_owned();
        let bytes = gix_repo
            .find_blob(entry.oid)
            .expect("find blob")
            .data
            .clone();
        if entry.mode.kind() == gix::objs::tree::EntryKind::Link {
            out.insert(
                rel,
                TreeEntry::Symlink {
                    target: String::from_utf8_lossy(&bytes).into_owned(),
                },
            );
        } else {
            let executable = entry.mode.kind() == gix::objs::tree::EntryKind::BlobExecutable;
            out.insert(rel, TreeEntry::File { bytes, executable });
        }
    }
    out
}

fn diff_snapshots(
    a: &BTreeMap<String, TreeEntry>,
    b: &BTreeMap<String, TreeEntry>,
) -> BTreeSet<String> {
    let mut changed = BTreeSet::new();
    for (k, va) in a {
        if b.get(k) != Some(va) {
            changed.insert(k.clone());
        }
    }
    for k in b.keys() {
        if !a.contains_key(k) {
            changed.insert(k.clone());
        }
    }
    changed
}

fn proptest_config(default_cases: u32) -> ProptestConfig {
    let fallback_cases = if cfg!(windows) && std::env::var_os("CI").is_some() {
        default_cases.min(10)
    } else {
        default_cases
    };
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(fallback_cases);
    ProptestConfig {
        cases,
        failure_persistence: Some(Box::new(FileFailurePersistence::Off)),
        ..ProptestConfig::default()
    }
}

fn sys_git_bin() -> PathBuf {
    let unix_git = Path::new("/usr/bin/git");
    if unix_git.is_file() {
        unix_git.to_path_buf()
    } else {
        PathBuf::from("git")
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new(sys_git_bin())
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_DATE", "2026-01-15T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-15T12:00:00Z")
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

fn init_git_repo(dir: &Path) {
    fs::create_dir_all(dir).expect("mkdir repo");
    run_git(dir, &["init", "-b", "main"]);
    run_git(dir, &["config", "user.name", "PropTest OpLog"]);
    run_git(dir, &["config", "user.email", "oplog@ozone.test"]);
    run_git(dir, &["config", "core.autocrlf", "false"]);
}

fn snapshot_worktree_disk(root: &Path) -> BTreeMap<String, TreeEntry> {
    let mut out = BTreeMap::new();
    collect_worktree_recursive(root, root, &mut out);
    out
}

fn collect_worktree_recursive(root: &Path, dir: &Path, out: &mut BTreeMap<String, TreeEntry>) {
    let mut children: Vec<_> = fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .collect();
    children.sort_by_key(|e| e.file_name());

    for entry in children {
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).expect("symlink_metadata");
        let rel = path
            .strip_prefix(root)
            .expect("strip prefix")
            .to_string_lossy()
            .replace('\\', "/");

        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path)
                .expect("read_link")
                .to_string_lossy()
                .into_owned();
            out.insert(rel, TreeEntry::Symlink { target });
        } else if meta.is_dir() {
            collect_worktree_recursive(root, &path, out);
        } else {
            let bytes = fs::read(&path).expect("read file");
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                (meta.permissions().mode() & 0o111) != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            out.insert(rel, TreeEntry::File { bytes, executable });
        }
    }
}

fn prune_empty_parents(root: &Path, path: &Path) {
    let mut cur = path.parent();
    while let Some(dir) = cur {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}

fn remove_path_and_clean(root: &Path, rel: &str) {
    let full = root.join(rel);
    if let Ok(meta) = fs::symlink_metadata(&full) {
        if meta.is_dir() && !meta.file_type().is_symlink() {
            fs::remove_dir_all(&full).expect("remove dir");
        } else {
            fs::remove_file(&full).expect("remove file");
        }
        prune_empty_parents(root, &full);
    }
}

fn write_entry(root: &Path, rel: &str, entry: &TreeEntry) {
    let full = root.join(rel);
    if let Ok(meta) = fs::symlink_metadata(&full) {
        if meta.is_dir() && !meta.file_type().is_symlink() {
            fs::remove_dir_all(&full).expect("remove dir collision");
        } else if meta.file_type().is_symlink() {
            fs::remove_file(&full).expect("remove symlink");
        }
    }
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("mkdir parents");
    }
    match entry {
        TreeEntry::File { bytes, executable } => {
            fs::write(&full, bytes).expect("write file");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if *executable { 0o755 } else { 0o644 };
                fs::set_permissions(&full, fs::Permissions::from_mode(mode)).expect("chmod");
            }
            #[cfg(not(unix))]
            let _ = executable;
        }
        TreeEntry::Symlink { target } => {
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &full).expect("symlink");
            #[cfg(not(unix))]
            fs::write(&full, target.as_bytes()).expect("symlink fallback");
        }
    }
}

fn apply_file_edit(root: &Path, op: &FileEditOp) {
    match op {
        FileEditOp::WriteText {
            path,
            content,
            executable,
        } => {
            write_entry(
                root,
                path,
                &TreeEntry::File {
                    bytes: content.as_bytes().to_vec(),
                    executable: *executable,
                },
            );
        }
        FileEditOp::WriteBinary { path, bytes } => {
            write_entry(
                root,
                path,
                &TreeEntry::File {
                    bytes: bytes.clone(),
                    executable: false,
                },
            );
        }
        FileEditOp::SetSymlink { path, target } => {
            write_entry(
                root,
                path,
                &TreeEntry::Symlink {
                    target: target.clone(),
                },
            );
        }
        FileEditOp::Delete { path } => {
            remove_path_and_clean(root, path);
        }
        FileEditOp::Rename { from, to } => {
            let full = root.join(from);
            if let Ok(meta) = fs::symlink_metadata(&full)
                && !meta.is_dir()
            {
                let entry = if meta.file_type().is_symlink() {
                    TreeEntry::Symlink {
                        target: fs::read_link(&full).unwrap().to_string_lossy().into_owned(),
                    }
                } else {
                    #[cfg(unix)]
                    let executable = {
                        use std::os::unix::fs::PermissionsExt;
                        (meta.permissions().mode() & 0o111) != 0
                    };
                    #[cfg(not(unix))]
                    let executable = false;
                    TreeEntry::File {
                        bytes: fs::read(&full).unwrap(),
                        executable,
                    }
                };
                remove_path_and_clean(root, from);
                write_entry(root, to, &entry);
            } else {
                write_entry(
                    root,
                    to,
                    &TreeEntry::File {
                        bytes: format!("renamed-fresh {from} -> {to}\n").into_bytes(),
                        executable: false,
                    },
                );
            }
        }
    }
}

fn mirror_disk_into_dir(src_disk: &BTreeMap<String, TreeEntry>, dst_dir: &Path) {
    for entry in fs::read_dir(dst_dir).expect("read_dir") {
        let entry = entry.expect("dir entry");
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).expect("meta");
        if meta.is_dir() && !meta.file_type().is_symlink() {
            fs::remove_dir_all(&path).expect("rm dir");
        } else {
            fs::remove_file(&path).expect("rm file");
        }
    }
    for (rel, item) in src_disk {
        write_entry(dst_dir, rel, item);
    }
}

fn backdate_older_than_cutoff(layout: &OzoneLayout, min_keep_recent: usize, age_days: i64) {
    let entries = list_entries(layout).expect("list entries");
    if entries.len() <= min_keep_recent {
        return;
    }
    let cutoff_idx = entries.len() - min_keep_recent;
    for entry in entries.iter().take(cutoff_idx) {
        let mut updated = entry.clone();
        updated.timestamp = Utc::now() - Duration::days(age_days);
        let path = oz_oplog::entry_path(layout, &entry.op_id).expect("entry_path");
        fs::write(&path, serde_json::to_string_pretty(&updated).unwrap()).expect("backdate");
    }
}

fn test_actor(ws: Option<&str>) -> ActorInfo {
    ActorInfo {
        kind: ActorKind::Agent,
        client: Some("proptest-oplog".to_string()),
        workspace: ws.map(ToOwned::to_owned),
    }
}

fn arb_token() -> BoxedStrategy<String> {
    prop::string::string_regex("[a-zA-Z0-9_]{3,14}")
        .expect("token")
        .boxed()
}

fn arb_rel_path() -> BoxedStrategy<String> {
    prop_oneof![
        Just("src/lib.rs".to_string()),
        Just("docs/café/résumé-日本語.md".to_string()),
        Just("nested/a/b/c/mod-α.txt".to_string()),
        Just("extra/notes.txt".to_string()),
    ]
    .boxed()
}

fn arb_script_path() -> BoxedStrategy<String> {
    prop_oneof![
        Just("scripts/run-café.sh".to_string()),
        Just("scripts/nested/deploy-β.sh".to_string()),
    ]
    .boxed()
}

fn arb_bin_path() -> BoxedStrategy<String> {
    prop_oneof![
        Just("assets/blob.bin".to_string()),
        Just("assets/café/payload.bin".to_string()),
    ]
    .boxed()
}

fn arb_link_path() -> BoxedStrategy<String> {
    prop_oneof![
        Just("links/latest".to_string()),
        Just("links/nested/ref_link".to_string()),
    ]
    .boxed()
}

fn arb_link_target() -> BoxedStrategy<String> {
    prop_oneof![
        Just("../README.md".to_string()),
        Just("../docs/café/résumé-日本語.md".to_string()),
        Just("../src/lib.rs".to_string()),
    ]
    .boxed()
}

fn arb_binary_bytes() -> BoxedStrategy<Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..64)
        .prop_map(|mut v| {
            if !v.contains(&0) {
                v.insert(0, 0);
            }
            v
        })
        .boxed()
}

fn arb_file_edit() -> BoxedStrategy<FileEditOp> {
    prop_oneof![
        3 => (arb_rel_path(), arb_token(), any::<bool>()).prop_map(|(path, tok, executable)| {
            FileEditOp::WriteText {
                path,
                content: format!("text line {tok}\nsecond line\n"),
                executable,
            }
        }),
        2 => (arb_script_path(), arb_token(), any::<bool>()).prop_map(|(path, tok, executable)| {
            FileEditOp::WriteText {
                path,
                content: format!("#!/bin/sh\necho \"{tok}\"\n"),
                executable,
            }
        }),
        2 => (arb_bin_path(), arb_binary_bytes())
            .prop_map(|(path, bytes)| FileEditOp::WriteBinary { path, bytes }),
        2 => (arb_link_path(), arb_link_target())
            .prop_map(|(path, target)| FileEditOp::SetSymlink { path, target }),
        1 => Just(FileEditOp::Rename {
            from: "extra/notes.txt".to_string(),
            to: "nested/moved-café/notes-日本語.txt".to_string(),
        }),
        1 => Just(FileEditOp::Delete {
            path: "extra/notes.txt".to_string(),
        }),
    ]
    .boxed()
}

fn arb_machine_step() -> BoxedStrategy<MachineStep> {
    prop_oneof![
        4 => arb_file_edit().prop_map(MachineStep::DirtyEdit),
        2 => arb_token().prop_map(|t| MachineStep::Checkpoint { message: format!("cp {t}") }),
        2 => arb_token().prop_map(|t| MachineStep::CommitFull { message: format!("feat: {t}") }),
        1 => arb_token().prop_map(|t| MachineStep::CommitExcludeOne { message: format!("partial: {t}") }),
        1 => arb_token().prop_map(|t| MachineStep::CommitAmend { message: format!("amend: {t}") }),
        2 => Just(MachineStep::UndoStep),
        2 => Just(MachineStep::RedoStep),
        1 => Just(MachineStep::RunGcCompact { age_days: 45 }),
    ]
    .boxed()
}

fn arb_state_machine_case() -> BoxedStrategy<StateMachineCase> {
    (
        arb_token(),
        arb_binary_bytes(),
        any::<bool>(),
        arb_link_target(),
        prop::collection::vec(arb_machine_step(), 6..12),
    )
        .prop_map(
            |(base_tok, base_bin, base_exec, base_link, steps)| StateMachineCase {
                base_tok,
                base_bin,
                base_exec,
                base_link,
                steps,
            },
        )
        .boxed()
}

fn arb_multi_workspace_case() -> BoxedStrategy<MultiWorkspaceCase> {
    (
        arb_token(),
        arb_token(),
        arb_binary_bytes(),
        arb_link_target(),
        any::<bool>(),
        arb_token(),
        arb_token(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(
                main_tok,
                ws_tok,
                ws_bin,
                ws_link,
                ws_exec,
                main_edits_after_cp,
                ws_edits_after_cp,
                corrupt_head,
                add_interrupted_txn,
            )| MultiWorkspaceCase {
                main_tok,
                ws_tok,
                ws_bin,
                ws_link,
                ws_exec,
                main_edits_after_cp,
                ws_edits_after_cp,
                corrupt_head,
                add_interrupted_txn,
            },
        )
        .boxed()
}

proptest! {
    #![proptest_config(proptest_config(28))]

    #[test]
    fn proptest_oplog_state_machine_checkpoint_commit_undo_redo_and_gc(
        case in arb_state_machine_case()
    ) {
        let tmp = TempDir::new().expect("tempdir");
        let repo_dir = tmp.path().join("oplog-sm");
        let ref_dir = tmp.path().join("ref-sm");
        init_git_repo(&repo_dir);

        write_entry(
            &repo_dir,
            "README.md",
            &TreeEntry::File {
                bytes: format!("# OpLog State Machine {}\n", case.base_tok).into_bytes(),
                executable: false,
            },
        );
        write_entry(
            &repo_dir,
            "src/lib.rs",
            &TreeEntry::File {
                bytes: format!("pub fn base() -> &'static str {{ \"{}\" }}\n", case.base_tok)
                    .into_bytes(),
                executable: false,
            },
        );
        write_entry(
            &repo_dir,
            "docs/café/résumé-日本語.md",
            &TreeEntry::File {
                bytes: format!("Base {}\n", case.base_tok).into_bytes(),
                executable: false,
            },
        );
        write_entry(
            &repo_dir,
            "scripts/run-café.sh",
            &TreeEntry::File {
                bytes: format!("#!/bin/sh\necho \"{}\"\n", case.base_tok).into_bytes(),
                executable: case.base_exec,
            },
        );
        write_entry(
            &repo_dir,
            "assets/blob.bin",
            &TreeEntry::File {
                bytes: case.base_bin.clone(),
                executable: false,
            },
        );
        write_entry(
            &repo_dir,
            "links/latest",
            &TreeEntry::Symlink {
                target: case.base_link.clone(),
            },
        );
        write_entry(
            &repo_dir,
            "extra/notes.txt",
            &TreeEntry::File {
                bytes: format!("notes {}\n", case.base_tok).into_bytes(),
                executable: false,
            },
        );
        run_git(&repo_dir, &["add", "-A"]);
        run_git(&repo_dir, &["commit", "-m", "initial base"]);

        let base_head = run_git(&repo_dir, &["rev-parse", "HEAD"]);
        let base_tree = run_git(&repo_dir, &["rev-parse", "HEAD^{tree}"]);
        let base_disk = snapshot_worktree_disk(&repo_dir);

        let repo = OzoneRepo::discover(Some(&repo_dir)).expect("discover");
        repo.layout.ensure_initialized().expect("init ozone");

        // Maintain a parallel clean git repo solely to verify tree-OID parity on every full commit!
        init_git_repo(&ref_dir);
        mirror_disk_into_dir(&base_disk, &ref_dir);
        run_git(&ref_dir, &["add", "-A"]);
        run_git(&ref_dir, &["commit", "-m", "initial base"]);

        let mut model = OracleModel::new(base_head.clone(), base_tree.clone(), base_disk.clone());
        let actor = test_actor(None);

        for step in &case.steps {
            match step {
                MachineStep::DirtyEdit(op) => {
                    apply_file_edit(&repo_dir, op);
                    model.apply_dirty(snapshot_worktree_disk(&repo_dir));
                }
                MachineStep::Checkpoint { message } => {
                    let cp = checkpoint(
                        &repo,
                        "oz checkpoint",
                        Some(message),
                        actor.clone(),
                        None,
                    )
                    .expect("checkpoint");
                    let post_head = run_git(&repo_dir, &["rev-parse", "HEAD"]);
                    let post_tree = run_git(&repo_dir, &["rev-parse", "HEAD^{tree}"]);
                    let post_disk = snapshot_worktree_disk(&repo_dir);
                    model.push_checkpoint(
                        cp.op_id,
                        post_head,
                        post_tree,
                        post_disk,
                        &(base_head.clone(), base_tree.clone(), base_disk.clone()),
                    );
                }
                MachineStep::CommitFull { message } => {
                    let pre_head = run_git(&repo_dir, &["rev-parse", "HEAD"]);
                    let pre_tree = run_git(&repo_dir, &["rev-parse", "HEAD^{tree}"]);
                    let pre_disk = snapshot_worktree_disk(&repo_dir);

                    let mut opts = CommitOptions::new(message.clone());
                    opts.force = true;
                    match record_mutation(&repo, "oz commit", actor.clone(), None, || {
                        commit_changes(&repo, &opts)
                    }) {
                        Ok((res, entry)) => {
                            let post_head = res.commit_oid.clone();
                            let post_tree = run_git(&repo_dir, &["rev-parse", "HEAD^{tree}"]);
                            let post_disk = snapshot_worktree_disk(&repo_dir);

                            // Cross-check tree OID against reference `git commit` of identical snapshot!
                            // Drop `ref_dir`'s `.git/index` first so Windows NTFS coarse mtime + file
                            // tunneling never causes `git add -A` to skip an equal-length rewrite.
                            mirror_disk_into_dir(&post_disk, &ref_dir);
                            let _ = fs::remove_file(ref_dir.join(".git").join("index"));
                            run_git(&ref_dir, &["add", "-A"]);
                            let ref_res = Command::new(sys_git_bin())
                                .args(["diff", "--cached", "--quiet"])
                                .current_dir(&ref_dir)
                                .status()
                                .expect("git diff cached");
                            if !ref_res.success() {
                                run_git(&ref_dir, &["commit", "-m", message]);
                            }
                            let ref_tree = run_git(&ref_dir, &["rev-parse", "HEAD^{tree}"]);
                            prop_assert_eq!(&post_tree, &ref_tree);

                            model.push_commit(
                                &repo,
                                entry.op_id,
                                pre_head,
                                pre_tree,
                                pre_disk,
                                post_head,
                                post_tree,
                                post_disk,
                            );
                        }
                        Err(OzError::InvalidUsage { .. }) => {
                            // No-op commit must leave state untouched and append nothing.
                        }
                        Err(other) => {
                            return Err(proptest::test_runner::TestCaseError::fail(format!(
                                "unexpected commit error: {other:?}"
                            )));
                        }
                    }
                }
                MachineStep::CommitExcludeOne { message } => {
                    let pre_head = run_git(&repo_dir, &["rev-parse", "HEAD"]);
                    let pre_tree = run_git(&repo_dir, &["rev-parse", "HEAD^{tree}"]);
                    let pre_disk = snapshot_worktree_disk(&repo_dir);
                    let mut opts = CommitOptions::new(message.clone());
                    opts.exclude = vec!["assets/*".to_string(), "extra/*".to_string()];
                    opts.force = true;
                    match record_mutation(&repo, "oz commit --exclude", actor.clone(), None, || {
                        commit_changes(&repo, &opts)
                    }) {
                        Ok((res, entry)) => {
                            let post_head = res.commit_oid.clone();
                            let post_tree = run_git(&repo_dir, &["rev-parse", "HEAD^{tree}"]);
                            let post_disk = snapshot_worktree_disk(&repo_dir);
                            model.push_commit(
                                &repo,
                                entry.op_id,
                                pre_head,
                                pre_tree,
                                pre_disk,
                                post_head,
                                post_tree,
                                post_disk,
                            );
                        }
                        Err(OzError::InvalidUsage { .. }) => {}
                        Err(other) => {
                            return Err(proptest::test_runner::TestCaseError::fail(format!(
                                "unexpected partial commit error: {other:?}"
                            )));
                        }
                    }
                }
                MachineStep::CommitAmend { message } => {
                    // Only amend once we have at least one commit beyond or at base.
                    let pre_head = run_git(&repo_dir, &["rev-parse", "HEAD"]);
                    let pre_tree = run_git(&repo_dir, &["rev-parse", "HEAD^{tree}"]);
                    let pre_disk = snapshot_worktree_disk(&repo_dir);
                    let mut opts = CommitOptions::new(message.clone());
                    opts.amend = true;
                    opts.force = true;
                    match record_mutation(&repo, "oz commit --amend", actor.clone(), None, || {
                        commit_changes(&repo, &opts)
                    }) {
                        Ok((res, entry)) => {
                            let post_head = res.commit_oid.clone();
                            let post_tree = run_git(&repo_dir, &["rev-parse", "HEAD^{tree}"]);
                            let post_disk = snapshot_worktree_disk(&repo_dir);
                            model.push_commit(
                                &repo,
                                entry.op_id,
                                pre_head,
                                pre_tree,
                                pre_disk,
                                post_head,
                                post_tree,
                                post_disk,
                            );
                        }
                        Err(OzError::InvalidUsage { .. }) => {}
                        Err(other) => {
                            return Err(proptest::test_runner::TestCaseError::fail(format!(
                                "unexpected amend error: {other:?}"
                            )));
                        }
                    }
                }
                MachineStep::UndoStep => {
                    if !model.can_undo_now() {
                        let res = undo(&repo, UndoTarget::Steps(1), actor.clone());
                        prop_assert!(
                            matches!(res, Err(OzError::InvalidUsage { .. })),
                            "unavailable undo should yield InvalidUsage, got {res:?}"
                        );
                    } else {
                        undo(&repo, UndoTarget::Steps(1), actor.clone()).expect("undo step");
                        let did = model.undo_step();
                        prop_assert!(did);
                    }
                }
                MachineStep::RedoStep => {
                    if model.redo_stack.is_empty() {
                        let res = redo(&repo, actor.clone());
                        prop_assert!(
                            matches!(res, Err(OzError::InvalidUsage { .. })),
                            "empty redo stack should yield InvalidUsage, got {res:?}"
                        );
                    } else {
                        redo(&repo, actor.clone()).expect("redo step");
                        let did = model.redo_step();
                        prop_assert!(did);
                    }
                }
                MachineStep::RunGcCompact { age_days } => {
                    // Age older entries past cutoff while keeping the newest 2 entries fresh.
                    backdate_older_than_cutoff(&repo.layout, 2, *age_days);
                    let dry = compact_oplog(
                        &repo,
                        &OpLogGcOptions {
                            oplog_days: 30,
                            dry_run: true,
                            ..Default::default()
                        },
                    )
                    .expect("dry-run gc");
                    prop_assert!(dry.dry_run);

                    let gc = compact_oplog(
                        &repo,
                        &OpLogGcOptions {
                            oplog_days: 30,
                            dry_run: false,
                            ..Default::default()
                        },
                    )
                    .expect("real gc");
                    prop_assert!(!gc.dry_run);

                    let surviving_ids: BTreeSet<String> = list_entries(&repo.layout)
                        .expect("list surviving")
                        .into_iter()
                        .map(|e| e.op_id)
                        .collect();
                    model.sync_surviving_after_gc(&surviving_ids);

                    // `redo_stack` entries linked through `Undo` closure survive GC too!
                    let fsck = fsck_repo(&repo, &FsckOptions { repair: false }).expect("fsck after gc");
                    prop_assert!(fsck.healthy, "fsck after gc unhealthy: {fsck:?}");
                }
            }

            // Invariant checked after EVERY step: exact HEAD, HEAD^{tree}, and bit-for-bit disk match!
            let actual_head = run_git(&repo_dir, &["rev-parse", "HEAD"]);
            let actual_tree = run_git(&repo_dir, &["rev-parse", "HEAD^{tree}"]);
            let actual_disk = snapshot_worktree_disk(&repo_dir);
            prop_assert_eq!(actual_head, model.current_head.clone());
            prop_assert_eq!(actual_tree, model.current_tree.clone());
            prop_assert_eq!(actual_disk, model.current_disk.clone());
        }

        // Final full compaction, repair check, `fsck_repo`, and `git fsck --full`.
        let final_fsck = fsck_repo(&repo, &FsckOptions { repair: false }).expect("final fsck");
        prop_assert!(final_fsck.healthy, "final fsck failed: {final_fsck:?}");
        assert_git_fsck_clean(&repo_dir);
    }

    #[test]
    fn proptest_oplog_gc_and_fsck_preserve_multi_workspace_isolation(
        case in arb_multi_workspace_case()
    ) {
        let tmp = TempDir::new().expect("tempdir");
        let repo_dir = tmp.path().join("multi-ws");
        let ws_dir = tmp.path().join("multi-ws.oz/ws-active");
        init_git_repo(&repo_dir);

        write_entry(
            &repo_dir,
            "README.md",
            &TreeEntry::File {
                bytes: format!("# Multi WS {}\n", case.main_tok).into_bytes(),
                executable: false,
            },
        );
        write_entry(
            &repo_dir,
            "café/shared.rs",
            &TreeEntry::File {
                bytes: b"pub fn shared() -> i32 { 1 }\n".to_vec(),
                executable: false,
            },
        );
        run_git(&repo_dir, &["add", "-A"]);
        run_git(&repo_dir, &["commit", "-m", "init"]);
        let head_oid = run_git(&repo_dir, &["rev-parse", "HEAD"]);

        let main_repo = OzoneRepo::discover(Some(&repo_dir)).expect("discover main");
        main_repo.layout.ensure_initialized().expect("init ozone");

        // Create active secondary workspace via `git worktree add` + WorkspaceState JSON.
        fs::create_dir_all(ws_dir.parent().unwrap()).unwrap();
        let ws_dir_str = ws_dir.to_string_lossy().into_owned();
        run_git(&repo_dir, &["worktree", "add", "-b", "feat/ws-active", &ws_dir_str, &head_oid]);
        let ws_state = WorkspaceState {
            schema_version: WorkspaceState::CURRENT_SCHEMA_VERSION,
            name: "ws-active".to_string(),
            branch: "feat/ws-active".to_string(),
            base_branch: Some("main".to_string()),
            base_commit: Some(head_oid.clone()),
            base_tree: head_oid.clone(),
            backend: BackendKind::CasHardlink,
            worktree_path: ws_dir.clone(),
            git_worktree_dir: Some(main_repo.layout.git_common_dir.join("worktrees/ws-active")),
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
            main_repo.layout.workspace_state_path("ws-active"),
            serde_json::to_string_pretty(&ws_state).unwrap(),
        )
        .unwrap();

        let ws_repo = OzoneRepo::discover(Some(&ws_dir)).expect("discover ws");
        let main_actor = test_actor(None);
        let ws_actor = test_actor(Some("ws-active"));

        // Old superseded checkpoint on main (50d) + recent checkpoint on main (1d).
        write_entry(
            &repo_dir,
            "café/shared.rs",
            &TreeEntry::File {
                bytes: format!("// old {}\n", case.main_tok).into_bytes(),
                executable: false,
            },
        );
        let main_old = checkpoint(&main_repo, "oz checkpoint", Some("old-main"), main_actor.clone(), None)
            .expect("main_old");

        // Active workspace makes unicode/binary/symlink/exec edits and checkpoints (aged 45d -> STILL protected!).
        write_entry(
            &ws_dir,
            "ws/nested-日本語/script.sh",
            &TreeEntry::File {
                bytes: format!("#!/bin/sh\necho \"{}\"\n", case.ws_tok).into_bytes(),
                executable: case.ws_exec,
            },
        );
        write_entry(
            &ws_dir,
            "ws/blob.bin",
            &TreeEntry::File {
                bytes: case.ws_bin.clone(),
                executable: false,
            },
        );
        write_entry(
            &ws_dir,
            "ws/sym_ref",
            &TreeEntry::Symlink {
                target: case.ws_link.clone(),
            },
        );
        let ws_cp = checkpoint(&ws_repo, "oz checkpoint", Some("ws-cp"), ws_actor.clone(), None)
            .expect("ws_cp");

        // Dead workspace aged 50d -> MUST be pruned by GC.
        let dead_entry = OpLogEntry {
            op_id: new_op_id(),
            parents: Vec::new(),
            timestamp: Utc::now() - Duration::days(50),
            actor: test_actor(Some("ws-gone")),
            kind: OpKind::Mutate,
            command: "oz commit".to_string(),
            message: None,
            inverse_of: None,
            undone_ops: Vec::new(),
            rewound_uncheckpointed: false,
            ref_deltas: BTreeMap::new(),
            ws_snapshots: BTreeMap::new(),
        };
        let dead_op = append_entry(&main_repo.layout, dead_entry).expect("dead_op");

        // Main recent checkpoint (kept within 30d window + latest for default).
        write_entry(
            &repo_dir,
            "café/shared.rs",
            &TreeEntry::File {
                bytes: format!("// recent {}\n", case.main_tok).into_bytes(),
                executable: true,
            },
        );
        let main_recent = checkpoint(
            &main_repo,
            "oz checkpoint",
            Some("recent-main"),
            main_actor.clone(),
            None,
        )
        .expect("main_recent");

        // Backdate old/dead/ws ops, leaving `main_recent` fresh.
        for (id, days) in [(&main_old.op_id, 50), (&ws_cp.op_id, 45), (&dead_op.op_id, 50)] {
            let p = oz_oplog::entry_path(&main_repo.layout, id).expect("entry_path");
            let mut e: OpLogEntry =
                serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
            e.timestamp = Utc::now() - Duration::days(days);
            fs::write(&p, serde_json::to_string_pretty(&e).unwrap()).unwrap();
        }

        let gc = compact_oplog(
            &main_repo,
            &OpLogGcOptions {
                oplog_days: 30,
                dry_run: false,
                ..Default::default()
            },
        )
        .expect("gc");
        prop_assert!(gc.removed_entries.contains(&main_old.op_id));
        prop_assert!(gc.removed_entries.contains(&dead_op.op_id));
        prop_assert!(!gc.removed_entries.contains(&ws_cp.op_id));
        prop_assert!(!gc.removed_entries.contains(&main_recent.op_id));

        // Add user uncheckpointed edits in BOTH workspaces that MUST NEVER be touched by fsck --repair!
        write_entry(
            &repo_dir,
            "user_main_café.txt",
            &TreeEntry::File {
                bytes: case.main_edits_after_cp.as_bytes().to_vec(),
                executable: false,
            },
        );
        write_entry(
            &ws_dir,
            "user_ws_α.txt",
            &TreeEntry::File {
                bytes: case.ws_edits_after_cp.as_bytes().to_vec(),
                executable: case.ws_exec,
            },
        );
        let expected_main_disk = snapshot_worktree_disk(&repo_dir);
        let expected_ws_disk = snapshot_worktree_disk(&ws_dir);

        // Inject crash artifacts: optional corrupt HEAD + optional interrupted txn artifacts + orphan mount.
        if case.corrupt_head {
            fs::write(main_repo.layout.oplog_dir().join("HEAD"), "corrupt-id\n").unwrap();
        }
        if case.add_interrupted_txn {
            fs::write(
                main_repo.layout.oplog_dir().join("ops/.tmp-crashed.json"),
                "{",
            )
            .unwrap();
            fs::create_dir_all(main_repo.layout.ozone_dir.join("integrate")).unwrap();
            fs::write(
                main_repo.layout.ozone_dir.join("integrate/session.json"),
                "{partial",
            )
            .unwrap();
        }
        let ghost_mount_path = main_repo.layout.ws_mount_record_path("ghost-ws");
        fs::create_dir_all(ghost_mount_path.parent().unwrap()).unwrap();
        let ghost_mount = oz_core::MountRecord {
            schema_version: oz_core::MountRecord::CURRENT_SCHEMA_VERSION,
            workspace: "ghost-ws".to_string(),
            branch: "feat/ghost".to_string(),
            backend: BackendKind::CasHardlink,
            repo_root: repo_dir.clone(),
            git_dir: main_repo.git_common_dir.clone(),
            mount_point: tmp.path().join("nonexistent/ghost-ws"),
            upper_dir: None,
            meta_dir: None,
            base_tree: None,
            base_commit: None,
            socket_path: None,
            daemon_pid: Some(999_999),
            mounted: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        fs::write(
            &ghost_mount_path,
            serde_json::to_string_pretty(&ghost_mount).unwrap(),
        )
        .unwrap();

        let repaired = fsck_repo(&main_repo, &FsckOptions { repair: true }).expect("fsck repair");
        prop_assert!(repaired.healthy, "expected healthy after repair: {repaired:?}");
        prop_assert_eq!(
            read_head_op_id(&main_repo.layout).unwrap(),
            Some(main_recent.op_id.clone())
        );

        // Zero user working-tree files deleted or altered in EITHER workspace!
        prop_assert_eq!(snapshot_worktree_disk(&repo_dir), expected_main_disk);
        prop_assert_eq!(snapshot_worktree_disk(&ws_dir), expected_ws_disk);

        // Active workspace still uncorrupted and undoable!
        undo(&ws_repo, UndoTarget::Steps(1), ws_actor).expect("undo ws_cp after gc+repair");
        assert_git_fsck_clean(&repo_dir);
    }
}
