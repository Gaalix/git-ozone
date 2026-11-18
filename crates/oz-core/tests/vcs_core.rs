// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Comprehensive integration tests for indexless status (including FUSE3 O(dirty) fast path),
//! Histogram diff & deterministic IDs, partial hunk/file commits, Change-Id trailers,
//! `.git/index` updates, and pre-commit guardrails.

use chrono::Utc;
use oz_core::{
    BackendKind, ChangeKind, CommitOptions, ExitCode, GuardrailOptions, GuardrailTarget,
    MountRecord, OzError, OzoneLayout, OzoneRepo, SwitchOptions, WorkspaceState, add_pinned_paths,
    check_guardrails, clear_pinned_paths, commit_changes, commit_changes_ignoring_pinned,
    compute_diff, compute_status, read_pinned_paths, remove_pinned_paths, render_unified_diff,
    switch_branch,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Instant;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
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

fn init_repo_with_user(dir: &Path) {
    fs::create_dir_all(dir).expect("mkdir repo");
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.name", "Ozone Test"]);
    git(dir, &["config", "user.email", "test@ozone.dev"]);
}

#[test]
fn fuse_fast_path_o_dirty_ignores_clean_upper_and_handles_whiteouts_and_opaque() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_root = tmp.path().join("mono");
    init_repo_with_user(&repo_root);

    // Populate 1,200 tracked files across nested subdirectories plus `.gitignore` and `old_dir/`.
    fs::write(repo_root.join(".gitignore"), "*.log\nbuild/\n").expect("write gitignore");
    for i in 0..1200 {
        let dir = repo_root.join(format!("pkg_{:02}/src", i / 100));
        fs::create_dir_all(&dir).expect("mkdir pkg");
        fs::write(
            dir.join(format!("mod_{i:04}.rs")),
            format!("pub fn f_{i}() -> usize {{ {i} }}\n"),
        )
        .expect("write mod");
    }
    fs::create_dir_all(repo_root.join("old_dir")).expect("mkdir old_dir");
    fs::write(repo_root.join("old_dir/keep.txt"), "keep v1\n").expect("write keep");
    fs::write(repo_root.join("old_dir/gone.txt"), "gone v1\n").expect("write gone");
    fs::write(repo_root.join("tracked.log"), "tracked log v1\n").expect("write tracked.log");

    git(&repo_root, &["add", "."]);
    git(&repo_root, &["add", "-f", "tracked.log"]);
    git(&repo_root, &["commit", "-m", "seed 1200 tracked files"]);
    git(&repo_root, &["branch", "feat/fuse-ws"]);
    let head_oid = git(&repo_root, &["rev-parse", "HEAD"]).trim().to_string();

    let layout = OzoneLayout::from_git_common_dir(repo_root.join(".git"));
    layout.ensure_initialized().expect("init ozone");

    let ws_name = "ws-fuse";
    let upper_dir = layout.ws_upper_dir(ws_name);
    let meta_dir = layout.ws_tombstone_dir(ws_name);
    let mount_point = tmp.path().join("mono.oz").join(ws_name);
    fs::create_dir_all(&upper_dir).expect("mkdir upper");
    fs::create_dir_all(meta_dir.join("whiteouts")).expect("mkdir whiteouts");
    fs::create_dir_all(meta_dir.join("opaque")).expect("mkdir opaque");
    // NOTE: intentionally leave `mount_point` empty/unpopulated — FUSE fast path must NOT
    // depend on walking untouched baseline/mount files!
    fs::create_dir_all(&mount_point).expect("mkdir mount_point");

    let record = MountRecord {
        schema_version: MountRecord::CURRENT_SCHEMA_VERSION,
        workspace: ws_name.to_string(),
        branch: "feat/fuse-ws".to_string(),
        backend: BackendKind::LinuxFuse3,
        repo_root: repo_root.clone(),
        git_dir: repo_root.join(".git"),
        mount_point: mount_point.clone(),
        upper_dir: Some(upper_dir.clone()),
        meta_dir: Some(meta_dir.clone()),
        base_tree: None,
        base_commit: Some(head_oid),
        socket_path: Some(layout.vfsd_socket_path()),
        daemon_pid: None,
        mounted: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    fs::create_dir_all(layout.ws_meta_dir(ws_name)).expect("mkdir ws meta");
    fs::write(
        layout.ws_mount_record_path(ws_name),
        serde_json::to_string_pretty(&record).expect("json"),
    )
    .expect("write mount.json");

    // 1. Place 200 committed-identical files in `upper_dir` -> MUST be treated as CLEAN!
    for i in 0..200 {
        let dir = upper_dir.join(format!("pkg_{:02}/src", i / 100));
        fs::create_dir_all(&dir).expect("mkdir upper pkg");
        fs::write(
            dir.join(format!("mod_{i:04}.rs")),
            format!("pub fn f_{i}() -> usize {{ {i} }}\n"),
        )
        .expect("write clean upper file");
    }

    // 2. Place a whiteout for a path NOT in HEAD (already committed deletion) -> MUST be ignored!
    fs::write(meta_dir.join("whiteouts/not_in_head.txt.oz_wh"), b"").expect("stale whiteout");

    // 3. Place gitignored files in `upper_dir` -> MUST be ignored!
    fs::write(upper_dir.join("ignored.log"), "noise\n").expect("ignored log");
    fs::create_dir_all(upper_dir.join("build/nested")).expect("mkdir ignored build");
    fs::write(upper_dir.join("build/nested/out.bin"), b"\x00\x01\x02").expect("ignored bin");

    // 4. Actual dirty edits:
    //    - Modified tracked file `pkg_00/src/mod_0005.rs`
    //    - Modified tracked-but-gitignored file `tracked.log`
    //    - Added file `crates/new_pkce.rs`
    //    - Whiteout deletion `pkg_03/src/mod_0300.rs` (not present in `upper_dir`)
    //    - Opaque dir `old_dir` where `old_dir/keep.txt` is recreated identical (CLEAN) and
    //      `old_dir/gone.txt` is absent from `upper_dir` -> `Deleted`!
    fs::write(
        upper_dir.join("pkg_00/src/mod_0005.rs"),
        "pub fn f_5() -> usize { 999 }\n",
    )
    .expect("modify upper file");
    fs::write(upper_dir.join("tracked.log"), "tracked log v2\n").expect("modify tracked.log");
    fs::create_dir_all(upper_dir.join("crates")).expect("mkdir crates");
    fs::write(
        upper_dir.join("crates/new_pkce.rs"),
        "pub fn verify() {}\n",
    )
    .expect("add file");
    fs::create_dir_all(meta_dir.join("whiteouts/pkg_03/src")).expect("mkdir wh parent");
    fs::write(
        meta_dir.join("whiteouts/pkg_03/src/mod_0300.rs.oz_wh"),
        b"",
    )
    .expect("write whiteout");
    fs::write(meta_dir.join("opaque/old_dir.oz_opaque"), b"").expect("write opaque");
    fs::create_dir_all(upper_dir.join("old_dir")).expect("mkdir upper old_dir");
    fs::write(upper_dir.join("old_dir/keep.txt"), "keep v1\n").expect("recreate keep.txt");

    let repo = OzoneRepo::discover(Some(&mount_point)).expect("discover fuse workspace");
    let start = Instant::now();
    let status = compute_status(&repo).expect("compute status");
    let elapsed = start.elapsed();

    assert!(status.used_fuse_fast_path);
    assert_eq!(status.branch.as_deref(), Some("feat/fuse-ws"));
    assert!(
        elapsed.as_millis() < 250,
        "expected fast O(dirty) FUSE status, took {:?}",
        elapsed
    );

    let summary: Vec<(&str, &str, ChangeKind)> = status
        .changes
        .iter()
        .map(|c| (c.id.as_str(), c.path.as_str(), c.kind))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("f1", "crates/new_pkce.rs", ChangeKind::Added),
            ("f2", "old_dir/gone.txt", ChangeKind::Deleted),
            ("f3", "pkg_00/src/mod_0005.rs", ChangeKind::Modified),
            ("f4", "pkg_03/src/mod_0300.rs", ChangeKind::Deleted),
            ("f5", "tracked.log", ChangeKind::Modified),
        ]
    );

    // Commit everything on feat/fuse-ws; committed files stay in upper_dir and whiteouts stay in meta_dir.
    let outcome = commit_changes(&repo, &CommitOptions::new("feat: commit fuse edits"))
        .expect("commit fuse edits");
    assert_eq!(outcome.remaining_dirty_count, 0);

    // Re-running status immediately (even with committed files still in upper_dir & whiteouts still in meta_dir)
    // MUST report 0 dirty files!
    let after_status = compute_status(&repo).expect("status after commit");
    assert!(
        after_status.is_clean(),
        "expected clean status after commit, got {:?}",
        after_status.changes
    );
}

