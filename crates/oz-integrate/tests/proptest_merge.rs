// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Property-based safety tests for `oz integrate`, `oz resolve`, and post-merge `oz undo`
//! across unicode paths, symlinks, executable bits, renames, binary files, and nested directories.

use oz_core::{OzError, OzoneRepo};
use oz_integrate::{
    IntegrateOptions, ResolveOptions, TakeSide, active_session, continue_integration, integrate,
    resolve_conflict,
};
use oz_oplog::{ActorInfo, ActorKind, UndoTarget, undo};
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
enum SideEdit {
    ModifyTextBottom { suffix: String, make_exec: bool },
    ReplaceBytes { bytes: Vec<u8>, executable: bool },
    MakeSymlink { target: String },
    Delete,
    Rename { new_path: String },
}

#[derive(Debug, Clone)]
struct NonConflictCase {
    base: BTreeMap<String, TreeEntry>,
    main_edits: BTreeMap<String, SideEdit>,
    feat_edits: BTreeMap<String, SideEdit>,
    rename_feat_file: bool,
    shared_main_top: String,
    shared_feat_bottom: String,
}

#[derive(Debug, Clone)]
struct ConflictCase {
    base: BTreeMap<String, TreeEntry>,
    conflict_kind: u8,
    main_text: String,
    feat_text: String,
    main_bin: Vec<u8>,
    feat_bin: Vec<u8>,
    main_link: String,
    feat_link: String,
    main_exec: bool,
    feat_exec: bool,
    take_side: bool, // true => Ours, false => Theirs
    disjoint_main_extra: Vec<u8>,
    disjoint_feat_extra: Vec<u8>,
}

#[derive(Debug, Clone)]
struct MultiSourceCase {
    base: BTreeMap<String, TreeEntry>,
    b1_add_bytes: Vec<u8>,
    b2_add_bytes: Vec<u8>,
    b1_shared_top: String,
    b2_shared_bottom: String,
    dirty_untracked_bytes: Vec<u8>,
    dirty_symlink_target: String,
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

fn run_git_status_only(cwd: &Path, args: &[&str]) -> (bool, String, String) {
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
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn assert_git_fsck_clean(repo_dir: &Path) {
    let (ok, stdout, stderr) = run_git_status_only(repo_dir, &["fsck", "--full"]);
    assert!(
        ok,
        "git fsck --full failed in {}:\nstdout: {}\nstderr: {}",
        repo_dir.display(),
        stdout,
        stderr
    );
}

fn init_git_repo(dir: &Path) {
    fs::create_dir_all(dir).expect("mkdir repo");
    run_git(dir, &["init", "-b", "main"]);
    run_git(dir, &["config", "user.name", "PropTest Merge"]);
    run_git(dir, &["config", "user.email", "merge@ozone.test"]);
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
            fs::remove_file(&full).expect("remove old symlink");
        }
    }
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
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
            std::os::unix::fs::symlink(target, &full).expect("create symlink");
            #[cfg(not(unix))]
            fs::write(&full, target.as_bytes()).expect("write symlink fallback");
        }
    }
}

fn apply_side_edits(root: &Path, edits: &BTreeMap<String, SideEdit>) {
    for (rel, edit) in edits {
        match edit {
            SideEdit::ModifyTextBottom { suffix, make_exec } => {
                let mut text = fs::read_to_string(root.join(rel)).unwrap_or_default();
                text.push_str(suffix);
                write_entry(
                    root,
                    rel,
                    &TreeEntry::File {
                        bytes: text.into_bytes(),
                        executable: *make_exec,
                    },
                );
            }
            SideEdit::ReplaceBytes { bytes, executable } => {
                write_entry(
                    root,
                    rel,
                    &TreeEntry::File {
                        bytes: bytes.clone(),
                        executable: *executable,
                    },
                );
            }
            SideEdit::MakeSymlink { target } => {
                write_entry(
                    root,
                    rel,
                    &TreeEntry::Symlink {
                        target: target.clone(),
                    },
                );
            }
            SideEdit::Delete => {
                remove_path_and_clean(root, rel);
            }
            SideEdit::Rename { new_path } => {
                let full_src = root.join(rel);
                if let Ok(meta) = fs::symlink_metadata(&full_src) {
                    let entry = if meta.file_type().is_symlink() {
                        TreeEntry::Symlink {
                            target: fs::read_link(&full_src)
                                .unwrap()
                                .to_string_lossy()
                                .into_owned(),
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
                            bytes: fs::read(&full_src).unwrap(),
                            executable,
                        }
                    };
                    remove_path_and_clean(root, rel);
                    write_entry(root, new_path, &entry);
                }
            }
        }
    }
}

