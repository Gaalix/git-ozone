// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Property-based safety tests (`proptest`) for `oz-core`:
//! `oz commit` (`--pick` / `--exclude` by file ID, hunk ID, path, dir prefix; `--amend`)
//! across unicode paths, symlinks, executable bits, renames, binary files, and nested directories.
//!
//! Invariants verified on every generated case:
//! 1. Zero lost edits (remaining working tree matches reference `git` bit-for-bit).
//! 2. Committed `HEAD^{tree}` OID matches reference `git` bit-for-bit.
//! 3. `git fsck --full` passes cleanly after operations.

use oz_core::{CommitOptions, OzoneRepo, commit_changes, compute_status};
use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, FileFailurePersistence};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn proptest_config(default_cases: u32) -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&c| c > 0)
        .unwrap_or(default_cases);
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

fn run_git(dir: &Path, args: &[&str], stdin_bytes: Option<&[u8]>) -> String {
    let mut cmd = Command::new(sys_git_bin());
    cmd.args(["-c", "core.quotePath=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_DATE", "2026-01-15T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-15T12:00:00Z");
    if stdin_bytes.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn git");
    if let Some(bytes) = stdin_bytes {
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(bytes).expect("write git stdin");
    }
    let out = child.wait_with_output().expect("wait git");
    assert!(
        out.status.success(),
        "git {:?} failed in {}:\nstdout: {}\nstderr: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn assert_git_fsck_clean(dir: &Path) {
    let out = Command::new(sys_git_bin())
        .args(["fsck", "--full"])
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .expect("run git fsck --full");
    assert!(
        out.status.success(),
        "git fsck --full failed in {}:\nstdout: {}\nstderr: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum WorktreeEntry {
    File { bytes: Vec<u8>, executable: bool },
    Symlink { target: Vec<u8> },
}

fn snapshot_worktree(root: &Path) -> BTreeMap<String, WorktreeEntry> {
    fn walk(base: &Path, cur: &Path, out: &mut BTreeMap<String, WorktreeEntry>) {
        let mut entries: Vec<_> = fs::read_dir(cur)
            .expect("read_dir")
            .map(|r| r.expect("dir entry"))
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).expect("symlink_metadata");
            let ft = meta.file_type();
            if ft.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .expect("strip_prefix")
                    .to_str()
                    .expect("utf8 path")
                    .replace('\\', "/");
                if ft.is_symlink() {
                    let target = fs::read_link(&path).expect("read_link");
                    #[cfg(unix)]
                    let target_bytes = {
                        use std::os::unix::ffi::OsStrExt;
                        target.as_os_str().as_bytes().to_vec()
                    };
                    #[cfg(not(unix))]
                    let target_bytes = target.to_string_lossy().as_bytes().to_vec();
                    out.insert(rel, WorktreeEntry::Symlink { target: target_bytes });
                } else if ft.is_file() {
                    #[cfg(unix)]
                    let executable = {
                        use std::os::unix::fs::PermissionsExt;
                        (meta.permissions().mode() & 0o111) != 0
                    };
                    #[cfg(not(unix))]
                    let executable = false;
                    let bytes = fs::read(&path).expect("read file");
                    out.insert(rel, WorktreeEntry::File { bytes, executable });
                }
            }
        }
    }
    let mut map = BTreeMap::new();
    walk(root, root, &mut map);
    map
}

fn write_worktree_entry(root: &Path, rel: &str, entry: &WorktreeEntry) {
    let full = root.join(rel);
    if let Ok(meta) = fs::symlink_metadata(&full) {
        if meta.is_dir() {
            fs::remove_dir_all(&full).expect("rm conflicting dir");
        } else {
            fs::remove_file(&full).expect("rm existing file");
        }
    }
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    match entry {
        WorktreeEntry::File { bytes, executable } => {
            fs::write(&full, bytes).expect("write file");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if *executable { 0o755 } else { 0o644 };
                fs::set_permissions(&full, fs::Permissions::from_mode(mode)).expect("chmod");
            }
            let _ = executable;
        }
        WorktreeEntry::Symlink { target } => {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                let os_target = std::ffi::OsStr::from_bytes(target);
                std::os::unix::fs::symlink(os_target, &full).expect("symlink");
            }
            #[cfg(not(unix))]
            {
                fs::write(&full, target).expect("write link fallback");
            }
        }
    }
}