#[test]
fn deterministic_hunk_ids_beyond_26_and_binary_detection() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_root = tmp.path().join("hunks");
    init_repo_with_user(&repo_root);

    // Build a 300-line file and modify 28 separated regions so hunks span h1a..h1z, h1aa, h1ab.
    let orig: String = (0..300).map(|i| format!("row_{i:03}\n")).collect();
    fs::write(repo_root.join("big.txt"), &orig).expect("write big.txt");
    git(&repo_root, &["add", "big.txt"]);
    git(&repo_root, &["commit", "-m", "init big"]);

    let mut edited: Vec<String> = (0..300).map(|i| format!("row_{i:03}\n")).collect();
    for k in 0..28 {
        let line_idx = k * 10 + 1;
        edited[line_idx] = format!("row_{line_idx:03}_changed_{k}\n");
    }
    fs::write(repo_root.join("big.txt"), edited.concat()).expect("write edited big.txt");

    // Binary file with NUL inside first 8 KiB, plus text file with NUL after 8 KiB.
    let mut bin_bytes = vec![b'x'; 128];
    bin_bytes[42] = 0;
    fs::write(repo_root.join("blob.bin"), &bin_bytes).expect("write binary");

    let mut late_nul = vec![b'a'; 9000];
    late_nul[8500] = 0;
    fs::write(repo_root.join("z_late.txt"), &late_nul).expect("write late nul");

    let repo = OzoneRepo::discover(Some(&repo_root)).expect("discover");
    let diff_all = compute_diff(&repo, &[]).expect("compute diff");
    assert_eq!(diff_all.files.len(), 3);
    assert_eq!(diff_all.files[0].id, "f1");
    assert_eq!(diff_all.files[0].path, "big.txt");
    assert_eq!(diff_all.files[0].hunks.len(), 28);
    assert_eq!(diff_all.files[0].hunks[0].id, "h1a");
    assert_eq!(diff_all.files[0].hunks[25].id, "h1z");
    assert_eq!(diff_all.files[0].hunks[26].id, "h1aa");
    assert_eq!(diff_all.files[0].hunks[27].id, "h1ab");

    assert_eq!(diff_all.files[1].id, "f2");
    assert_eq!(diff_all.files[1].path, "blob.bin");
    assert!(diff_all.files[1].is_binary);
    assert!(diff_all.files[1].hunks.is_empty());

    assert_eq!(diff_all.files[2].id, "f3");
    assert!(!diff_all.files[2].is_binary);

    // Filter by hunk ID `h1aa`
    let diff_h1aa = compute_diff(&repo, &["h1aa".to_string()]).expect("filter h1aa");
    assert_eq!(diff_h1aa.files.len(), 1);
    assert_eq!(diff_h1aa.files[0].hunks.len(), 1);
    assert_eq!(diff_h1aa.files[0].hunks[0].id, "h1aa");
    let rendered = render_unified_diff(&diff_h1aa);
    assert!(rendered.contains("[h1aa]"));
    assert!(rendered.contains("changed_26"));
    assert!(!rendered.contains("changed_0"));
}