fn write_shared_source_file(root: &Path, rel: &str, top_tag: &str, bottom_tag: &str) {
    let mut lines = Vec::with_capacity(28);
    lines.push(format!("// TOP:{top_tag}"));
    for i in 1..=24 {
        lines.push(format!("pub fn anchor_fn_{i:02}() -> usize {{ {i} }}"));
    }
    lines.push(format!("// BOTTOM:{bottom_tag}"));
    write_entry(
        root,
        rel,
        &TreeEntry::File {
            bytes: (lines.join("\n") + "\n").into_bytes(),
            executable: false,
        },
    );
}

fn test_actor() -> ActorInfo {
    ActorInfo {
        kind: ActorKind::Agent,
        client: Some("proptest-merge".to_string()),
        workspace: None,
    }
}

fn arb_safe_token() -> BoxedStrategy<String> {
    prop::string::string_regex("[a-zA-Z0-9_]{3,14}")
        .expect("token regex")
        .boxed()
}

fn arb_binary_bytes() -> BoxedStrategy<Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..96)
        .prop_map(|mut v| {
            // Ensure binary detection sees NUL bytes regularly.
            if !v.contains(&0) {
                v.insert(0, 0);
            }
            v
        })
        .boxed()
}

fn arb_symlink_target() -> BoxedStrategy<String> {
    prop_oneof![
        Just("../README.md".to_string()),
        Just("../docs/résumé-日本語.md".to_string()),
        Just("../scripts/run-café.sh".to_string()),
        Just("nested/target.txt".to_string()),
    ]
    .boxed()
}

fn arb_base_snapshot() -> BoxedStrategy<BTreeMap<String, TreeEntry>> {
    (
        arb_safe_token(),
        arb_binary_bytes(),
        any::<bool>(),
        arb_symlink_target(),
    )
        .prop_map(|(tok, bin, exec, sym)| {
            let mut m = BTreeMap::new();
            m.insert(
                "README.md".to_string(),
                TreeEntry::File {
                    bytes: format!("# Repo {tok}\nbase line\n").into_bytes(),
                    executable: false,
                },
            );
            m.insert(
                "docs/résumé-日本語.md".to_string(),
                TreeEntry::File {
                    bytes: format!("Résumé header {tok}\nLine 2\n").into_bytes(),
                    executable: false,
                },
            );
            m.insert(
                "scripts/run-café.sh".to_string(),
                TreeEntry::File {
                    bytes: format!("#!/bin/sh\necho \"{tok}\"\n").into_bytes(),
                    executable: exec,
                },
            );
            m.insert(
                "assets/blob.bin".to_string(),
                TreeEntry::File {
                    bytes: bin,
                    executable: false,
                },
            );
            m.insert(
                "links/latest".to_string(),
                TreeEntry::Symlink { target: sym },
            );
            m.insert(
                "deep/a/b/c/mod.rs".to_string(),
                TreeEntry::File {
                    bytes: (1..=12)
                        .map(|i| format!("pub const C_{i}: usize = {i}; // {tok}\n"))
                        .collect::<String>()
                        .into_bytes(),
                    executable: false,
                },
            );
            m.insert(
                "main_only/item.txt".to_string(),
                TreeEntry::File {
                    bytes: format!("main side {tok}\n").into_bytes(),
                    executable: false,
                },
            );
            m.insert(
                "feat_only/item-α.txt".to_string(),
                TreeEntry::File {
                    bytes: format!("feat side {tok}\n").into_bytes(),
                    executable: false,
                },
            );
            m
        })
        .boxed()
}