fn remove_worktree_path(root: &Path, rel: &str) {
    let full = root.join(rel);
    if let Ok(meta) = fs::symlink_metadata(&full) {
        if meta.is_dir() {
            fs::remove_dir_all(&full).expect("rm dir");
        } else {
            fs::remove_file(&full).expect("rm file");
        }
    }
    let mut cur = full.parent();
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

fn init_paired_repos(initial: &BTreeMap<String, WorktreeEntry>) -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let oz_dir = tmp.path().join("oz_repo");
    let git_dir = tmp.path().join("git_repo");
    for dir in [&oz_dir, &git_dir] {
        fs::create_dir_all(dir).expect("mkdir repo");
        run_git(dir, &["init", "-b", "main"], None);
        run_git(dir, &["config", "user.name", "Ozone PropTest"], None);
        run_git(dir, &["config", "user.email", "proptest@ozone.dev"], None);
        run_git(dir, &["config", "core.quotePath", "false"], None);
        for (rel, entry) in initial {
            write_worktree_entry(dir, rel, entry);
        }
        run_git(dir, &["add", "-A"], None);
        run_git(dir, &["commit", "--allow-empty", "-m", "base commit"], None);
    }
    (tmp, oz_dir, git_dir)
}

const FILE_PATHS: &[&str] = &[
    "README.md",
    "src/lib.rs",
    "src/nested/deep/core.rs",
    "scripts/deploy.sh",
    "café/résumé.txt",
    "日本語/データ.txt",
    "dir_αβγ/файл.md",
    "emoji_🚀/note.bin",
    "nested/sub/bin.dat",
    "links/ptr_a",
    "café/links/ptr_b",
];

fn arb_entry_for_path(path: &'static str) -> BoxedStrategy<WorktreeEntry> {
    if path.contains("ptr_") {
        prop_oneof![
            Just(WorktreeEntry::Symlink {
                target: b"../README.md".to_vec()
            }),
            Just(WorktreeEntry::Symlink {
                target: b"../caf\xc3\xa9/r\xc3\xa9sum\xc3\xa9.txt".to_vec()
            }),
            Just(WorktreeEntry::File {
                bytes: b"regular-replacement\n".to_vec(),
                executable: false,
            }),
        ]
        .boxed()
    } else if path.ends_with(".bin") || path.ends_with(".dat") {
        proptest::collection::vec(any::<u8>(), 1..=96)
            .prop_map(|mut bytes| {
                // Ensure binary detection sees NUL bytes reliably.
                if !bytes.contains(&0) {
                    bytes.push(0);
                }
                WorktreeEntry::File {
                    bytes,
                    executable: false,
                }
            })
            .boxed()
    } else {
        (any::<bool>(), proptest::collection::vec("[a-zA-Z0-9 _\n]{1,24}", 1..=5))
            .prop_map(move |(exec, lines)| {
                let mut content = lines.join("\n");
                content.push('\n');
                let executable = if path.ends_with(".sh") { exec } else { exec && path.contains("core") };
                WorktreeEntry::File {
                    bytes: content.into_bytes(),
                    executable,
                }
            })
            .boxed()
    }
}