#[test]
fn partial_commit_pick_f2_h1a_and_exclude_leaves_unselected_hunks_and_files_untouched() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_root = tmp.path().join("partial");
    init_repo_with_user(&repo_root);

    let initial_a: String = (1..=30).map(|i| format!("token_line_{i}\n")).collect();
    fs::write(repo_root.join("a_token.rs"), &initial_a).expect("write a_token");
    fs::write(repo_root.join("c_debug.rs"), "pub fn debug() {}\n").expect("write c_debug");
    git(&repo_root, &["add", "."]);
    git(&repo_root, &["commit", "-m", "initial"]);

    // Modify a_token.rs (f1) with two separated hunks (h1a near top, h1b near bottom),
    // add b_pkce.rs (f2), and modify c_debug.rs (f3).
    let mut edited_a: Vec<String> = (1..=30).map(|i| format!("token_line_{i}\n")).collect();
    edited_a[1] = "token_line_2_PKCE_HUNK_A\n".to_string();
    edited_a[26] = "token_line_27_TEMP_HUNK_B\n".to_string();
    let edited_a_str = edited_a.concat();

    fs::write(repo_root.join("a_token.rs"), &edited_a_str).expect("edit a_token");
    fs::write(repo_root.join("b_pkce.rs"), "pub fn pkce() -> bool { true }\n").expect("add b_pkce");
    fs::write(
        repo_root.join("c_debug.rs"),
        "pub fn debug() { println!(\"dbg\"); }\n",
    )
    .expect("edit c_debug");

    let repo = OzoneRepo::discover(Some(&repo_root)).expect("discover");
    let status = compute_status(&repo).expect("status");
    assert_eq!(status.changes.len(), 3);
    assert_eq!(status.changes[0].id, "f1");
    assert_eq!(status.changes[0].path, "a_token.rs");
    assert_eq!(status.changes[0].hunks.len(), 2);
    assert_eq!(status.changes[0].hunks[0].id, "h1a");
    assert_eq!(status.changes[0].hunks[1].id, "h1b");
    assert_eq!(status.changes[1].id, "f2");
    assert_eq!(status.changes[1].path, "b_pkce.rs");
    assert_eq!(status.changes[2].id, "f3");
    assert_eq!(status.changes[2].path, "c_debug.rs");

    // Pick ONLY f2 and h1a!
    let outcome = commit_changes(
        &repo,
        &CommitOptions::new("feat(auth): add PKCE verifier").with_pick(["f2", "h1a"]),
    )
    .expect("partial pick commit");

    assert_eq!(outcome.committed_paths, vec!["a_token.rs", "b_pkce.rs"]);
    assert_eq!(outcome.partial_hunk_paths, vec!["a_token.rs"]);
    assert_eq!(outcome.remaining_dirty_count, 2);
    assert!(outcome.change_id.starts_with('I'));
    assert_eq!(outcome.change_id.len(), 41);

    // 1. Working directory files MUST still be 100% untouched on disk!
    assert_eq!(
        fs::read_to_string(repo_root.join("a_token.rs")).expect("read disk a_token"),
        edited_a_str
    );
    assert_eq!(
        fs::read_to_string(repo_root.join("c_debug.rs")).expect("read disk c_debug"),
        "pub fn debug() { println!(\"dbg\"); }\n"
    );

    // 2. HEAD commit tree MUST have ONLY h1a in a_token.rs, b_pkce.rs, and original c_debug.rs!
    let head_a = git(&repo_root, &["show", "HEAD:a_token.rs"]);
    assert!(head_a.contains("token_line_2_PKCE_HUNK_A"));
    assert!(!head_a.contains("token_line_27_TEMP_HUNK_B"));
    assert!(head_a.contains("token_line_27\n"));

    let head_b = git(&repo_root, &["show", "HEAD:b_pkce.rs"]);
    assert_eq!(head_b, "pub fn pkce() -> bool { true }\n");

    let head_c = git(&repo_root, &["show", "HEAD:c_debug.rs"]);
    assert_eq!(head_c, "pub fn debug() {}\n");

    // 3. Remaining status has only h1b in a_token.rs and c_debug.rs; C git agrees!
    let remaining = compute_status(&repo).expect("remaining status");
    assert_eq!(remaining.changes.len(), 2);
    assert_eq!(remaining.changes[0].path, "a_token.rs");
    assert_eq!(remaining.changes[0].hunks.len(), 1);
    assert_eq!(remaining.changes[1].path, "c_debug.rs");

    let git_porcelain = git(&repo_root, &["status", "--porcelain"]);
    assert!(git_porcelain.contains(" M a_token.rs"));
    assert!(git_porcelain.contains(" M c_debug.rs"));
    assert!(!git_porcelain.contains("b_pkce.rs"));

    // 4. Test `--exclude` on remaining changes: exclude c_debug.rs (`f2` now), leaving h1b committed!
    let outcome2 = commit_changes(
        &repo,
        &CommitOptions::new("chore: commit remaining token hunk").with_exclude(["f2"]),
    )
    .expect("exclude commit");
    assert_eq!(outcome2.committed_paths, vec!["a_token.rs"]);
    assert_eq!(outcome2.remaining_dirty_count, 1);

    let after2 = compute_status(&repo).expect("after2");
    assert_eq!(after2.changes.len(), 1);
    assert_eq!(after2.changes[0].path, "c_debug.rs");
}

#[test]
fn unborn_head_commit_pinned_paths_and_change_id_trailer() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_root = tmp.path().join("unborn");
    fs::create_dir_all(&repo_root).expect("mkdir");
    // Unborn repository with NO user.name/user.email set yet.
    git(&repo_root, &["init", "-b", "main"]);
    fs::write(repo_root.join("README.md"), "# unborn\n").expect("write readme");

    let repo = OzoneRepo::discover(Some(&repo_root)).expect("discover unborn");
    assert_eq!(repo.head_oid, None);
    let st = compute_status(&repo).expect("unborn status");
    assert_eq!(st.changes.len(), 1);
    assert_eq!(st.changes[0].path, "README.md");

    let out = commit_changes(&repo, &CommitOptions::new("chore: initial unborn commit"))
        .expect("commit on unborn HEAD");
    assert!(out.parent_oid.is_none());
    assert_eq!(out.branch.as_deref(), Some("main"));
    assert!(out.message.contains(&format!("Change-Id: {}", out.change_id)));

    // C git should see a clean working tree on main with the Change-Id trailer.
    assert!(git(&repo_root, &["status", "--porcelain"]).trim().is_empty());
    let log_msg = git(&repo_root, &["log", "-1", "--pretty=%B"]);
    assert!(log_msg.contains(&format!("Change-Id: {}", out.change_id)));

    // Test pinned_paths fallback in sibling workspace state.
    let ws_path = tmp.path().join("unborn.oz/agent-pin");
    fs::create_dir_all(&ws_path).expect("mkdir ws");
    fs::write(ws_path.join("README.md"), "# unborn\n").expect("copy readme");
    fs::write(ws_path.join("pinned.rs"), "pub fn p() {}\n").expect("write pinned");
    fs::write(ws_path.join("unpinned.rs"), "pub fn u() {}\n").expect("write unpinned");

    let layout = OzoneLayout::from_git_common_dir(repo_root.join(".git"));
    layout.ensure_initialized().expect("init ozone");
    git(&repo_root, &["branch", "feat/pin"]);

    let mut pinned = BTreeSet::new();
    pinned.insert("pinned.rs".to_string());
    let state = WorkspaceState {
        schema_version: WorkspaceState::CURRENT_SCHEMA_VERSION,
        name: "agent-pin".to_string(),
        branch: "feat/pin".to_string(),
        base_branch: Some("main".to_string()),
        base_commit: Some(out.commit_oid.clone()),
        base_tree: out.tree_oid.clone(),
        backend: BackendKind::CasHardlink,
        worktree_path: ws_path.clone(),
        git_worktree_dir: None,
        baseline_id: None,
        dirty_paths: BTreeMap::new(),
        pinned_paths: pinned,
        scope: Vec::new(),
        deny: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        last_checkpoint_oid: None,
    };
    fs::write(
        layout.workspace_state_path("agent-pin"),
        serde_json::to_string_pretty(&state).expect("json"),
    )
    .expect("write state");

    let ws_repo = OzoneRepo::discover(Some(&ws_path)).expect("discover ws");
    let pin_out = commit_changes(&ws_repo, &CommitOptions::new("feat: commit pinned only"))
        .expect("commit pinned");
    assert_eq!(pin_out.committed_paths, vec!["pinned.rs"]);
    assert_eq!(pin_out.remaining_dirty_count, 1);

    // pinned_paths MUST be cleared on commit!
    let updated_state: WorkspaceState = serde_json::from_str(
        &fs::read_to_string(layout.workspace_state_path("agent-pin")).expect("read state"),
    )
    .expect("parse state");
    assert!(updated_state.pinned_paths.is_empty());

    let ws_after = compute_status(&ws_repo).expect("ws status after");
    assert_eq!(ws_after.changes.len(), 1);
    assert_eq!(ws_after.changes[0].path, "unpinned.rs");
}