fn arb_non_conflict_case() -> BoxedStrategy<NonConflictCase> {
    (
        arb_base_snapshot(),
        arb_safe_token(),
        arb_safe_token(),
        arb_binary_bytes(),
        any::<bool>(),
        any::<bool>(),
        arb_symlink_target(),
        any::<bool>(),
        arb_safe_token(),
        arb_safe_token(),
    )
        .prop_map(
            |(
                base,
                m_tok,
                f_tok,
                f_bin,
                m_exec,
                f_delete_main_only_never_collision,
                f_sym,
                rename_feat_file,
                shared_main_top,
                shared_feat_bottom,
            )| {
                let mut main_edits = BTreeMap::new();
                main_edits.insert(
                    "main_only/item.txt".to_string(),
                    SideEdit::ReplaceBytes {
                        bytes: format!("updated on main: {m_tok}\n").into_bytes(),
                        executable: false,
                    },
                );
                main_edits.insert(
                    "scripts/run-café.sh".to_string(),
                    SideEdit::ModifyTextBottom {
                        suffix: format!("echo \"{m_tok}\"\n"),
                        make_exec: m_exec,
                    },
                );
                main_edits.insert(
                    "new_dir/café/added_by_main.txt".to_string(),
                    SideEdit::ReplaceBytes {
                        bytes: format!("main new {m_tok}\n").into_bytes(),
                        executable: m_exec,
                    },
                );

                let mut feat_edits = BTreeMap::new();
                feat_edits.insert(
                    "assets/blob.bin".to_string(),
                    SideEdit::ReplaceBytes {
                        bytes: f_bin,
                        executable: false,
                    },
                );
                feat_edits.insert(
                    "links/latest".to_string(),
                    SideEdit::MakeSymlink { target: f_sym },
                );
                if rename_feat_file {
                    feat_edits.insert(
                        "deep/a/b/c/mod.rs".to_string(),
                        SideEdit::Rename {
                            new_path: "nested/relocated/mod-café.rs".to_string(),
                        },
                    );
                } else {
                    feat_edits.insert(
                        "deep/a/b/c/mod.rs".to_string(),
                        SideEdit::ModifyTextBottom {
                            suffix: format!("pub const EXTRA: &str = \"{f_tok}\";\n"),
                            make_exec: false,
                        },
                    );
                }
                if f_delete_main_only_never_collision {
                    feat_edits.insert("feat_only/item-α.txt".to_string(), SideEdit::Delete);
                } else {
                    feat_edits.insert(
                        "docs/résumé-日本語.md".to_string(),
                        SideEdit::ModifyTextBottom {
                            suffix: format!("Added {f_tok}\n"),
                            make_exec: false,
                        },
                    );
                }
                feat_edits.insert(
                    "feat_added/λ/exec.sh".to_string(),
                    SideEdit::ReplaceBytes {
                        bytes: format!("#!/bin/sh\n# {f_tok}\n").into_bytes(),
                        executable: true,
                    },
                );

                NonConflictCase {
                    base,
                    main_edits,
                    feat_edits,
                    rename_feat_file,
                    shared_main_top,
                    shared_feat_bottom,
                }
            },
        )
        .boxed()
}

fn arb_conflict_case() -> BoxedStrategy<ConflictCase> {
    (
        (
            arb_base_snapshot(),
            0u8..4,
            arb_safe_token(),
            arb_safe_token(),
            arb_binary_bytes(),
            arb_binary_bytes(),
            arb_symlink_target(),
            arb_symlink_target(),
        ),
        (
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            prop::collection::vec(any::<u8>(), 1..32),
            prop::collection::vec(any::<u8>(), 1..32),
        ),
    )
        .prop_filter("distinct conflict payloads", |tuple| {
            let ((_, kind, mt, ft, mb, fb, ml, fl), (me, fe, _, _, _)) = tuple;
            match kind {
                0 => mt != ft,
                1 => mb != fb,
                2 => ml != fl,
                _ => mt != ft || me != fe,
            }
        })
        .prop_map(
            |(
                (
                    base,
                    conflict_kind,
                    main_text,
                    feat_text,
                    main_bin,
                    feat_bin,
                    main_link,
                    feat_link,
                ),
                (
                    main_exec,
                    feat_exec,
                    take_side,
                    disjoint_main_extra,
                    disjoint_feat_extra,
                ),
            )| ConflictCase {
                base,
                conflict_kind,
                main_text,
                feat_text,
                main_bin,
                feat_bin,
                main_link,
                feat_link,
                main_exec,
                feat_exec,
                take_side,
                disjoint_main_extra,
                disjoint_feat_extra,
            },
        )
        .boxed()
}