#[derive(Debug, Clone)]
enum FileMutation {
    Upsert { path: &'static str, entry: WorktreeEntry },
    Delete { path: &'static str },
    Rename { from: &'static str, to: &'static str, new_bytes: Option<Vec<u8>> },
    ToggleExec { path: &'static str },
}

fn arb_initial_tree() -> BoxedStrategy<BTreeMap<String, WorktreeEntry>> {
    let base_paths: Vec<&'static str> = vec![
        "README.md",
        "src/lib.rs",
        "src/nested/deep/core.rs",
        "scripts/deploy.sh",
        "café/résumé.txt",
        "日本語/データ.txt",
        "nested/sub/bin.dat",
        "links/ptr_a",
    ];
    let strats: Vec<BoxedStrategy<(&'static str, WorktreeEntry)>> = base_paths
        .into_iter()
        .map(|p| arb_entry_for_path(p).prop_map(move |e| (p, e)).boxed())
        .collect();
    strats
        .prop_map(|items| items.into_iter().collect::<BTreeMap<_, _>>())
        .prop_map(|map| {
            map.into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect()
        })
        .boxed()
}

fn arb_mutation_batch() -> BoxedStrategy<Vec<FileMutation>> {
    let path_strat = proptest::sample::select(FILE_PATHS);
    let mut_strat = prop_oneof![
        path_strat
            .clone()
            .prop_flat_map(|p| arb_entry_for_path(p).prop_map(move |entry| FileMutation::Upsert { path: p, entry })),
        path_strat.clone().prop_map(|path| FileMutation::Delete { path }),
        (
            proptest::sample::select(&["README.md", "café/résumé.txt", "nested/sub/bin.dat"][..]),
            proptest::sample::select(&["dir_αβγ/файл.md", "emoji_🚀/note.bin", "src/nested/deep/core.rs"][..]),
            proptest::option::of(proptest::collection::vec(any::<u8>(), 4..=32)),
        )
            .prop_map(|(from, to, new_bytes)| FileMutation::Rename { from, to, new_bytes }),
        proptest::sample::select(&["scripts/deploy.sh", "src/nested/deep/core.rs"][..])
            .prop_map(|path| FileMutation::ToggleExec { path }),
    ];
    proptest::collection::vec(mut_strat, 1..=6).boxed()
}

fn apply_mutation(dir: &Path, m: &FileMutation) {
    match m {
        FileMutation::Upsert { path, entry } => write_worktree_entry(dir, path, entry),
        FileMutation::Delete { path } => remove_worktree_path(dir, path),
        FileMutation::Rename { from, to, new_bytes } => {
            let from_full = dir.join(from);
            if let Ok(meta) = fs::symlink_metadata(&from_full)
                && meta.is_file()
            {
                let data = match new_bytes {
                    Some(b) => b.clone(),
                    None => fs::read(&from_full).expect("read rename src"),
                };
                #[cfg(unix)]
                let exec = {
                    use std::os::unix::fs::PermissionsExt;
                    (meta.permissions().mode() & 0o111) != 0
                };
                #[cfg(not(unix))]
                let exec = false;
                remove_worktree_path(dir, from);
                write_worktree_entry(
                    dir,
                    to,
                    &WorktreeEntry::File {
                        bytes: data,
                        executable: exec,
                    },
                );
            }
        }
        FileMutation::ToggleExec { path } => {
            let full = dir.join(path);
            if let Ok(meta) = fs::symlink_metadata(&full)
                && meta.is_file()
            {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let cur = (meta.permissions().mode() & 0o111) != 0;
                    let next = if cur { 0o644 } else { 0o755 };
                    fs::set_permissions(&full, fs::Permissions::from_mode(next)).expect("chmod");
                }
            }
        }
    }
}

fn extract_hunk_patch(full_diff: &str, hunk_indices: &BTreeSet<usize>) -> String {
    let mut header = String::new();
    let mut hunks: Vec<String> = Vec::new();
    let mut current_hunk = String::new();
    let mut in_hunk = false;

    for line in full_diff.split_inclusive('\n') {
        if line.starts_with("@@ ") {
            if in_hunk && !current_hunk.is_empty() {
                hunks.push(std::mem::take(&mut current_hunk));
            }
            in_hunk = true;
            current_hunk.push_str(line);
        } else if in_hunk {
            current_hunk.push_str(line);
        } else {
            header.push_str(line);
        }
    }
    if in_hunk && !current_hunk.is_empty() {
        hunks.push(current_hunk);
    }

    let mut out = header;
    for &idx in hunk_indices {
        if let Some(h) = hunks.get(idx) {
            out.push_str(h);
        }
    }
    out
}

proptest! {
    #![proptest_config(proptest_config(28))]

    #[test]
    fn proptest_whole_and_partial_pick_exclude_matches_git(
        initial in arb_initial_tree(),
        mutations in arb_mutation_batch(),
        pick_mask in proptest::collection::vec(any::<bool>(), 1..=12),
        use_exclude_mode in any::<bool>(),
        use_file_ids in any::<bool>(),
    ) {
        let (_tmp, oz_dir, git_dir) = init_paired_repos(&initial);

        for m in &mutations {
            apply_mutation(&oz_dir, m);
            apply_mutation(&git_dir, m);
        }

        // Baseline sanity: working trees start identical.
        prop_assert_eq!(snapshot_worktree(&oz_dir), snapshot_worktree(&git_dir));

        let oz_repo = OzoneRepo::discover(Some(&oz_dir)).expect("discover oz");
        let status = compute_status(&oz_repo).expect("compute status");
        if status.changes.is_empty() {
            return Ok(());
        }

        let mut picked_indices = BTreeSet::new();
        for (idx, _) in status.changes.iter().enumerate() {
            if pick_mask[idx % pick_mask.len()] {
                picked_indices.insert(idx);
            }
        }
        if picked_indices.is_empty() {
            picked_indices.insert(0);
        }

        // Build either `--pick` or `--exclude` targets covering `picked_indices`.
        let mut opts = CommitOptions::new("feat: randomized partial commit");
        opts.force = true;

        if use_exclude_mode && picked_indices.len() < status.changes.len() {
            for (idx, ch) in status.changes.iter().enumerate() {
                if !picked_indices.contains(&idx) {
                    let target = if use_file_ids { ch.id.clone() } else { ch.path.clone() };
                    opts.exclude.push(target);
                }
            }
        } else {
            for &idx in &picked_indices {
                let ch = &status.changes[idx];
                let target = if use_file_ids { ch.id.clone() } else { ch.path.clone() };
                opts.pick.push(target);
            }
        }

        commit_changes(&oz_repo, &opts).expect("oz commit");

        // Mirror exact picked paths in reference `git`.
        let mut git_add_args = vec!["add", "-A", "--"];
        let picked_paths: Vec<String> = picked_indices
            .iter()
            .map(|&i| status.changes[i].path.clone())
            .collect();
        for p in &picked_paths {
            git_add_args.push(p.as_str());
        }
        run_git(&git_dir, &git_add_args, None);
        run_git(
            &git_dir,
            &["commit", "--allow-empty", "-m", "feat: randomized partial commit"],
            None,
        );

        let oz_tree = run_git(&oz_dir, &["rev-parse", "HEAD^{tree}"], None);
        let git_tree = run_git(&git_dir, &["rev-parse", "HEAD^{tree}"], None);
        prop_assert_eq!(oz_tree, git_tree, "committed tree OID mismatch");
        prop_assert_eq!(
            snapshot_worktree(&oz_dir),
            snapshot_worktree(&git_dir),
            "remaining dirty working tree mismatch"
        );
        assert_git_fsck_clean(&oz_dir);

        // Commit any remaining dirty changes and confirm final trees still match bit-for-bit.
        let rem_status = compute_status(&oz_repo).expect("rem status");
        if !rem_status.changes.is_empty() {
            let mut rem_opts = CommitOptions::new("chore: commit remainder");
            rem_opts.force = true;
            commit_changes(&oz_repo, &rem_opts).expect("oz remainder commit");

            run_git(&git_dir, &["add", "-A"], None);
            run_git(&git_dir, &["commit", "-m", "chore: commit remainder"], None);

            let oz_final = run_git(&oz_dir, &["rev-parse", "HEAD^{tree}"], None);
            let git_final = run_git(&git_dir, &["rev-parse", "HEAD^{tree}"], None);
            prop_assert_eq!(oz_final, git_final, "final remainder tree OID mismatch");
        }
        assert_git_fsck_clean(&oz_dir);
    }

    #[test]
    fn proptest_partial_hunk_pick_and_exclude_preserves_edits_and_matches_git(
        edit_sections in proptest::collection::vec(any::<bool>(), 3..=4),
        pick_flags in proptest::collection::vec(any::<bool>(), 3..=4),
        use_exclude in any::<bool>(),
        extra_unicode_edit in "[a-zA-Z0-9_]{1,12}",
    ) {
        // Ensure at least 1 section is edited and at least 1 hunk is selected.
        let mut edits = edit_sections;
        if !edits.iter().any(|&b| b) {
            edits[0] = true;
        }

        // Build a 4-section file with >=8 unchanged context lines between sections so Histogram
        // hunks are cleanly separable in both `oz` and `git diff --histogram`.
        let mut base_lines = Vec::new();
        for s in 0..edits.len() {
            base_lines.push(format!("// section {s} header"));
            base_lines.push(format!("pub fn sec_{s}() -> i32 {{ {s} }}"));
            for k in 0..9 {
                base_lines.push(format!("// pad_{s}_{k}"));
            }
        }
        let base_content = format!("{}\n", base_lines.join("\n"));

        let mut initial = BTreeMap::new();
        initial.insert(
            "café/multi_hunk.rs".to_string(),
            WorktreeEntry::File {
                bytes: base_content.as_bytes().to_vec(),
                executable: false,
            },
        );
        initial.insert(
            "bin/tool.sh".to_string(),
            WorktreeEntry::File {
                bytes: b"#!/bin/sh\necho base\n".to_vec(),
                executable: true,
            },
        );

        let (_tmp, oz_dir, git_dir) = init_paired_repos(&initial);

        let mut dirty_lines = base_lines;
        for (s, &do_edit) in edits.iter().enumerate() {
            if do_edit {
                dirty_lines[s * 11 + 1] = format!("pub fn sec_{s}() -> i32 {{ {} }}", s + 100);
            }
        }
        let dirty_content = format!("{}\n", dirty_lines.join("\n"));
        let expected_unicode = format!("pub const TAG: &str = \"{extra_unicode_edit}\";\n");

        for dir in [&oz_dir, &git_dir] {
            write_worktree_entry(
                dir,
                "café/multi_hunk.rs",
                &WorktreeEntry::File {
                    bytes: dirty_content.as_bytes().to_vec(),
                    executable: false,
                },
            );
            write_worktree_entry(
                dir,
                "日本語/extra.rs",
                &WorktreeEntry::File {
                    bytes: expected_unicode.as_bytes().to_vec(),
                    executable: false,
                },
            );
        }

        let oz_repo = OzoneRepo::discover(Some(&oz_dir)).expect("discover");
        let status = compute_status(&oz_repo).expect("status");
        let hunk_file = status
            .changes
            .iter()
            .find(|c| c.path == "café/multi_hunk.rs")
            .expect("multi_hunk change");
        prop_assert!(!hunk_file.hunks.is_empty());

        let mut chosen_hunk_indices = BTreeSet::new();
        for (idx, _) in hunk_file.hunks.iter().enumerate() {
            if pick_flags[idx % pick_flags.len()] {
                chosen_hunk_indices.insert(idx);
            }
        }
        if chosen_hunk_indices.is_empty() {
            chosen_hunk_indices.insert(0);
        }

        let mut opts = CommitOptions::new("feat: partial hunk commit");
        opts.force = true;
        if use_exclude && chosen_hunk_indices.len() < hunk_file.hunks.len() {
            for (idx, h) in hunk_file.hunks.iter().enumerate() {
                if !chosen_hunk_indices.contains(&idx) {
                    opts.exclude.push(h.id.clone());
                }
            }
            // Also exclude the extra file if not testing it in this pick.
            if let Some(extra) = status.changes.iter().find(|c| c.path == "日本語/extra.rs") {
                opts.exclude.push(extra.id.clone());
            }
        } else {
            for &idx in &chosen_hunk_indices {
                opts.pick.push(hunk_file.hunks[idx].id.clone());
            }
        }

        commit_changes(&oz_repo, &opts).expect("partial hunk commit");

        // Mirror exact selected hunks with `git diff --histogram` + `git apply --cached`.
        let git_diff = run_git(
            &git_dir,
            &["diff", "--histogram", "--", "café/multi_hunk.rs"],
            None,
        );
        let patch = extract_hunk_patch(&git_diff, &chosen_hunk_indices);
        run_git(&git_dir, &["apply", "--cached", "-"], Some(patch.as_bytes()));
        run_git(&git_dir, &["commit", "-m", "feat: partial hunk commit"], None);

        let oz_tree = run_git(&oz_dir, &["rev-parse", "HEAD^{tree}"], None);
        let git_tree = run_git(&git_dir, &["rev-parse", "HEAD^{tree}"], None);
        prop_assert_eq!(oz_tree, git_tree, "partial hunk committed tree mismatch");
        prop_assert_eq!(
            snapshot_worktree(&oz_dir),
            snapshot_worktree(&git_dir),
            "working tree must remain untouched by partial hunk commit"
        );

        // Commit remainder and verify full target state matches bit-for-bit.
        let mut rem_opts = CommitOptions::new("chore: commit remaining edits");
        rem_opts.force = true;
        commit_changes(&oz_repo, &rem_opts).expect("commit remainder");
        run_git(&git_dir, &["add", "-A"], None);
        run_git(&git_dir, &["commit", "-m", "chore: commit remaining edits"], None);

        prop_assert_eq!(
            run_git(&oz_dir, &["rev-parse", "HEAD^{tree}"], None),
            run_git(&git_dir, &["rev-parse", "HEAD^{tree}"], None)
        );
        assert_git_fsck_clean(&oz_dir);
    }

    #[test]
    fn proptest_amend_sequence_preserves_change_id_and_matches_git(
        initial in arb_initial_tree(),
        rounds in proptest::collection::vec(arb_mutation_batch(), 1..=4),
    ) {
        let (_tmp, oz_dir, git_dir) = init_paired_repos(&initial);
        let expected_oz_parent = run_git(&oz_dir, &["rev-parse", "HEAD"], None);

        // First create a fresh commit to amend against.
        for dir in [&oz_dir, &git_dir] {
            write_worktree_entry(
                dir,
                "café/initial_round.txt",
                &WorktreeEntry::File {
                    bytes: b"round 0\n".to_vec(),
                    executable: false,
                },
            );
        }
        let oz_repo = OzoneRepo::discover(Some(&oz_dir)).expect("discover");
        let first = commit_changes(&oz_repo, &CommitOptions::new("feat: round 0")).expect("c0");
        run_git(&git_dir, &["add", "-A"], None);
        run_git(&git_dir, &["commit", "-m", "feat: round 0"], None);
        let original_change_id = first.change_id.clone();

        for (round_idx, batch) in rounds.iter().enumerate() {
            for m in batch {
                apply_mutation(&oz_dir, m);
                apply_mutation(&git_dir, m);
            }

            let mut amend_opts =
                CommitOptions::new(format!("feat: amended round {}", round_idx + 1));
            amend_opts.amend = true;
            amend_opts.force = true;
            let amended = commit_changes(&oz_repo, &amend_opts).expect("amend commit");
            prop_assert!(amended.amended);
            prop_assert_eq!(
                &amended.change_id,
                &original_change_id,
                "Change-Id must be preserved across --amend"
            );

            run_git(&git_dir, &["add", "-A"], None);
            run_git(
                &git_dir,
                &[
                    "commit",
                    "--amend",
                    "--allow-empty",
                    "-m",
                    &format!("feat: amended round {}", round_idx + 1),
                ],
                None,
            );

            prop_assert_eq!(
                run_git(&oz_dir, &["rev-parse", "HEAD^{tree}"], None),
                run_git(&git_dir, &["rev-parse", "HEAD^{tree}"], None),
                "amended tree OID mismatch"
            );
            prop_assert_eq!(
                run_git(&oz_dir, &["rev-parse", "HEAD^"], None),
                expected_oz_parent.clone(),
                "amended commit must keep the original parent commit OID"
            );
            prop_assert_eq!(
                run_git(&oz_dir, &["rev-parse", "HEAD^"], None),
                run_git(&git_dir, &["rev-parse", "HEAD^"], None),
                "amended parent OID mismatch against git"
            );
            prop_assert_eq!(
                snapshot_worktree(&oz_dir),
                snapshot_worktree(&git_dir),
                "worktree mismatch after amend"
            );
            assert_git_fsck_clean(&oz_dir);
        }
    }
}