#[test]
fn guardrails_block_secrets_conflicts_oversized_blobs_and_scope_deny() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_root = tmp.path().join("guarded");
    init_repo_with_user(&repo_root);
    fs::create_dir_all(repo_root.join("crates/auth")).expect("mkdir auth");
    fs::write(repo_root.join("crates/auth/lib.rs"), "pub fn init() {}\n").expect("write lib");
    fs::write(repo_root.join("Cargo.lock"), "# lock\n").expect("write lock");
    git(&repo_root, &["add", "."]);
    git(&repo_root, &["commit", "-m", "init"]);

    // 1. Secret `.env` vs allowed `.env.example`
    fs::write(repo_root.join(".env.example"), "PORT=8080\n").expect("write env example");
    let repo = OzoneRepo::discover(Some(&repo_root)).expect("discover");
    commit_changes(&repo, &CommitOptions::new("chore: add env template"))
        .expect(".env.example must be allowed");

    fs::write(repo_root.join(".env"), "DB_PASS=supersecret\n").expect("write .env");
    let err = commit_changes(&repo, &CommitOptions::new("oops dotenv"))
        .expect_err(".env must be blocked");
    assert_eq!(err.exit_code(), ExitCode::UsageOrGuardrail);
    assert!(matches!(err, OzError::GuardrailViolation { .. }));
    assert!(err.recovery_commands().iter().any(|r| r.contains("--force")));
    fs::remove_file(repo_root.join(".env")).expect("rm .env");

    // 2. Secret token in code
    let aws_line = format!("const KEY: &str = \"{}{}\";\n", "AKIA", "IOSFODNN7EXAMPLE");
    fs::write(repo_root.join("crates/auth/lib.rs"), aws_line).expect("write secret");
    let err = commit_changes(&repo, &CommitOptions::new("oops secret"))
        .expect_err("secret must be blocked");
    assert!(matches!(err, OzError::GuardrailViolation { .. }));
    assert_eq!(err.exit_code(), ExitCode::UsageOrGuardrail);

    // Deleting a secret / .env file or resetting it clean works; `--force` overrides.
    let mut force_opts = CommitOptions::new("force secret test");
    force_opts.force = true;
    commit_changes(&repo, &force_opts).expect("--force overrides secret");

    // 3. Conflict markers
    fs::write(
        repo_root.join("crates/auth/lib.rs"),
        "<<<<<<< HEAD\na\n=======\nb\n>>>>>>> feat/other\n",
    )
    .expect("write conflict");
    let err = commit_changes(&repo, &CommitOptions::new("oops conflict"))
        .expect_err("conflict marker must be blocked");
    assert!(matches!(err, OzError::GuardrailViolation { ref kind, .. } if kind == "conflict_marker"));
    fs::write(repo_root.join("crates/auth/lib.rs"), "pub fn clean() {}\n").expect("fix conflict");
    commit_changes(&repo, &CommitOptions::new("clean fix")).expect("clean commit");

    // 4. Oversized blob (> 50 MiB default)
    {
        let big_path = repo_root.join("huge.bin");
        let mut file = fs::File::create(&big_path).expect("create huge");
        // Write > 50 MiB (sparse/zeros, fast on Linux ext4/tmpfs).
        file.set_len(50 * 1024 * 1024 + 1024).expect("set_len");
        file.write_all(b"\x00").expect("write byte");
    }
    let err = commit_changes(&repo, &CommitOptions::new("oops >50MB"))
        .expect_err(">50MB blob must be blocked");
    assert!(matches!(err, OzError::GuardrailViolation { ref kind, .. } if kind == "oversized_blob"));
    assert_eq!(err.exit_code(), ExitCode::UsageOrGuardrail);
    fs::remove_file(repo_root.join("huge.bin")).expect("rm huge.bin");

    // 5. Scope / deny violations via WorkspaceState + .ozone.toml
    let ws_path = tmp.path().join("guarded.oz/ws-scoped");
    fs::create_dir_all(ws_path.join("crates/auth")).expect("mkdir ws auth");
    fs::create_dir_all(ws_path.join("crates/billing")).expect("mkdir ws billing");
    fs::write(ws_path.join(".env.example"), "PORT=8080\n").expect("copy env example");
    fs::write(ws_path.join("crates/auth/lib.rs"), "pub fn clean() {}\n").expect("copy auth");
    fs::write(ws_path.join("Cargo.lock"), "# modified lock\n").expect("modify lock");
    fs::write(ws_path.join("crates/billing/invoice.rs"), "pub fn inv() {}\n")
        .expect("out of scope file");

    let layout = OzoneLayout::from_git_common_dir(repo_root.join(".git"));
    layout.ensure_initialized().expect("init ozone");
    git(&repo_root, &["branch", "feat/scoped"]);
    let head_oid = git(&repo_root, &["rev-parse", "HEAD"]).trim().to_string();
    let head_tree = git(&repo_root, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .to_string();

    let state = WorkspaceState {
        schema_version: WorkspaceState::CURRENT_SCHEMA_VERSION,
        name: "ws-scoped".to_string(),
        branch: "feat/scoped".to_string(),
        base_branch: Some("main".to_string()),
        base_commit: Some(head_oid),
        base_tree: head_tree,
        backend: BackendKind::CasHardlink,
        worktree_path: ws_path.clone(),
        git_worktree_dir: None,
        baseline_id: None,
        dirty_paths: BTreeMap::new(),
        pinned_paths: BTreeSet::new(),
        scope: vec!["crates/auth/**".to_string()],
        deny: vec!["Cargo.lock".to_string()],
        created_at: Utc::now(),
        updated_at: Utc::now(),
        last_checkpoint_oid: None,
    };
    fs::write(
        layout.workspace_state_path("ws-scoped"),
        serde_json::to_string_pretty(&state).expect("json"),
    )
    .expect("write state");

    let ws_repo = OzoneRepo::discover(Some(&ws_path)).expect("discover ws");
    let ws_status = compute_status(&ws_repo).expect("ws status");
    assert_eq!(ws_status.changes.len(), 2);
    assert!(ws_status.changes.iter().all(|c| c.out_of_scope));

    let scope_err = commit_changes(&ws_repo, &CommitOptions::new("blocked out of scope"))
        .expect_err("scope/deny must block");
    assert!(matches!(scope_err, OzError::GuardrailViolation { ref kind, .. } if kind == "out_of_scope"));
    assert!(
        scope_err
            .recovery_commands()
            .iter()
            .any(|r| r.contains("--allow-out-of-scope"))
    );

    let mut allow_opts = CommitOptions::new("allow out of scope");
    allow_opts.allow_out_of_scope = true;
    commit_changes(&ws_repo, &allow_opts).expect("--allow-out-of-scope succeeds");
}