fn arb_multi_source_case() -> BoxedStrategy<MultiSourceCase> {
    (
        arb_base_snapshot(),
        arb_binary_bytes(),
        arb_binary_bytes(),
        arb_safe_token(),
        arb_safe_token(),
        prop::collection::vec(any::<u8>(), 1..48),
        arb_symlink_target(),
    )
        .prop_map(
            |(
                base,
                b1_add_bytes,
                b2_add_bytes,
                b1_shared_top,
                b2_shared_bottom,
                dirty_untracked_bytes,
                dirty_symlink_target,
            )| MultiSourceCase {
                base,
                b1_add_bytes,
                b2_add_bytes,
                b1_shared_top,
                b2_shared_bottom,
                dirty_untracked_bytes,
                dirty_symlink_target,
            },
        )
        .boxed()
}

fn seed_base_in_both_repos(oz_dir: &Path, ref_dir: &Path, base: &BTreeMap<String, TreeEntry>) {
    for dir in [oz_dir, ref_dir] {
        init_git_repo(dir);
        for (rel, entry) in base {
            write_entry(dir, rel, entry);
        }
        write_shared_source_file(dir, "src/shared_merge.rs", "BASE", "BASE");
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-m", "base commit"]);
    }
}

proptest! {
    #![proptest_config(proptest_config(28))]

    #[test]
    fn proptest_non_conflicting_three_way_merge_matches_git(case in arb_non_conflict_case()) {
        let tmp = TempDir::new().expect("tempdir");
        let oz_dir = tmp.path().join("oz-repo");
        let ref_dir = tmp.path().join("ref-repo");
        seed_base_in_both_repos(&oz_dir, &ref_dir, &case.base);
        let base_oid = run_git(&oz_dir, &["rev-parse", "HEAD"]);

        for dir in [&oz_dir, &ref_dir] {
            run_git(dir, &["checkout", "-b", "feat/agent", &base_oid]);
            apply_side_edits(dir, &case.feat_edits);
            write_shared_source_file(
                dir,
                "src/shared_merge.rs",
                "BASE",
                &case.shared_feat_bottom,
            );
            run_git(dir, &["add", "-A"]);
            run_git(dir, &["commit", "-m", "feat work"]);

            run_git(dir, &["checkout", "main"]);
            apply_side_edits(dir, &case.main_edits);
            write_shared_source_file(
                dir,
                "src/shared_merge.rs",
                &case.shared_main_top,
                "BASE",
            );
            run_git(dir, &["add", "-A"]);
            run_git(dir, &["commit", "-m", "main work"]);
        }

        let pre_main_oid = run_git(&oz_dir, &["rev-parse", "main"]);
        let pre_main_tree_oid = run_git(&oz_dir, &["rev-parse", "main^{tree}"]);
        let pre_main_snapshot = snapshot_worktree_disk(&oz_dir);

        // Reference `git merge --no-ff`.
        run_git(&ref_dir, &["merge", "--no-ff", "feat/agent", "-m", "merge feat/agent"]);
        let ref_tree_oid = run_git(&ref_dir, &["rev-parse", "HEAD^{tree}"]);
        let ref_snapshot = snapshot_worktree_disk(&ref_dir);

        // Ozone `oz integrate`.
        let repo = OzoneRepo::discover(Some(&oz_dir)).expect("discover oz");
        let outcome = integrate(
            &repo,
            &IntegrateOptions {
                sources: vec!["feat/agent".to_string()],
                into: Some("main".to_string()),
                verify_cmd: None,
                allow_out_of_scope: true,
                force: true,
                message: Some("merge feat/agent".to_string()),
                ..Default::default()
            },
            test_actor(),
        )
        .expect("oz integrate non-conflicting");

        prop_assert_eq!(outcome.previous_oid.as_str(), pre_main_oid.as_str());
        prop_assert_eq!(outcome.tree_oid.as_str(), ref_tree_oid.as_str());
        prop_assert!(outcome.main_worktree_synced);
        prop_assert_eq!(
            run_git(&oz_dir, &["rev-parse", "main^{tree}"]),
            ref_tree_oid
        );
        prop_assert!(
            run_git(&oz_dir, &["status", "--porcelain"]).is_empty(),
            "main index must be completely synced after oz integrate"
        );
        let oz_snapshot = snapshot_worktree_disk(&oz_dir);
        prop_assert_eq!(&oz_snapshot, &ref_snapshot);

        if case.rename_feat_file {
            prop_assert!(oz_snapshot.contains_key("nested/relocated/mod-café.rs"));
            prop_assert!(!oz_snapshot.contains_key("deep/a/b/c/mod.rs"));
        }

        // Verify `oz undo` restores pre-merge state cleanly bit-for-bit.
        let repo_reloaded = OzoneRepo::discover(Some(&oz_dir)).expect("rediscover");
        undo(&repo_reloaded, UndoTarget::Steps(1), test_actor()).expect("undo integrate");
        prop_assert_eq!(run_git(&oz_dir, &["rev-parse", "main"]), pre_main_oid);
        prop_assert_eq!(
            run_git(&oz_dir, &["rev-parse", "main^{tree}"]),
            pre_main_tree_oid
        );
        prop_assert_eq!(snapshot_worktree_disk(&oz_dir), pre_main_snapshot);

        assert_git_fsck_clean(&oz_dir);
    }

    #[test]
    fn proptest_conflict_resolve_and_undo_invariants(case in arb_conflict_case()) {
        let tmp = TempDir::new().expect("tempdir");
        let oz_dir = tmp.path().join("oz-conflict");
        let ref_dir = tmp.path().join("ref-conflict");
        seed_base_in_both_repos(&oz_dir, &ref_dir, &case.base);
        let base_oid = run_git(&oz_dir, &["rev-parse", "HEAD"]);

        let target_rel = match case.conflict_kind {
            0 => "docs/résumé-日本語.md",
            1 => "assets/blob.bin",
            2 => "links/latest",
            _ => "scripts/run-café.sh",
        };

        let make_target_entry = |is_main: bool| -> TreeEntry {
            let side = if is_main { "M" } else { "F" };
            match case.conflict_kind {
                0 => {
                    let t = if is_main { &case.main_text } else { &case.feat_text };
                    TreeEntry::File {
                        bytes: format!("line1 {side}:{t}\nline2 conflict\n").into_bytes(),
                        executable: false,
                    }
                }
                1 => {
                    let mut bytes = vec![if is_main { b'M' } else { b'F' }, 0];
                    bytes.extend_from_slice(if is_main {
                        &case.main_bin
                    } else {
                        &case.feat_bin
                    });
                    TreeEntry::File {
                        bytes,
                        executable: false,
                    }
                }
                2 => TreeEntry::Symlink {
                    target: format!(
                        "{side}/{}",
                        if is_main {
                            &case.main_link
                        } else {
                            &case.feat_link
                        }
                    ),
                },
                _ => {
                    let t = if is_main { &case.main_text } else { &case.feat_text };
                    let ex = if is_main { case.main_exec } else { case.feat_exec };
                    TreeEntry::File {
                        bytes: format!("#!/bin/sh\necho \"{side}:{t}\"\n").into_bytes(),
                        executable: ex,
                    }
                }
            }
        };

        for dir in [&oz_dir, &ref_dir] {
            run_git(dir, &["checkout", "-b", "feat/conflict", &base_oid]);
            write_entry(dir, target_rel, &make_target_entry(false));
            write_entry(
                dir,
                "disjoint/feat-café.bin",
                &TreeEntry::File {
                    bytes: case.disjoint_feat_extra.clone(),
                    executable: false,
                },
            );
            run_git(dir, &["add", "-A"]);
            run_git(dir, &["commit", "-m", "feat conflicting commit"]);

            run_git(dir, &["checkout", "main"]);
            write_entry(dir, target_rel, &make_target_entry(true));
            write_entry(
                dir,
                "disjoint/main-日本語.bin",
                &TreeEntry::File {
                    bytes: case.disjoint_main_extra.clone(),
                    executable: false,
                },
            );
            run_git(dir, &["add", "-A"]);
            run_git(dir, &["commit", "-m", "main conflicting commit"]);
        }

        let pre_main_oid = run_git(&oz_dir, &["rev-parse", "main"]);
        let pre_main_snapshot = snapshot_worktree_disk(&oz_dir);

        // Compute exact expected resolved tree in `ref_dir`.
        let (ok, _, _) = run_git_status_only(&ref_dir, &["merge", "--no-ff", "feat/conflict"]);
        prop_assert!(!ok, "reference git merge must conflict on {:?}", target_rel);
        let take_flag = if case.take_side { "--ours" } else { "--theirs" };
        run_git(&ref_dir, &["checkout", take_flag, "--", target_rel]);
        run_git(&ref_dir, &["add", "--", target_rel]);
        run_git(&ref_dir, &["commit", "-m", "resolved merge"]);
        let ref_tree_oid = run_git(&ref_dir, &["rev-parse", "HEAD^{tree}"]);
        let ref_snapshot = snapshot_worktree_disk(&ref_dir);

        // Step 1: `oz integrate` conflicts without mutating `main`!
        let repo = OzoneRepo::discover(Some(&oz_dir)).expect("discover oz conflict");
        let err = integrate(
            &repo,
            &IntegrateOptions {
                sources: vec!["feat/conflict".to_string()],
                into: Some("main".to_string()),
                verify_cmd: None,
                allow_out_of_scope: true,
                force: true,
                message: Some("resolved merge".to_string()),
                ..Default::default()
            },
            test_actor(),
        )
        .expect_err("expected conflict");
        let OzError::Conflict { conflicts, .. } = err else {
            return Err(proptest::test_runner::TestCaseError::fail(format!(
                "expected OzError::Conflict, got {err:?}"
            )));
        };
        prop_assert!(!conflicts.is_empty());
        prop_assert_eq!(run_git(&oz_dir, &["rev-parse", "main"]), pre_main_oid.clone());
        prop_assert_eq!(snapshot_worktree_disk(&oz_dir), pre_main_snapshot.clone());
        prop_assert!(active_session(&repo).expect("session").is_some());

        // Step 2: `oz resolve --all --take ours|theirs` + `continue_integration`.
        let side = if case.take_side {
            TakeSide::Ours
        } else {
            TakeSide::Theirs
        };
        let resolved = resolve_conflict(
            &repo,
            &ResolveOptions {
                conflict_id: None,
                all: true,
                take: side,
                auto_continue: false,
            },
            test_actor(),
        )
        .expect("resolve conflict");
        prop_assert!(resolved.remaining_unresolved.is_empty());

        let cont = continue_integration(&repo, test_actor()).expect("continue integration");
        prop_assert_eq!(cont.tree_oid.as_str(), ref_tree_oid.as_str());
        prop_assert_eq!(
            run_git(&oz_dir, &["rev-parse", "main^{tree}"]),
            ref_tree_oid
        );
        prop_assert_eq!(snapshot_worktree_disk(&oz_dir), ref_snapshot);
        prop_assert!(active_session(&repo).expect("cleared session").is_none());

        // Step 3: `oz undo` cleanly rolls back the resolved integration commit.
        let repo_reloaded = OzoneRepo::discover(Some(&oz_dir)).expect("rediscover");
        undo(&repo_reloaded, UndoTarget::Steps(1), test_actor()).expect("undo resolved merge");
        prop_assert_eq!(run_git(&oz_dir, &["rev-parse", "main"]), pre_main_oid);
        prop_assert_eq!(snapshot_worktree_disk(&oz_dir), pre_main_snapshot);

        assert_git_fsck_clean(&oz_dir);
    }

    #[test]
    fn proptest_multi_source_integration_preserves_unconflicting_dirty_edits(
        case in arb_multi_source_case()
    ) {
        let tmp = TempDir::new().expect("tempdir");
        let oz_dir = tmp.path().join("oz-multi");
        let ref_dir = tmp.path().join("ref-multi");
        seed_base_in_both_repos(&oz_dir, &ref_dir, &case.base);
        let base_oid = run_git(&oz_dir, &["rev-parse", "HEAD"]);

        for dir in [&oz_dir, &ref_dir] {
            run_git(dir, &["checkout", "-b", "feat/one", &base_oid]);
            write_shared_source_file(dir, "src/shared_merge.rs", &case.b1_shared_top, "BASE");
            write_entry(
                dir,
                "multi/café/b1.bin",
                &TreeEntry::File {
                    bytes: case.b1_add_bytes.clone(),
                    executable: false,
                },
            );
            run_git(dir, &["add", "-A"]);
            run_git(dir, &["commit", "-m", "feat one"]);

            run_git(dir, &["checkout", "-b", "feat/two", &base_oid]);
            write_shared_source_file(dir, "src/shared_merge.rs", "BASE", &case.b2_shared_bottom);
            write_entry(
                dir,
                "multi/nested/run-β.sh",
                &TreeEntry::File {
                    bytes: case.b2_add_bytes.clone(),
                    executable: true,
                },
            );
            run_git(dir, &["add", "-A"]);
            run_git(dir, &["commit", "-m", "feat two"]);

            run_git(dir, &["checkout", "main"]);
        }

        // Reference sequential merges into main.
        run_git(&ref_dir, &["merge", "--no-ff", "feat/one", "-m", "merge one"]);
        run_git(&ref_dir, &["merge", "--no-ff", "feat/two", "-m", "merge two"]);
        let ref_tree_oid = run_git(&ref_dir, &["rev-parse", "HEAD^{tree}"]);
        let mut expected_snapshot = snapshot_worktree_disk(&ref_dir);

        // Place unrelated untracked file + symlink in `oz_dir` and confirm neither is clobbered.
        let dirty_file_entry = TreeEntry::File {
            bytes: case.dirty_untracked_bytes.clone(),
            executable: false,
        };
        let dirty_sym_entry = TreeEntry::Symlink {
            target: case.dirty_symlink_target.clone(),
        };
        write_entry(&oz_dir, "local_notes/scratch-π.bin", &dirty_file_entry);
        write_entry(&oz_dir, "local_notes/sym_note", &dirty_sym_entry);
        expected_snapshot.insert("local_notes/scratch-π.bin".to_string(), dirty_file_entry);
        let expected_sym_entry = if cfg!(unix) {
            dirty_sym_entry
        } else {
            TreeEntry::File {
                bytes: case.dirty_symlink_target.clone().into_bytes(),
                executable: false,
            }
        };
        expected_snapshot.insert("local_notes/sym_note".to_string(), expected_sym_entry);

        let repo = OzoneRepo::discover(Some(&oz_dir)).expect("discover multi");
        let outcome = integrate(
            &repo,
            &IntegrateOptions {
                sources: vec!["feat/one".to_string(), "feat/two".to_string()],
                into: Some("main".to_string()),
                verify_cmd: None,
                allow_out_of_scope: true,
                force: true,
                message: None,
                ..Default::default()
            },
            test_actor(),
        )
        .expect("multi-source integrate");

        prop_assert_eq!(outcome.merged_sources, vec!["feat/one", "feat/two"]);
        prop_assert_eq!(outcome.tree_oid.as_str(), ref_tree_oid.as_str());
        prop_assert_eq!(
            run_git(&oz_dir, &["rev-parse", "main^{tree}"]),
            ref_tree_oid
        );
        prop_assert_eq!(snapshot_worktree_disk(&oz_dir), expected_snapshot);

        // Every tracked path committed on `main` matches reference repo (`ls-tree -r` parity).
        let oz_tracked: BTreeSet<String> = run_git(&oz_dir, &["ls-tree", "-r", "--name-only", "main"])
            .lines()
            .map(ToOwned::to_owned)
            .collect();
        let ref_tracked: BTreeSet<String> =
            run_git(&ref_dir, &["ls-tree", "-r", "--name-only", "main"])
                .lines()
                .map(ToOwned::to_owned)
                .collect();
        prop_assert_eq!(oz_tracked, ref_tracked);

        assert_git_fsck_clean(&oz_dir);
    }
}