#[cfg(unix)]
#[test]
fn tracks_executable_mode_and_symlinks_accurately() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let tmp = TempDir::new().expect("tempdir");
    let repo_root = tmp.path().join("modes");
    init_repo_with_user(&repo_root);
    fs::write(repo_root.join("script.sh"), "#!/bin/sh\necho hi\n").expect("write script");
    git(&repo_root, &["add", "script.sh"]);
    git(&repo_root, &["commit", "-m", "init script"]);

    // chmod +x
    let mut perms = fs::metadata(repo_root.join("script.sh"))
        .expect("meta")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(repo_root.join("script.sh"), perms).expect("chmod");

    // Add symlink
    symlink("script.sh", repo_root.join("link.sh")).expect("symlink");

    let repo = OzoneRepo::discover(Some(&repo_root)).expect("discover");
    let status = compute_status(&repo).expect("status");
    assert_eq!(status.changes.len(), 2);
    assert_eq!(status.changes[0].path, "link.sh");
    assert_eq!(status.changes[0].kind, ChangeKind::Added);
    assert_eq!(status.changes[1].path, "script.sh");
    assert_eq!(status.changes[1].kind, ChangeKind::Modified);

    commit_changes(&repo, &CommitOptions::new("chmod and symlink")).expect("commit modes");
    assert!(git(&repo_root, &["status", "--porcelain"]).trim().is_empty());

    let ls_tree = git(&repo_root, &["ls-tree", "HEAD"]);
    assert!(ls_tree.contains("100755 blob"));
    assert!(ls_tree.contains("120000 blob"));
}

#[test]
fn linked_worktree_fuse_partial_commit_and_dir_whiteout_and_all_guardrails() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_root = tmp.path().join("linked-fuse");
    init_repo_with_user(&repo_root);

    let lines: String = (1..=30).map(|i| format!("line_{i}\n")).collect();
    fs::write(repo_root.join("app.rs"), &lines).expect("write app");
    fs::create_dir_all(repo_root.join("gone_dir")).expect("mkdir gone_dir");
    fs::write(repo_root.join("gone_dir/x.txt"), "x\n").expect("write x");
    fs::write(repo_root.join("gone_dir/y.txt"), "y\n").expect("write y");
    git(&repo_root, &["add", "."]);
    git(&repo_root, &["commit", "-m", "base"]);

    let layout = OzoneLayout::from_git_common_dir(repo_root.join(".git"));
    layout.ensure_initialized().expect("init ozone");

    let ws_name = "ws-linked";
    let mount_point = tmp.path().join("linked-fuse.oz").join(ws_name);
    fs::create_dir_all(&mount_point).expect("mkdir mount_point");
    git(&repo_root, &["branch", "feat/linked"]);
    let head_oid = git(&repo_root, &["rev-parse", "HEAD"]).trim().to_string();

    // Create colocated `.git/worktrees/<ws>` metadata cleanly without populating mount_point.
    let wt_git_dir = repo_root.join(".git/worktrees").join(ws_name);
    fs::create_dir_all(&wt_git_dir).expect("mkdir wt git dir");
    fs::write(wt_git_dir.join("HEAD"), "ref: refs/heads/feat/linked\n").expect("wt HEAD");
    fs::write(wt_git_dir.join("commondir"), "../..\n").expect("wt commondir");
    fs::write(
        wt_git_dir.join("gitdir"),
        format!("{}/.git\n", mount_point.display()),
    )
    .expect("wt gitdir");

    let upper_dir = layout.ws_upper_dir(ws_name);
    let meta_dir = layout.ws_tombstone_dir(ws_name);
    fs::create_dir_all(&upper_dir).expect("mkdir upper");
    fs::create_dir_all(meta_dir.join("whiteouts")).expect("mkdir wh");
    fs::create_dir_all(meta_dir.join("opaque")).expect("mkdir op");

    let record = MountRecord {
        schema_version: MountRecord::CURRENT_SCHEMA_VERSION,
        workspace: ws_name.to_string(),
        branch: "feat/linked".to_string(),
        backend: BackendKind::LinuxFuse3,
        repo_root: repo_root.clone(),
        git_dir: repo_root.join(".git"),
        mount_point: mount_point.clone(),
        upper_dir: Some(upper_dir.clone()),
        meta_dir: Some(meta_dir.clone()),
        base_tree: None,
        base_commit: Some(head_oid),
        socket_path: Some(layout.vfsd_socket_path()),
        daemon_pid: None,
        mounted: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    fs::create_dir_all(layout.ws_meta_dir(ws_name)).expect("mkdir ws meta");
    fs::write(
        layout.ws_mount_record_path(ws_name),
        serde_json::to_string_pretty(&record).expect("json"),
    )
    .expect("write mount.json");

    // Directory-level whiteout `gone_dir.oz_wh` marks both `gone_dir/x.txt` & `gone_dir/y.txt` Deleted.
    fs::write(meta_dir.join("whiteouts/gone_dir.oz_wh"), b"").expect("dir whiteout");

    // `app.rs` has clean hunk h1a and conflict-marker hunk h1b!
    let mut edited: Vec<String> = (1..=30).map(|i| format!("line_{i}\n")).collect();
    edited[1] = "line_2_clean_hunk_a\n".to_string();
    edited[26] = "<<<<<<< HEAD\nconflict\n=======\ntheirs\n>>>>>>> other\n".to_string();
    let edited_str = edited.concat();
    fs::write(upper_dir.join("app.rs"), &edited_str).expect("write upper app.rs");

    let repo = OzoneRepo::discover(Some(&mount_point)).expect("discover");
    assert!(repo.is_linked_worktree);

    // Filter compute_diff by file ID (`f1`) and path (`app.rs`).
    let diff_by_fid = compute_diff(&repo, &["f1".to_string()]).expect("diff f1");
    assert_eq!(diff_by_fid.files.len(), 1);
    assert_eq!(diff_by_fid.files[0].path, "app.rs");
    let diff_by_path = compute_diff(&repo, &["gone_dir".to_string()]).expect("diff gone_dir");
    assert_eq!(diff_by_path.files.len(), 2);

    // Full commit must fail on conflict marker in h1b, whereas `--pick h1a` succeeds!
    let full_err = commit_changes(&repo, &CommitOptions::new("blocked conflict"))
        .expect_err("full commit blocked by conflict marker");
    assert!(matches!(
        full_err,
        OzError::GuardrailViolation { ref kind, .. } if kind == "conflict_marker"
    ));

    let pick_out = commit_changes(&repo, &CommitOptions::new("clean hunk").with_pick(["h1a"]))
        .expect("--pick h1a succeeds without triggering h1b conflict marker");
    assert_eq!(pick_out.committed_paths, vec!["app.rs"]);
    assert!(wt_git_dir.join("index").is_file());
    assert_eq!(
        fs::read_to_string(upper_dir.join("app.rs")).expect("read upper app.rs"),
        edited_str
    );

    let head_app = git(&repo_root, &["show", "feat/linked:app.rs"]);
    assert!(head_app.contains("line_2_clean_hunk_a"));
    assert!(!head_app.contains("<<<<<<< HEAD"));

    // Verify all remaining secret and env template guardrail patterns directly.
    for allowed in [".env.sample", ".env.template", ".env.dist", ".env.example"] {
        check_guardrails(
            &repo,
            &[GuardrailTarget::new(allowed, ChangeKind::Added, b"KEY=1\n")],
            &GuardrailOptions::default(),
        )
        .expect("template .env allowed");
    }
    check_guardrails(
        &repo,
        &[GuardrailTarget::new(".env.local", ChangeKind::Added, b"K=1\n")],
        &GuardrailOptions::default(),
    )
    .expect_err(".env.local blocked");

    let ghp = format!("{}0123456789abcdefghijklmnopqrstuvwxyz\n", "ghp_");
    let fg_pat = format!("{}11abcDEF_token\n", "github_pat_");
    let priv_key = format!("{}RSA PRIVATE KEY-----\n", "-----BEGIN ");
    for secret in [ghp, fg_pat, priv_key] {
        let err = check_guardrails(
            &repo,
            &[GuardrailTarget::new("cfg.txt", ChangeKind::Added, secret.as_bytes())],
            &GuardrailOptions::default(),
        )
        .expect_err("secret pattern blocked");
        assert!(matches!(err, OzError::GuardrailViolation { ref kind, .. } if kind == "secret"));
    }

    // Deleting a `.env` or oversized file must NOT be blocked by secret/size guardrails.
    check_guardrails(
        &repo,
        &[GuardrailTarget::new(".env", ChangeKind::Deleted, b"")],
        &GuardrailOptions::default(),
    )
    .expect("deleting .env is allowed");
}

#[test]
fn main_repo_pinned_main_json_and_windows_projfs_fast_path_and_switch() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_root = tmp.path().join("pinned-main");
    init_repo_with_user(&repo_root);
    fs::write(repo_root.join("a.rs"), "pub fn a() -> i32 { 1 }\n").expect("write a");
    fs::write(repo_root.join("b.rs"), "pub fn b() -> i32 { 1 }\n").expect("write b");
    git(&repo_root, &["add", "."]);
    git(&repo_root, &["commit", "-m", "init"]);

    let repo = OzoneRepo::discover(Some(&repo_root)).expect("discover");
    assert_eq!(repo.workspace, None);

    fs::write(repo_root.join("a.rs"), "pub fn a() -> i32 { 2 }\n").expect("edit a");
    fs::write(repo_root.join("b.rs"), "pub fn b() -> i32 { 2 }\n").expect("edit b");
    fs::write(repo_root.join("c.rs"), "pub fn c() -> i32 { 1 }\n").expect("add c");

    // Pin a.rs and c.rs in main repo -> stored at `.git/ozone/pinned_main.json`, NOT `.git/ozone/state/`!
    let pinned = add_pinned_paths(&repo, ["a.rs", "./c.rs"]).expect("add pinned");
    assert_eq!(pinned.len(), 2);
    assert!(repo.layout.main_pinned_path().is_file());
    let state_entries: Vec<_> = fs::read_dir(repo.layout.state_dir())
        .expect("read state dir")
        .collect();
    assert!(
        state_entries.is_empty(),
        "main-repo pinned paths MUST NOT be stored inside .git/ozone/state/"
    );

    let status = compute_status(&repo).expect("status");
    assert_eq!(status.changes.len(), 3);
    assert!(status.changes[0].pinned); // a.rs
    assert!(!status.changes[1].pinned); // b.rs
    assert!(status.changes[2].pinned); // c.rs

    // Unpin c.rs, then commit -> commits ONLY a.rs!
    let rem = remove_pinned_paths(&repo, ["c.rs"]).expect("remove c.rs");
    assert_eq!(rem.len(), 1);
    let out1 = commit_changes(&repo, &CommitOptions::new("feat: commit pinned a.rs")).expect("commit a");
    assert_eq!(out1.committed_paths, vec!["a.rs"]);
    assert_eq!(out1.remaining_dirty_count, 2);
    assert!(read_pinned_paths(&repo).expect("read pinned").is_empty());

    // Pin b.rs, then commit ignoring pinned (`-a`) -> commits BOTH b.rs and c.rs and clears pinned!
    add_pinned_paths(&repo, ["b.rs"]).expect("pin b");
    let out2 = commit_changes_ignoring_pinned(&repo, &CommitOptions::new("feat: commit all"))
        .expect("commit all ignoring pinned");
    assert_eq!(out2.committed_paths, vec!["b.rs", "c.rs"]);
    assert_eq!(out2.remaining_dirty_count, 0);
    assert!(read_pinned_paths(&repo).expect("read pinned").is_empty());
    clear_pinned_paths(&repo).expect("clear no-op");

    // Verify WindowsProjFs triggers O(dirty_files) upper/ + meta/ fast path via backend.requires_daemon().
    let ws_name = "win-projfs";
    let upper_dir = repo.layout.ws_upper_dir(ws_name);
    let meta_dir = repo.layout.ws_tombstone_dir(ws_name);
    let ws_mount = tmp.path().join("pinned-main.oz").join(ws_name);
    fs::create_dir_all(&upper_dir).expect("mkdir upper");
    fs::create_dir_all(meta_dir.join("whiteouts")).expect("mkdir wh");
    fs::create_dir_all(meta_dir.join("opaque")).expect("mkdir op");
    fs::create_dir_all(&ws_mount).expect("mkdir mount");
    fs::write(upper_dir.join("a.rs"), "pub fn a() -> i32 { 99 }\n").expect("write upper a");
    fs::write(meta_dir.join("whiteouts/b.rs.oz_wh"), b"").expect("write whiteout b");
    git(&repo_root, &["branch", "feat/projfs"]);

    let state = WorkspaceState {
        schema_version: WorkspaceState::CURRENT_SCHEMA_VERSION,
        name: ws_name.to_string(),
        branch: "feat/projfs".to_string(),
        base_branch: Some("main".to_string()),
        base_commit: Some(out2.commit_oid.clone()),
        base_tree: out2.tree_oid.clone(),
        backend: BackendKind::WindowsProjFs,
        worktree_path: ws_mount.clone(),
        git_worktree_dir: None,
        baseline_id: None,
        dirty_paths: BTreeMap::new(),
        pinned_paths: BTreeSet::new(),
        scope: Vec::new(),
        deny: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        last_checkpoint_oid: None,
    };
    fs::write(
        repo.layout.workspace_state_path(ws_name),
        serde_json::to_string_pretty(&state).expect("json"),
    )
    .expect("write state");

    let projfs_repo = OzoneRepo::discover(Some(&ws_mount)).expect("discover projfs");
    let projfs_status = compute_status(&projfs_repo).expect("projfs status");
    assert!(projfs_status.used_fuse_fast_path);
    assert_eq!(projfs_status.changes.len(), 2);
    assert_eq!(projfs_status.changes[0].path, "a.rs");
    assert_eq!(projfs_status.changes[0].kind, ChangeKind::Modified);
    assert_eq!(projfs_status.changes[1].path, "b.rs");
    assert_eq!(projfs_status.changes[1].kind, ChangeKind::Deleted);

    // Switch branches in main repo with dirty conflict check.
    let sw1 = switch_branch(
        &repo,
        &SwitchOptions {
            create_branch: Some("feat/sw".to_string()),
            ..Default::default()
        },
    )
    .expect("switch -c");
    assert_eq!(sw1.branch.as_deref(), Some("feat/sw"));
    fs::write(repo_root.join("a.rs"), "pub fn a() -> i32 { 77 }\n").expect("edit a on feat/sw");
    commit_changes(&repo, &CommitOptions::new("feat: change a on feat/sw")).expect("commit feat/sw");

    fs::write(repo_root.join("a.rs"), "dirty conflict\n").expect("dirty a");
    let err = switch_branch(
        &repo,
        &SwitchOptions {
            target: Some("main".to_string()),
            ..Default::default()
        },
    )
    .expect_err("dirty overwrite refused");
    assert!(matches!(err, OzError::Conflict { .. }));

    let sw2 = switch_branch(
        &repo,
        &SwitchOptions {
            target: Some("main".to_string()),
            force: true,
            ..Default::default()
        },
    )
    .expect("force switch");
    assert_eq!(sw2.branch.as_deref(), Some("main"));
    assert_eq!(
        fs::read_to_string(repo_root.join("a.rs")).expect("read a on main"),
        "pub fn a() -> i32 { 2 }\n"
    );
}

#[test]
fn status_hardening_spaces_unicode_empty_dirs_anchored_gitignore_racy_index_and_large_tree() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_root = tmp.path().join("edge-mono");
    init_repo_with_user(&repo_root);

    // Anchored root-only gitignore rule `/root_only.tmp` vs nested `nested/root_only.tmp`,
    // plus `*.log` with `!keep.log`.
    fs::write(
        repo_root.join(".gitignore"),
        "/root_only.tmp\n/build_out/\n*.log\n!keep.log\n",
    )
    .expect("write .gitignore");

    // Tracked files with spaces, unicode, and 1,500 files across 30 subdirs (parallel walk + stat cache).
    fs::create_dir_all(repo_root.join("docs/my folder")).expect("mkdir spaces");
    fs::write(repo_root.join("docs/my folder/report (v1).txt"), "hello spaces\n").expect("write spaces");
    fs::create_dir_all(repo_root.join("café")).expect("mkdir unicode");
    fs::write(repo_root.join("café/日本語.rs"), "pub fn greet() -> &'static str { \"こんにちは\" }\n")
        .expect("write unicode");
    fs::write(repo_root.join("same_len.txt"), "AAAA\n").expect("write same_len");

    for d in 0..30 {
        let dir = repo_root.join(format!("pkg_{d:02}/src"));
        fs::create_dir_all(&dir).expect("mkdir pkg");
        for f in 0..50 {
            fs::write(
                dir.join(format!("mod_{f:02}.rs")),
                format!("pub const V_{d}_{f}: usize = {};\n", d * 100 + f),
            )
            .expect("write mod");
        }
    }

    git(&repo_root, &["add", "."]);
    git(&repo_root, &["commit", "-m", "seed edge-case repo"]);
    git(&repo_root, &["branch", "feat/edge-fuse"]);
    let head_oid = git(&repo_root, &["rev-parse", "HEAD"]).trim().to_string();

    // 1. Empty directory must not produce spurious dirty entries.
    fs::create_dir_all(repo_root.join("empty_dir/deep_empty")).expect("mkdir empty");

    // 2. Special FIFO file on Unix must be skipped without blocking `compute_status`.
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let fifo_path = repo_root.join("live.fifo");
        let c_fifo = CString::new(fifo_path.as_os_str().as_bytes()).expect("cstr");
        assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);
    }

    // 3. Anchored `.gitignore` verification on general workdir:
    fs::write(repo_root.join("root_only.tmp"), "ignored at root\n").expect("write root_only");
    fs::create_dir_all(repo_root.join("nested")).expect("mkdir nested");
    fs::write(repo_root.join("nested/root_only.tmp"), "tracked candidate in nested\n")
        .expect("write nested root_only");
    fs::write(repo_root.join("noise.log"), "ignored\n").expect("write noise.log");
    fs::write(repo_root.join("keep.log"), "whitelisted\n").expect("write keep.log");

    // 4. Racy same-second / same-length modification AND `git add` staged-only change:
    //    `compute_status` MUST still compare against HEAD even when `.git/index` matches disk!
    fs::write(repo_root.join("same_len.txt"), "BBBB\n").expect("modify same_len");
    git(&repo_root, &["add", "same_len.txt"]);
    fs::write(repo_root.join("docs/my folder/report (v1).txt"), "hello spaces v2\n")
        .expect("edit spaces");
    fs::write(repo_root.join("café/日本語.rs"), "pub fn greet() -> &'static str { \"やあ\" }\n")
        .expect("edit unicode");
    fs::remove_file(repo_root.join("pkg_05/src/mod_10.rs")).expect("delete tracked");

    let repo = OzoneRepo::discover(Some(&repo_root)).expect("discover");
    let st = compute_status(&repo).expect("status general");
    let summary: Vec<(&str, ChangeKind)> = st
        .changes
        .iter()
        .map(|c| (c.path.as_str(), c.kind))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("café/日本語.rs", ChangeKind::Modified),
            ("docs/my folder/report (v1).txt", ChangeKind::Modified),
            ("keep.log", ChangeKind::Added),
            ("nested/root_only.tmp", ChangeKind::Added),
            ("pkg_05/src/mod_10.rs", ChangeKind::Deleted),
            ("same_len.txt", ChangeKind::Modified),
        ]
    );

    // 5. Verify anchored `.gitignore`, spaces, unicode, FIFO skip, and 300 clean upper files
    //    on the FUSE3 fast path (`scan_fuse_fast_path`)!
    let layout = OzoneLayout::from_git_common_dir(repo_root.join(".git"));
    layout.ensure_initialized().expect("init ozone");
    let ws_name = "ws-edge";
    let upper_dir = layout.ws_upper_dir(ws_name);
    let meta_dir = layout.ws_tombstone_dir(ws_name);
    let mount_point = tmp.path().join("edge-mono.oz").join(ws_name);
    fs::create_dir_all(&upper_dir).expect("mkdir upper");
    fs::create_dir_all(meta_dir.join("whiteouts")).expect("mkdir wh");
    fs::create_dir_all(meta_dir.join("opaque")).expect("mkdir opq");
    fs::create_dir_all(&mount_point).expect("mkdir mp");

    let record = MountRecord {
        schema_version: MountRecord::CURRENT_SCHEMA_VERSION,
        workspace: ws_name.to_string(),
        branch: "feat/edge-fuse".to_string(),
        backend: BackendKind::LinuxFuse3,
        repo_root: repo_root.clone(),
        git_dir: repo_root.join(".git"),
        mount_point: mount_point.clone(),
        upper_dir: Some(upper_dir.clone()),
        meta_dir: Some(meta_dir.clone()),
        base_tree: None,
        base_commit: Some(head_oid),
        socket_path: Some(layout.vfsd_socket_path()),
        daemon_pid: None,
        mounted: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    fs::create_dir_all(layout.ws_meta_dir(ws_name)).expect("mkdir ws meta");
    fs::write(
        layout.ws_mount_record_path(ws_name),
        serde_json::to_string_pretty(&record).expect("json"),
    )
    .expect("write mount.json");

    // 300 committed-identical files in upper_dir -> must all be detected as CLEAN in parallel!
    for d in 0..6 {
        let dir = upper_dir.join(format!("pkg_{d:02}/src"));
        fs::create_dir_all(&dir).expect("mkdir upper pkg");
        for f in 0..50 {
            fs::write(
                dir.join(format!("mod_{f:02}.rs")),
                format!("pub const V_{d}_{f}: usize = {};\n", d * 100 + f),
            )
            .expect("write clean upper");
        }
    }

    // Anchored gitignore + spaces + unicode + empty dir + FIFO in upper_dir
    fs::create_dir_all(upper_dir.join("empty_upper")).expect("mkdir empty_upper");
    fs::write(upper_dir.join("root_only.tmp"), "ignored\n").expect("write root_only");
    fs::create_dir_all(upper_dir.join("build_out")).expect("mkdir build_out");
    fs::write(upper_dir.join("build_out/a.o"), b"\x00\x01").expect("write ignored build_out");
    fs::create_dir_all(upper_dir.join("nested")).expect("mkdir upper nested");
    fs::write(upper_dir.join("nested/root_only.tmp"), "not ignored\n").expect("write nested");
    fs::write(upper_dir.join("noise.log"), "ignored\n").expect("write noise");
    fs::write(upper_dir.join("keep.log"), "kept\n").expect("write keep");
    fs::create_dir_all(upper_dir.join("café")).expect("mkdir upper unicode");
    fs::write(upper_dir.join("café/日本語.rs"), "pub fn greet() -> &'static str { \"edited\" }\n")
        .expect("write upper unicode");

    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let fifo_path = upper_dir.join("upper.fifo");
        let c_fifo = CString::new(fifo_path.as_os_str().as_bytes()).expect("cstr");
        assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);
    }

    let fuse_repo = OzoneRepo::discover(Some(&mount_point)).expect("discover fuse");
    let fuse_st = compute_status(&fuse_repo).expect("status fuse");
    assert!(fuse_st.used_fuse_fast_path);
    let fuse_summary: Vec<(&str, ChangeKind)> = fuse_st
        .changes
        .iter()
        .map(|c| (c.path.as_str(), c.kind))
        .collect();
    assert_eq!(
        fuse_summary,
        vec![
            ("café/日本語.rs", ChangeKind::Modified),
            ("keep.log", ChangeKind::Added),
            ("nested/root_only.tmp", ChangeKind::Added),
        ]
    );
}



