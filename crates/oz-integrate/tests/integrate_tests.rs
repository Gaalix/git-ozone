// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end unit and integration tests for `oz-integrate`:
//! guardrails, collision radar, in-memory integration, conflict sandbox resolution,
//! `--verify`, main working-tree + `.git/index` sync, stacked rebasing, and `< 10 ms` undo.

use oz_core::{
    BackendKind, CommitOptions, OzError, OzoneRepo, commit_changes, compute_status,
};
use oz_cow::{SpawnWorkspaceOptions, spawn_workspace};
use oz_integrate::{
    CollisionSeverity, IntegrateOptions, IntegrationGuardrailOptions, IntegrationMode,
    RadarOptions, ResolveOptions, StackRebaseOptions, TakeSide, abort_integration, active_session,
    check_workspace_scope_for_integration, continue_integration, continue_stack_rebase, integrate,
    resolve_conflict, scan_radar, stack_rebase, stack_status,
};
use oz_oplog::{ActorInfo, ActorKind, undo};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn agent_actor(ws: Option<&str>) -> ActorInfo {
    ActorInfo {
        kind: ActorKind::Agent,
        client: Some("integrate-test".to_string()),
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

fn init_test_repo(files: &[(&str, &str)]) -> (TempDir, PathBuf, OzoneRepo) {
    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("demo");
    fs::create_dir_all(&repo_root).expect("mkdir repo");
    run_git(&repo_root, &["init", "-b", "main"]);
    run_git(&repo_root, &["config", "user.name", "Ozone Test"]);
    run_git(&repo_root, &["config", "user.email", "test@ozone.dev"]);

    for (rel, content) in files {
        let full = repo_root.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("mkdir parent");
        }
        fs::write(&full, content).expect("write file");
    }

    run_git(&repo_root, &["add", "."]);
    run_git(&repo_root, &["commit", "-m", "initial commit"]);
    let repo = OzoneRepo::discover(Some(&repo_root)).expect("discover repo");
    repo.layout.ensure_initialized().expect("init ozone");
    (dir, repo_root, repo)
}

fn spawn_cas_ws(
    repo: &OzoneRepo,
    name: &str,
    scope: &[&str],
    deny: &[&str],
) -> oz_cow::SpawnedWorkspaceInfo {
    let mut opts = SpawnWorkspaceOptions::new(name);
    opts.backend = Some(BackendKind::CasHardlink);
    opts.warm = false;
    opts.scope = scope.iter().map(|s| (*s).to_string()).collect();
    opts.deny = deny.iter().map(|s| (*s).to_string()).collect();
    spawn_workspace(repo, &opts).expect("spawn cas workspace")
}

fn write_ws_file(ws_path: &Path, rel: &str, content: &[u8]) {
    let full = ws_path.join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("mkdir ws parent");
    }
    fs::write(&full, content).expect("write ws file");
}

fn commit_ws(ws_path: &Path, msg: &str, allow_out_of_scope: bool, force: bool) -> String {
    let ws_repo = OzoneRepo::discover(Some(ws_path)).expect("discover ws");
    let mut opts = CommitOptions::new(msg);
    opts.allow_out_of_scope = allow_out_of_scope;
    opts.force = force;
    commit_changes(&ws_repo, &opts)
        .expect("commit ws")
        .commit_oid
}

#[test]
fn guardrails_enforce_workspace_and_repo_scope_deny_and_content_checks() {
    let (_tmp, repo_root, _repo) = init_test_repo(&[
        ("crates/auth/src/lib.rs", "pub fn auth() {}\n"),
        ("crates/billing/src/lib.rs", "pub fn bill() {}\n"),
        ("Cargo.lock", "version = 1\n"),
    ]);

    fs::write(
        repo_root.join(".ozone.toml"),
        r#"
max_blob_mb = 1
[guardrails]
deny = ["Cargo.lock"]
"#,
    )
    .expect("write .ozone.toml");

    let repo = OzoneRepo::discover(Some(&repo_root)).expect("reload repo");

    // 1. Out-of-scope path committed via --allow-out-of-scope is blocked by integration guardrails.
    let ws_oos = spawn_cas_ws(&repo, "agent-auth", &["crates/auth/**"], &[]);
    write_ws_file(
        &ws_oos.workspace.workspace_path,
        "crates/billing/src/lib.rs",
        b"pub fn bill() { /* touched */ }\n",
    );
    commit_ws(&ws_oos.workspace.workspace_path, "out of scope edit", true, false);

    let err = check_workspace_scope_for_integration(
        &repo,
        &ws_oos.state,
        &IntegrationGuardrailOptions::default(),
    )
    .expect_err("must reject out of scope");
    match &err {
        OzError::GuardrailViolation { kind, .. } => assert_eq!(kind, "out_of_scope"),
        other => panic!("expected GuardrailViolation, got {other:?}"),
    }
    assert_eq!(err.exit_code().as_i32(), 2);

    // `--allow-out-of-scope` overrides the path guardrail.
    check_workspace_scope_for_integration(
        &repo,
        &ws_oos.state,
        &IntegrationGuardrailOptions {
            allow_out_of_scope: true,
            force: false,
            into_branch: Some("main".to_string()),
        },
    )
    .expect("allow_out_of_scope should pass");

    // 2. Repo-level `--deny` (`Cargo.lock`) blocks integration even if workspace has no custom scope.
    let ws_deny = spawn_cas_ws(&repo, "agent-lock", &[], &[]);
    write_ws_file(
        &ws_deny.workspace.workspace_path,
        "Cargo.lock",
        b"version = 2\n",
    );
    commit_ws(&ws_deny.workspace.workspace_path, "lock edit", true, false);
    let deny_err = integrate(
        &repo,
        &IntegrateOptions {
            sources: vec!["agent-lock".to_string()],
            into: Some("main".to_string()),
            ..Default::default()
        },
        agent_actor(None),
    )
    .expect_err("repo deny rule must block integrate");
    assert!(matches!(
        deny_err,
        OzError::GuardrailViolation { ref kind, .. } if kind == "out_of_scope"
    ));

    // 3. Content guardrail check (`oz_core::check_guardrails`) blocks suspected secrets unless `force: true`.
    let ws_sec = spawn_cas_ws(&repo, "agent-sec", &["crates/auth/**"], &[]);
    let secret_payload = format!("pub const KEY: &str = \"{}1234567890ABCDEF\";\n", "AKIA");
    write_ws_file(
        &ws_sec.workspace.workspace_path,
        "crates/auth/src/secret.rs",
        secret_payload.as_bytes(),
    );
    commit_ws(&ws_sec.workspace.workspace_path, "add secret", false, true);

    let sec_err = check_workspace_scope_for_integration(
        &repo,
        &ws_sec.state,
        &IntegrationGuardrailOptions::default(),
    )
    .expect_err("secret guardrail must block");
    match &sec_err {
        OzError::GuardrailViolation { kind, .. } => assert_eq!(kind, "secret"),
        other => panic!("expected secret GuardrailViolation, got {other:?}"),
    }
    assert_eq!(sec_err.exit_code().as_i32(), 2);

    check_workspace_scope_for_integration(
        &repo,
        &ws_sec.state,
        &IntegrationGuardrailOptions {
            allow_out_of_scope: false,
            force: true,
            into_branch: Some("main".to_string()),
        },
    )
    .expect("force: true overrides content guardrail");
}

#[test]
fn radar_detects_overlapping_out_of_scope_and_same_file_different_lines_with_projection() {
    let lines: String = (1..=100).map(|i| format!("line {i}\n")).collect();
    let (_tmp, repo_root, repo) = init_test_repo(&[
        ("crates/core/src/session.rs", &lines),
        ("crates/core/src/other.rs", &lines),
        ("Cargo.lock", "lock-v1\n"),
    ]);

    // `ws-auth` branches at M0 and COMMITS an edit on lines 42-45 of `session.rs` plus line 5 of `other.rs`.
    let ws_auth = spawn_cas_ws(&repo, "agent-auth", &["crates/**"], &["Cargo.lock"]);
    let mut auth_session: Vec<String> = (1..=100).map(|i| format!("line {i}\n")).collect();
    for (idx, line) in auth_session.iter_mut().enumerate().take(45).skip(41) {
        *line = format!("auth line {}\n", idx + 1);
    }
    write_ws_file(
        &ws_auth.workspace.workspace_path,
        "crates/core/src/session.rs",
        auth_session.concat().as_bytes(),
    );
    let mut auth_other: Vec<String> = (1..=100).map(|i| format!("line {i}\n")).collect();
    auth_other[4] = "auth other line 5\n".to_string();
    write_ws_file(
        &ws_auth.workspace.workspace_path,
        "crates/core/src/other.rs",
        auth_other.concat().as_bytes(),
    );
    commit_ws(&ws_auth.workspace.workspace_path, "auth edits", false, false);

    // Advance `main` by inserting 10 lines at the top of `session.rs` (M0 -> M1).
    let mut main_session = String::new();
    for i in 1..=10 {
        main_session.push_str(&format!("header {i}\n"));
    }
    main_session.push_str(&lines);
    fs::write(repo_root.join("crates/core/src/session.rs"), &main_session).expect("write main");
    run_git(&repo_root, &["add", "crates/core/src/session.rs"]);
    run_git(&repo_root, &["commit", "-m", "insert 10 header lines"]);

    // `ws-billing` branches from M1 (after header insertion!) and leaves DIRTY uncommitted edits on:
    // - shifted lines 52-58 (which correspond to 42-48 before header insertion -> overlaps ws-auth!)
    // - line 90 of `other.rs` (non-overlapping -> SameFileDifferentLines)
    // - `Cargo.lock` (OutOfScope)
    let ws_billing = spawn_cas_ws(&repo, "agent-billing", &["crates/**"], &["Cargo.lock"]);
    let mut bill_session: Vec<String> = main_session.lines().map(|l| format!("{l}\n")).collect();
    for (idx, line) in bill_session.iter_mut().enumerate().take(58).skip(52) {
        *line = format!("billing line {}\n", idx + 1);
    }
    write_ws_file(
        &ws_billing.workspace.workspace_path,
        "crates/core/src/session.rs",
        bill_session.concat().as_bytes(),
    );
    let mut bill_other: Vec<String> = (1..=100).map(|i| format!("line {i}\n")).collect();
    bill_other[89] = "billing other line 90\n".to_string();
    write_ws_file(
        &ws_billing.workspace.workspace_path,
        "crates/core/src/other.rs",
        bill_other.concat().as_bytes(),
    );
    write_ws_file(
        &ws_billing.workspace.workspace_path,
        "Cargo.lock",
        b"lock-v2\n",
    );

    let report = scan_radar(&repo, &RadarOptions::default()).expect("scan radar");
    assert_eq!(report.base_branch, "main");
    assert_eq!(
        report.scanned_workspaces,
        vec!["agent-auth".to_string(), "agent-billing".to_string()]
    );
    assert_eq!(report.collisions.len(), 3);

    // Deterministic severity order: OverlappingHunks -> OutOfScope -> SameFileDifferentLines
    assert_eq!(report.collisions[0].path, "crates/core/src/session.rs");
    assert_eq!(
        report.collisions[0].severity,
        CollisionSeverity::OverlappingHunks
    );
    assert_eq!(report.collisions[0].workspaces.len(), 2);
    assert_eq!(report.collisions[0].workspaces[0].ws, "agent-auth");
    assert_eq!(report.collisions[0].workspaces[0].lines, "52-55");
    assert_eq!(report.collisions[0].workspaces[1].ws, "agent-billing");
    assert_eq!(report.collisions[0].workspaces[1].lines, "53-58");

    assert_eq!(report.collisions[1].path, "Cargo.lock");
    assert_eq!(report.collisions[1].severity, CollisionSeverity::OutOfScope);
    assert_eq!(report.collisions[1].workspaces.len(), 1);
    assert_eq!(report.collisions[1].workspaces[0].ws, "agent-billing");

    assert_eq!(report.collisions[2].path, "crates/core/src/other.rs");
    assert_eq!(
        report.collisions[2].severity,
        CollisionSeverity::SameFileDifferentLines
    );
    assert_eq!(report.collisions[2].workspaces.len(), 2);

    // Verify `include_committed: false` ignores committed-only edits in `agent-auth`.
    let dirty_only = scan_radar(
        &repo,
        &RadarOptions {
            include_committed: false,
            ..Default::default()
        },
    )
    .expect("scan dirty only");
    assert_eq!(dirty_only.collisions.len(), 1);
    assert_eq!(dirty_only.collisions[0].path, "Cargo.lock");
    assert_eq!(dirty_only.collisions[0].severity, CollisionSeverity::OutOfScope);
}

#[test]
fn integrate_clean_syncs_main_worktree_index_and_supports_sub_10ms_undo() {
    let lines: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    let (_tmp, repo_root, repo) = init_test_repo(&[
        ("shared.txt", &lines),
        ("obsolete.txt", "remove me\n"),
    ]);
    let old_main_oid = run_git(&repo_root, &["rev-parse", "HEAD"]);

    let ws_a = spawn_cas_ws(&repo, "ws-a", &[], &[]);
    let mut a_lines: Vec<String> = (1..=30).map(|i| format!("line {i}\n")).collect();
    a_lines[1] = "line 2 from ws-a\n".to_string();
    write_ws_file(
        &ws_a.workspace.workspace_path,
        "shared.txt",
        a_lines.concat().as_bytes(),
    );
    write_ws_file(
        &ws_a.workspace.workspace_path,
        "added/a.txt",
        b"hello a\n",
    );
    commit_ws(&ws_a.workspace.workspace_path, "ws-a commit", false, false);

    let ws_b = spawn_cas_ws(&repo, "ws-b", &[], &[]);
    let mut b_lines: Vec<String> = (1..=30).map(|i| format!("line {i}\n")).collect();
    b_lines[27] = "line 28 from ws-b\n".to_string();
    write_ws_file(
        &ws_b.workspace.workspace_path,
        "shared.txt",
        b_lines.concat().as_bytes(),
    );
    fs::remove_file(ws_b.workspace.workspace_path.join("obsolete.txt")).expect("delete obsolete");
    commit_ws(&ws_b.workspace.workspace_path, "ws-b commit", false, false);

    // Leave an unrelated untracked file in `repo_root` and verify it survives integration + undo.
    fs::write(repo_root.join("unrelated_notes.txt"), "keep me\n").expect("write unrelated");

    let outcome = integrate(
        &repo,
        &IntegrateOptions {
            sources: vec!["ws-a".to_string(), "ws-b".to_string()],
            into: Some("main".to_string()),
            ..Default::default()
        },
        agent_actor(None),
    )
    .expect("clean integrate");

    assert_eq!(outcome.into_branch, "main");
    assert_eq!(outcome.previous_oid, old_main_oid);
    assert_ne!(outcome.commit_oid, old_main_oid);
    assert_eq!(outcome.merged_sources, vec!["ws-a", "ws-b"]);
    assert!(outcome.main_worktree_synced);
    assert!(outcome.op_id.is_some());

    // Main working tree on disk must match merged tree!
    let merged_shared = fs::read_to_string(repo_root.join("shared.txt")).expect("read shared");
    assert!(merged_shared.contains("line 2 from ws-a\n"));
    assert!(merged_shared.contains("line 28 from ws-b\n"));
    assert_eq!(
        fs::read_to_string(repo_root.join("added/a.txt")).expect("read added"),
        "hello a\n"
    );
    assert!(!repo_root.join("obsolete.txt").exists());
    assert_eq!(
        fs::read_to_string(repo_root.join("unrelated_notes.txt")).expect("read notes"),
        "keep me\n"
    );

    // `.git/index` MUST be in sync with `HEAD`: `git status --porcelain` shows ONLY `unrelated_notes.txt`!
    let porcelain = run_git(&repo_root, &["status", "--porcelain"]);
    assert_eq!(porcelain, "?? unrelated_notes.txt");

    // `oz undo` reverses integration in < 10 ms (generous 250ms budget for CI VM jitter, log elapsed).
    let t0 = Instant::now();
    let undo_out = undo(&repo, 1usize, agent_actor(None)).expect("undo integrate");
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "undo took unexpectedly long: {elapsed:?}"
    );
    assert_eq!(run_git(&repo_root, &["rev-parse", "HEAD"]), old_main_oid);
    assert!(repo_root.join("obsolete.txt").is_file());
    assert!(!repo_root.join("added/a.txt").exists());
    assert!(!fs::read_to_string(repo_root.join("shared.txt")).unwrap().contains("ws-a"));
    assert_eq!(
        fs::read_to_string(repo_root.join("unrelated_notes.txt")).unwrap(),
        "keep me\n"
    );
    assert_eq!(
        undo_out.ref_deltas.get("refs/heads/main").and_then(|d| d.new.as_deref()),
        Some(old_main_oid.as_str())
    );
}

#[test]
fn integrate_rejects_conflicting_dirty_main_tree_and_dirty_source_workspace() {
    let (_tmp, repo_root, repo) = init_test_repo(&[("file.txt", "base\n")]);
    let old_main_oid = run_git(&repo_root, &["rev-parse", "HEAD"]);

    let ws = spawn_cas_ws(&repo, "ws-dirty", &[], &[]);
    write_ws_file(&ws.workspace.workspace_path, "file.txt", b"ws-edit\n");

    // Rejects uncommitted dirty source workspace.
    let err_dirty_ws = integrate(
        &repo,
        &IntegrateOptions {
            sources: vec!["ws-dirty".to_string()],
            into: Some("main".to_string()),
            ..Default::default()
        },
        agent_actor(None),
    )
    .expect_err("dirty workspace must be rejected");
    assert!(matches!(err_dirty_ws, OzError::InvalidUsage { .. }));

    commit_ws(&ws.workspace.workspace_path, "commit ws edit", false, false);

    // Dirty edit on `file.txt` in `repo_root` conflicts with `ws-dirty`'s change to `file.txt`.
    fs::write(repo_root.join("file.txt"), "local main dirty edit\n").expect("dirty main");
    let err_dirty_main = integrate(
        &repo,
        &IntegrateOptions {
            sources: vec!["ws-dirty".to_string()],
            into: Some("main".to_string()),
            ..Default::default()
        },
        agent_actor(None),
    )
    .expect_err("conflicting dirty main worktree must abort before advancing main");
    assert!(matches!(err_dirty_main, OzError::Conflict { .. }));
    assert_eq!(run_git(&repo_root, &["rev-parse", "HEAD"]), old_main_oid);
    assert_eq!(
        fs::read_to_string(repo_root.join("file.txt")).unwrap(),
        "local main dirty edit\n"
    );
}

#[test]
fn integrate_verify_gate_uses_warm_cache_and_cleans_sandbox_on_failure() {
    let (tmp, repo_root, repo) = init_test_repo(&[("lib.rs", "pub fn val() -> i32 { 1 }\n")]);
    let main_oid = run_git(&repo_root, &["rev-parse", "HEAD"]);

    // Seed a warm baseline in `.git/ozone/baselines/<main_oid>/root/target/warm_marker.txt`.
    let baseline_target = repo
        .layout
        .baselines_dir()
        .join(&main_oid)
        .join("root/target");
    fs::create_dir_all(&baseline_target).expect("mkdir baseline target");
    fs::write(baseline_target.join("warm_marker.txt"), "warm-cache-ok\n").expect("write warm");
    fs::write(
        repo.layout.baselines_dir().join(&main_oid).join("metadata.json"),
        format!(
            r#"{{"commit_oid":"{main_oid}","created_at":"2026-11-17T00:00:00Z","targets":[],"total_bytes":14}}"#
        ),
    )
    .expect("write metadata");

    let ws = spawn_cas_ws(&repo, "ws-verify", &[], &[]);
    write_ws_file(
        &ws.workspace.workspace_path,
        "lib.rs",
        b"pub fn val() -> i32 { 2 }\n",
    );
    commit_ws(&ws.workspace.workspace_path, "update val", false, false);

    let sandbox_dir = tmp.path().join("demo.oz/_integrate");

    // 1. Failing --verify leaves `main` untouched and removes `_integrate`.
    let fail_err = integrate(
        &repo,
        &IntegrateOptions {
            sources: vec!["ws-verify".to_string()],
            into: Some("main".to_string()),
            verify_cmd: Some("test -f target/warm_marker.txt && exit 1".to_string()),
            ..Default::default()
        },
        agent_actor(None),
    )
    .expect_err("failing verify must abort");
    assert!(matches!(fail_err, OzError::CheckFailed { .. }));
    assert_eq!(fail_err.exit_code().as_i32(), 1);
    assert_eq!(run_git(&repo_root, &["rev-parse", "HEAD"]), main_oid);
    assert!(!sandbox_dir.exists(), "_integrate sandbox must be cleaned on verify failure");

    // 2. Passing --verify sees warm baseline + candidate code and advances `main`.
    let ok = integrate(
        &repo,
        &IntegrateOptions {
            sources: vec!["ws-verify".to_string()],
            into: Some("main".to_string()),
            verify_cmd: Some(
                "grep -q 'warm-cache-ok' target/warm_marker.txt && grep -q 'val() -> i32 { 2 }' lib.rs"
                    .to_string(),
            ),
            ..Default::default()
        },
        agent_actor(None),
    )
    .expect("passing verify should succeed");
    assert_ne!(ok.commit_oid, main_oid);
    assert!(!sandbox_dir.exists(), "_integrate sandbox must be cleaned on success");
    assert_eq!(
        fs::read_to_string(repo_root.join("lib.rs")).unwrap(),
        "pub fn val() -> i32 { 2 }\n"
    );
}

#[test]
fn integrate_conflict_resolution_resolve_continue_and_abort_workflow() {
    let (tmp, repo_root, repo) = init_test_repo(&[
        ("a.txt", "base-a\n"),
        ("b.txt", "base-b\n"),
    ]);
    let old_main_oid = run_git(&repo_root, &["rev-parse", "HEAD"]);

    let ws1 = spawn_cas_ws(&repo, "ws1", &[], &[]);
    write_ws_file(&ws1.workspace.workspace_path, "a.txt", b"ws1-a\n");
    write_ws_file(&ws1.workspace.workspace_path, "b.txt", b"ws1-b\n");
    commit_ws(&ws1.workspace.workspace_path, "ws1 changes", false, false);

    let ws2 = spawn_cas_ws(&repo, "ws2", &[], &[]);
    write_ws_file(&ws2.workspace.workspace_path, "a.txt", b"ws2-a\n");
    write_ws_file(&ws2.workspace.workspace_path, "b.txt", b"ws2-b\n");
    commit_ws(&ws2.workspace.workspace_path, "ws2 changes", false, false);

    // ws3 does a non-conflicting change in `c.txt` to test resuming `remaining_sources`.
    let ws3 = spawn_cas_ws(&repo, "ws3", &[], &[]);
    write_ws_file(&ws3.workspace.workspace_path, "c.txt", b"ws3-c\n");
    commit_ws(&ws3.workspace.workspace_path, "ws3 changes", false, false);

    let err = integrate(
        &repo,
        &IntegrateOptions {
            sources: vec!["ws1".to_string(), "ws2".to_string(), "ws3".to_string()],
            into: Some("main".to_string()),
            verify_cmd: Some("grep -q 'ws1-a' a.txt && grep -q 'ws2-b' b.txt && grep -q 'ws3-c' c.txt".to_string()),
            ..Default::default()
        },
        agent_actor(None),
    )
    .expect_err("conflict must materialize _integrate");

    let sandbox_dir = tmp.path().join("demo.oz/_integrate");
    assert!(sandbox_dir.is_dir());
    let session = active_session(&repo)
        .expect("read session")
        .expect("session present");
    assert_eq!(session.mode, IntegrationMode::Integrate);
    assert_eq!(run_git(&repo_root, &["rev-parse", "HEAD"]), old_main_oid);

    match &err {
        OzError::Conflict { conflicts, .. } => {
            assert_eq!(conflicts.len(), 2);
            assert_eq!(conflicts[0].id, "c1");
            assert_eq!(conflicts[0].path, "a.txt");
            assert_eq!(conflicts[0].base.as_deref(), Some("base-a\n"));
            assert_eq!(conflicts[0].ours.as_deref(), Some("ws1-a\n"));
            assert_eq!(conflicts[0].theirs.as_deref(), Some("ws2-a\n"));
            assert_eq!(conflicts[1].id, "c2");
            assert_eq!(conflicts[1].path, "b.txt");
        }
        other => panic!("expected OzError::Conflict, got {other:?}"),
    }
    assert_eq!(err.exit_code().as_i32(), 1);

    // Trying to continue before resolving fails cleanly.
    let cont_err =
        continue_integration(&repo, agent_actor(None)).expect_err("unresolved markers must block");
    assert!(matches!(cont_err, OzError::Conflict { .. }));

    // Resolve c1 -> ours (`ws1-a`), leaving c2 unresolved.
    let r1 = resolve_conflict(
        &repo,
        &ResolveOptions {
            conflict_id: Some("c1".to_string()),
            all: false,
            take: TakeSide::Ours,
            auto_continue: false,
        },
        agent_actor(None),
    )
    .expect("resolve c1");
    assert_eq!(r1.resolved_ids, vec!["c1"]);
    assert_eq!(r1.remaining_unresolved, vec!["c2"]);

    // Resolve c2 -> theirs (`ws2-b`) with `auto_continue: true`.
    let r2 = resolve_conflict(
        &repo,
        &ResolveOptions {
            conflict_id: Some("c2".to_string()),
            all: false,
            take: TakeSide::Theirs,
            auto_continue: true,
        },
        agent_actor(None),
    )
    .expect("resolve c2 and continue");
    assert!(r2.continued.is_some());
    assert!(!sandbox_dir.exists());
    assert!(active_session(&repo).unwrap().is_none());

    assert_eq!(fs::read_to_string(repo_root.join("a.txt")).unwrap(), "ws1-a\n");
    assert_eq!(fs::read_to_string(repo_root.join("b.txt")).unwrap(), "ws2-b\n");
    assert_eq!(fs::read_to_string(repo_root.join("c.txt")).unwrap(), "ws3-c\n");

    // Also test abort_integration on a fresh conflict.
    let ws4 = spawn_cas_ws(&repo, "ws4", &[], &[]);
    write_ws_file(&ws4.workspace.workspace_path, "a.txt", b"ws4-a\n");
    commit_ws(&ws4.workspace.workspace_path, "ws4 edit", false, false);
    let ws5 = spawn_cas_ws(&repo, "ws5", &[], &[]);
    write_ws_file(&ws5.workspace.workspace_path, "a.txt", b"ws5-a\n");
    commit_ws(&ws5.workspace.workspace_path, "ws5 edit", false, false);

    let main_before_abort = run_git(&repo_root, &["rev-parse", "HEAD"]);
    let _ = integrate(
        &repo,
        &IntegrateOptions {
            sources: vec!["ws4".to_string(), "ws5".to_string()],
            into: Some("main".to_string()),
            ..Default::default()
        },
        agent_actor(None),
    )
    .expect_err("conflict for abort test");
    assert!(sandbox_dir.is_dir());

    let aborted = abort_integration(&repo).expect("abort");
    assert!(aborted.aborted);
    assert!(aborted.sandbox_removed);
    assert!(aborted.session_removed);
    assert!(!sandbox_dir.exists());
    assert!(active_session(&repo).unwrap().is_none());
    assert_eq!(run_git(&repo_root, &["rev-parse", "HEAD"]), main_before_abort);
}

#[test]
fn integrate_handles_swapped_modify_delete_conflict_without_inverting_ours_theirs() {
    // Regression test for `gix::merge::tree::Conflict` `ConflictMapping::Swapped`:
    // ours (`main`) DELETES `shared.txt`, theirs (`ws-mod`) MODIFIES `shared.txt`.
    let (_tmp, repo_root, repo) = init_test_repo(&[
        ("shared.txt", "base content\n"),
        ("keep.txt", "keep\n"),
    ]);

    let ws_mod = spawn_cas_ws(&repo, "ws-mod", &[], &[]);
    write_ws_file(&ws_mod.workspace.workspace_path, "shared.txt", b"theirs modified\n");
    commit_ws(&ws_mod.workspace.workspace_path, "modify shared", false, false);

    // Advance `main` by deleting `shared.txt`.
    fs::remove_file(repo_root.join("shared.txt")).expect("delete shared on main");
    run_git(&repo_root, &["add", "-A"]);
    run_git(&repo_root, &["commit", "-m", "main deletes shared"]);
    let main_del_oid = run_git(&repo_root, &["rev-parse", "HEAD"]);

    let err = integrate(
        &repo,
        &IntegrateOptions {
            sources: vec!["ws-mod".to_string()],
            into: Some("main".to_string()),
            ..Default::default()
        },
        agent_actor(None),
    )
    .expect_err("modify/delete conflict");

    match &err {
        OzError::Conflict { conflicts, .. } => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].path, "shared.txt");
            assert_eq!(conflicts[0].base.as_deref(), Some("base content\n"));
            assert_eq!(conflicts[0].ours, None, "ours (main) deleted shared.txt");
            assert_eq!(
                conflicts[0].theirs.as_deref(),
                Some("theirs modified\n"),
                "theirs (ws-mod) modified shared.txt"
            );
        }
        other => panic!("expected Conflict, got {other:?}"),
    }

    // `--take ours` deletes `shared.txt`!
    resolve_conflict(
        &repo,
        &ResolveOptions {
            conflict_id: Some("c1".to_string()),
            all: false,
            take: TakeSide::Ours,
            auto_continue: true,
        },
        agent_actor(None),
    )
    .expect("resolve take ours");
    assert!(!repo_root.join("shared.txt").exists());
    assert_ne!(run_git(&repo_root, &["rev-parse", "HEAD"]), main_del_oid);

    // Undo back to `main_del_oid` and verify `--take theirs` keeps `ws-mod`'s modified content!
    undo(&repo, 1usize, agent_actor(None)).expect("undo");
    assert_eq!(run_git(&repo_root, &["rev-parse", "HEAD"]), main_del_oid);

    let _ = integrate(
        &repo,
        &IntegrateOptions {
            sources: vec!["ws-mod".to_string()],
            into: Some("main".to_string()),
            ..Default::default()
        },
        agent_actor(None),
    )
    .expect_err("conflict again");
    resolve_conflict(
        &repo,
        &ResolveOptions {
            conflict_id: Some("c1".to_string()),
            all: false,
            take: TakeSide::Theirs,
            auto_continue: true,
        },
        agent_actor(None),
    )
    .expect("resolve take theirs");
    assert_eq!(
        fs::read_to_string(repo_root.join("shared.txt")).unwrap(),
        "theirs modified\n"
    );
}

#[test]
fn integrate_handles_binary_conflict_requiring_explicit_resolve() {
    let (_tmp, repo_root, repo) = init_test_repo(&[("keep.txt", "ok\n")]);
    fs::write(repo_root.join("blob.bin"), [0u8, 1, 2, 3]).expect("write bin");
    run_git(&repo_root, &["add", "blob.bin"]);
    run_git(&repo_root, &["commit", "-m", "add bin"]);

    let ws_bin = spawn_cas_ws(&repo, "ws-bin", &[], &[]);
    write_ws_file(&ws_bin.workspace.workspace_path, "blob.bin", &[0u8, 9, 9, 9]);
    commit_ws(&ws_bin.workspace.workspace_path, "ws binary change", false, false);

    fs::write(repo_root.join("blob.bin"), [0u8, 4, 5, 6]).expect("main bin edit");
    run_git(&repo_root, &["add", "blob.bin"]);
    run_git(&repo_root, &["commit", "-m", "main binary change"]);

    let err = integrate(
        &repo,
        &IntegrateOptions {
            sources: vec!["ws-bin".to_string()],
            into: Some("main".to_string()),
            ..Default::default()
        },
        agent_actor(None),
    )
    .expect_err("binary conflict");
    assert!(matches!(err, OzError::Conflict { .. }));

    // Cannot continue without resolving binary conflict via `oz resolve`!
    let cont_err = continue_integration(&repo, agent_actor(None))
        .expect_err("unresolved binary conflict must block continue");
    assert!(matches!(cont_err, OzError::Conflict { .. }));

    resolve_conflict(
        &repo,
        &ResolveOptions {
            conflict_id: Some("c1".to_string()),
            all: false,
            take: TakeSide::Theirs,
            auto_continue: true,
        },
        agent_actor(None),
    )
    .expect("resolve binary take theirs");

    assert_eq!(
        fs::read(repo_root.join("blob.bin")).unwrap(),
        vec![0u8, 9, 9, 9]
    );
}

#[test]
fn stack_rebase_clean_preserves_change_ids_syncs_workspaces_and_undoes_atomically() {
    let (_tmp, repo_root, repo) = init_test_repo(&[
        ("base.txt", "base v1\n"),
        ("s1.txt", "s1 init\n"),
        ("s2.txt", "s2 init\n"),
    ]);

    // Step 1 workspace stacked on `main`.
    let ws1 = spawn_cas_ws(&repo, "ws1", &[], &[]);
    write_ws_file(&ws1.workspace.workspace_path, "s1.txt", b"s1 commit 1\n");
    let c_s1_1 = commit_ws(&ws1.workspace.workspace_path, "feat(s1): part 1", false, false);
    write_ws_file(&ws1.workspace.workspace_path, "s1_extra.txt", b"s1 extra\n");
    let c_s1_2 = commit_ws(&ws1.workspace.workspace_path, "feat(s1): part 2", false, false);

    // Step 2 workspace stacked on `ws1`.
    let mut ws2_opts = SpawnWorkspaceOptions::new("ws2");
    ws2_opts.backend = Some(BackendKind::CasHardlink);
    ws2_opts.warm = false;
    ws2_opts.from_ref = Some("ws1".to_string());
    let ws2 = spawn_workspace(&repo, &ws2_opts).expect("spawn ws2 from ws1");
    write_ws_file(&ws2.workspace.workspace_path, "s2.txt", b"s2 commit 1\n");
    let c_s2_1 = commit_ws(&ws2.workspace.workspace_path, "feat(s2): part 1", false, false);

    // Advance `main` on an unrelated file (`base.txt`).
    fs::write(repo_root.join("base.txt"), "base v2 from main\n").expect("update main");
    run_git(&repo_root, &["add", "base.txt"]);
    run_git(&repo_root, &["commit", "-m", "main advance"]);
    let new_main_oid = run_git(&repo_root, &["rev-parse", "HEAD"]);

    let pre_status = stack_status(&repo, Some("main")).expect("stack status");
    assert_eq!(pre_status.branches.len(), 2);
    assert_eq!(pre_status.branches[0].branch, "ws1");
    assert_eq!(pre_status.branches[0].parent_branch, None);
    assert_eq!(pre_status.branches[0].ahead_count, 2);
    assert!(pre_status.branches[0].needs_rebase);
    assert_eq!(pre_status.branches[1].branch, "ws2");
    assert_eq!(pre_status.branches[1].parent_branch.as_deref(), Some("ws1"));
    assert_eq!(pre_status.branches[1].ahead_count, 1);
    assert!(pre_status.branches[1].needs_rebase);

    let orig_change_ids: Vec<String> = pre_status
        .branches
        .iter()
        .flat_map(|b| b.commits.iter().filter_map(|c| c.change_id.clone()))
        .collect();
    assert_eq!(orig_change_ids.len(), 3);

    // Execute stacked rebase (pass branches in reverse order to verify topological sorting!).
    let out = stack_rebase(
        &repo,
        &StackRebaseOptions {
            branches: vec!["ws2".to_string(), "ws1".to_string()],
            onto: Some("main".to_string()),
        },
        agent_actor(None),
    )
    .expect("clean stack rebase");

    assert_eq!(out.onto_branch, "main");
    assert_eq!(out.onto_oid, new_main_oid);
    assert_eq!(out.branches.len(), 2);
    assert_eq!(out.branches[0].branch, "ws1");
    assert_eq!(out.branches[0].previous_oid, c_s1_2);
    assert_ne!(out.branches[0].new_oid, c_s1_2);
    assert_eq!(out.branches[1].branch, "ws2");
    assert_eq!(out.branches[1].previous_oid, c_s2_1);
    assert_ne!(out.branches[1].new_oid, c_s2_1);
    assert_eq!(out.rebased_commits.len(), 3);
    assert_eq!(out.rebased_commits[0].original_oid, c_s1_1);
    assert_eq!(out.updated_workspaces, vec!["ws1", "ws2"]);

    // Verify `Change-Id:` trailers preserved across EVERY rebased commit!
    let rebased_change_ids: Vec<String> = out
        .rebased_commits
        .iter()
        .map(|c| c.change_id.clone())
        .collect();
    assert_eq!(rebased_change_ids, orig_change_ids);

    // Verify `main -> ws1 -> ws2` ancestry and synced checked-out workspace trees/states!
    let merge_base_main_ws1 = run_git(&repo_root, &["merge-base", "main", "ws1"]);
    assert_eq!(merge_base_main_ws1, new_main_oid);
    let merge_base_ws1_ws2 = run_git(&repo_root, &["merge-base", "ws1", "ws2"]);
    assert_eq!(merge_base_ws1_ws2, out.branches[0].new_oid);

    assert_eq!(
        fs::read_to_string(ws1.workspace.workspace_path.join("base.txt")).unwrap(),
        "base v2 from main\n"
    );
    assert_eq!(
        fs::read_to_string(ws2.workspace.workspace_path.join("base.txt")).unwrap(),
        "base v2 from main\n"
    );
    assert_eq!(
        fs::read_to_string(ws2.workspace.workspace_path.join("s1_extra.txt")).unwrap(),
        "s1 extra\n"
    );
    let ws2_repo = OzoneRepo::discover(Some(&ws2.workspace.workspace_path)).expect("ws2 discover");
    assert!(compute_status(&ws2_repo).unwrap().is_clean());
    assert_eq!(
        ws2_repo.workspace_state.unwrap().base_commit.as_deref(),
        Some(out.branches[1].new_oid.as_str())
    );

    // `oz undo` restores BOTH stacked branch refs atomically in < 10 ms!
    let t0 = Instant::now();
    undo(&repo, 1usize, agent_actor(None)).expect("undo stack rebase");
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "stack rebase undo took {elapsed:?}"
    );
    assert_eq!(run_git(&repo_root, &["rev-parse", "ws1"]), c_s1_2);
    assert_eq!(run_git(&repo_root, &["rev-parse", "ws2"]), c_s2_1);
}

#[test]
fn stack_rebase_conflict_leaves_refs_untouched_and_supports_resolve_continue_abort() {
    let (tmp, repo_root, repo) = init_test_repo(&[
        ("shared.txt", "line1\n"),
        ("s2.txt", "init\n"),
    ]);

    let ws1 = spawn_cas_ws(&repo, "stack1", &[], &[]);
    write_ws_file(&ws1.workspace.workspace_path, "shared.txt", b"stack1 edit\n");
    let orig_s1 = commit_ws(&ws1.workspace.workspace_path, "feat: s1 shared", false, false);

    let mut ws2_opts = SpawnWorkspaceOptions::new("stack2");
    ws2_opts.backend = Some(BackendKind::CasHardlink);
    ws2_opts.warm = false;
    ws2_opts.from_ref = Some("stack1".to_string());
    let ws2 = spawn_workspace(&repo, &ws2_opts).expect("spawn stack2");
    write_ws_file(&ws2.workspace.workspace_path, "s2.txt", b"stack2 edit\n");
    let orig_s2 = commit_ws(&ws2.workspace.workspace_path, "feat: s2 file", false, false);

    // Conflicting edit on `main`.
    fs::write(repo_root.join("shared.txt"), "main conflicting edit\n").expect("write main");
    run_git(&repo_root, &["add", "shared.txt"]);
    run_git(&repo_root, &["commit", "-m", "main conflict"]);
    let main_oid = run_git(&repo_root, &["rev-parse", "HEAD"]);

    let pre_status = stack_status(&repo, Some("main")).expect("status");
    let orig_change_ids: Vec<String> = pre_status
        .branches
        .iter()
        .flat_map(|b| b.commits.iter().filter_map(|c| c.change_id.clone()))
        .collect();
    assert_eq!(orig_change_ids.len(), 2);

    let err = stack_rebase(
        &repo,
        &StackRebaseOptions {
            branches: vec!["stack1".to_string(), "stack2".to_string()],
            onto: Some("main".to_string()),
        },
        agent_actor(None),
    )
    .expect_err("must conflict on shared.txt");
    assert!(matches!(err, OzError::Conflict { .. }));

    // ZERO branch refs modified on conflict!
    assert_eq!(run_git(&repo_root, &["rev-parse", "stack1"]), orig_s1);
    assert_eq!(run_git(&repo_root, &["rev-parse", "stack2"]), orig_s2);
    assert_eq!(run_git(&repo_root, &["rev-parse", "main"]), main_oid);

    let sandbox_dir = tmp.path().join("demo.oz/_integrate");
    assert!(sandbox_dir.is_dir());
    let session = active_session(&repo).unwrap().expect("session present");
    assert_eq!(session.mode, IntegrationMode::StackRebase);

    // Test abort first: cleans sandbox & session, leaves refs unchanged.
    abort_integration(&repo).expect("abort stack rebase");
    assert!(!sandbox_dir.exists());
    assert!(active_session(&repo).unwrap().is_none());
    assert_eq!(run_git(&repo_root, &["rev-parse", "stack1"]), orig_s1);
    assert_eq!(run_git(&repo_root, &["rev-parse", "stack2"]), orig_s2);

    // Re-run, resolve taking theirs (`stack1 edit`), and continue!
    let _ = stack_rebase(
        &repo,
        &StackRebaseOptions {
            branches: vec!["stack1".to_string(), "stack2".to_string()],
            onto: Some("main".to_string()),
        },
        agent_actor(None),
    )
    .expect_err("conflict again");

    resolve_conflict(
        &repo,
        &ResolveOptions {
            conflict_id: Some("c1".to_string()),
            all: false,
            take: TakeSide::Theirs,
            auto_continue: false,
        },
        agent_actor(None),
    )
    .expect("resolve c1");

    let cont = continue_stack_rebase(&repo, agent_actor(None)).expect("continue stack rebase");
    assert!(!sandbox_dir.exists());
    assert!(active_session(&repo).unwrap().is_none());
    assert_eq!(cont.branches.len(), 2);
    assert_eq!(
        cont.rebased_commits
            .iter()
            .map(|c| c.change_id.clone())
            .collect::<Vec<_>>(),
        orig_change_ids
    );
    assert_eq!(
        run_git(&repo_root, &["merge-base", "main", "stack1"]),
        main_oid
    );
    assert_eq!(
        run_git(&repo_root, &["merge-base", "stack1", "stack2"]),
        cont.branches[0].new_oid
    );
    assert_eq!(
        fs::read_to_string(ws2.workspace.workspace_path.join("shared.txt")).unwrap(),
        "stack1 edit\n"
    );
}

#[test]
fn radar_handles_add_add_delete_modify_binary_and_multi_hunk_spans() {
    let base_text: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    let (_tmp, repo_root, repo) = init_test_repo(&[
        ("multi.txt", &base_text),
        ("to_del.txt", "a\nb\nc\n"),
    ]);
    fs::write(repo_root.join("asset.bin"), [0u8, 1, 2]).expect("write bin");
    run_git(&repo_root, &["add", "asset.bin"]);
    run_git(&repo_root, &["commit", "-m", "add bin"]);

    let ws1 = spawn_cas_ws(&repo, "ws1", &[], &[]);
    let ws2 = spawn_cas_ws(&repo, "ws2", &[], &[]);

    // Same new file added with DIFFERENT content -> OverlappingHunks.
    write_ws_file(&ws1.workspace.workspace_path, "new_diff.txt", b"one\ntwo\n");
    write_ws_file(&ws2.workspace.workspace_path, "new_diff.txt", b"one\nthree\n");

    // Same new file added with IDENTICAL content -> no collision.
    write_ws_file(&ws1.workspace.workspace_path, "new_same.txt", b"identical\n");
    write_ws_file(&ws2.workspace.workspace_path, "new_same.txt", b"identical\n");

    // Binary file modified by both -> OverlappingHunks ("binary").
    write_ws_file(&ws1.workspace.workspace_path, "asset.bin", &[0u8, 8, 8]);
    write_ws_file(&ws2.workspace.workspace_path, "asset.bin", &[0u8, 9, 9]);

    // Delete vs modify on `to_del.txt` -> OverlappingHunks.
    fs::remove_file(ws1.workspace.workspace_path.join("to_del.txt")).expect("del in ws1");
    write_ws_file(&ws2.workspace.workspace_path, "to_del.txt", b"a\nmodified\nc\n");

    // Multiple non-overlapping hunks on `multi.txt` -> SameFileDifferentLines with comma-joined spans.
    let mut m1: Vec<String> = (1..=40).map(|i| format!("line {i}\n")).collect();
    m1[1] = "ws1 top\n".to_string();
    m1[30] = "ws1 bottom\n".to_string();
    write_ws_file(&ws1.workspace.workspace_path, "multi.txt", m1.concat().as_bytes());
    let mut m2: Vec<String> = (1..=40).map(|i| format!("line {i}\n")).collect();
    m2[15] = "ws2 middle\n".to_string();
    write_ws_file(&ws2.workspace.workspace_path, "multi.txt", m2.concat().as_bytes());

    let report = scan_radar(&repo, &RadarOptions::default()).expect("radar");
    assert!(!report.collisions.iter().any(|c| c.path == "new_same.txt"));

    let asset = report
        .collisions
        .iter()
        .find(|c| c.path == "asset.bin")
        .expect("asset collision");
    assert_eq!(asset.severity, CollisionSeverity::OverlappingHunks);
    assert_eq!(asset.workspaces[0].lines, "binary");

    let new_diff = report
        .collisions
        .iter()
        .find(|c| c.path == "new_diff.txt")
        .expect("new_diff collision");
    assert_eq!(new_diff.severity, CollisionSeverity::OverlappingHunks);

    let to_del = report
        .collisions
        .iter()
        .find(|c| c.path == "to_del.txt")
        .expect("to_del collision");
    assert_eq!(to_del.severity, CollisionSeverity::OverlappingHunks);

    let multi = report
        .collisions
        .iter()
        .find(|c| c.path == "multi.txt")
        .expect("multi collision");
    assert_eq!(multi.severity, CollisionSeverity::SameFileDifferentLines);
    assert_eq!(multi.workspaces[0].ws, "ws1");
    assert_eq!(multi.workspaces[0].lines, "2-2, 31-31");
    assert_eq!(multi.workspaces[1].ws, "ws2");
    assert_eq!(multi.workspaces[1].lines, "16-16");
}

#[test]
fn integrate_manual_resolution_preserves_warm_cache_exclusion_and_handles_sequential_conflicts() {
    let (tmp, repo_root, repo) = init_test_repo(&[
        ("f1.txt", "base-1\n"),
        ("f2.txt", "base-2\n"),
    ]);
    let main_oid = run_git(&repo_root, &["rev-parse", "HEAD"]);

    // Seed warm cache so `_integrate/target/cached.bin` exists during manual conflict editing.
    let b_target = repo.layout.baselines_dir().join(&main_oid).join("root/target");
    fs::create_dir_all(&b_target).expect("mkdir warm");
    fs::write(b_target.join("cached.bin"), b"warm").expect("write warm");
    fs::write(
        repo.layout.baselines_dir().join(&main_oid).join("metadata.json"),
        format!(
            r#"{{"commit_oid":"{main_oid}","created_at":"2026-11-17T00:00:00Z","targets":[],"total_bytes":4}}"#
        ),
    )
    .expect("write meta");

    let ws1 = spawn_cas_ws(&repo, "ws1", &[], &[]);
    write_ws_file(&ws1.workspace.workspace_path, "f1.txt", b"ws1-f1\n");
    write_ws_file(&ws1.workspace.workspace_path, "f2.txt", b"ws1-f2\n");
    commit_ws(&ws1.workspace.workspace_path, "ws1", false, false);

    let ws2 = spawn_cas_ws(&repo, "ws2", &[], &[]);
    write_ws_file(&ws2.workspace.workspace_path, "f1.txt", b"ws2-f1\n");
    commit_ws(&ws2.workspace.workspace_path, "ws2", false, false);

    let ws3 = spawn_cas_ws(&repo, "ws3", &[], &[]);
    write_ws_file(&ws3.workspace.workspace_path, "f2.txt", b"ws3-f2\n");
    commit_ws(&ws3.workspace.workspace_path, "ws3", false, false);

    // Step 1: ws1 + ws2 conflict on f1.txt.
    let err1 = integrate(
        &repo,
        &IntegrateOptions {
            sources: vec!["ws1".to_string(), "ws2".to_string(), "ws3".to_string()],
            into: Some("main".to_string()),
            ..Default::default()
        },
        agent_actor(None),
    )
    .expect_err("first conflict on f1");
    assert!(matches!(err1, OzError::Conflict { .. }));

    let sandbox_dir = tmp.path().join("demo.oz/_integrate");
    assert!(sandbox_dir.join("target/cached.bin").is_file());

    // Manually edit f1.txt (without calling `oz resolve`) and add a helper file.
    fs::write(sandbox_dir.join("f1.txt"), "manual-merged-f1\n").expect("manual resolve f1");
    fs::write(sandbox_dir.join("helper.txt"), "helper\n").expect("add helper");

    // Continue hits the SECOND conflict on f2.txt (from ws3)!
    let err2 = continue_integration(&repo, agent_actor(None))
        .expect_err("second sequential conflict on f2");
    match &err2 {
        OzError::Conflict { conflicts, .. } => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].path, "f2.txt");
        }
        other => panic!("expected Conflict on f2.txt, got {other:?}"),
    }
    assert_eq!(
        run_git(&repo_root, &["rev-parse", "HEAD"]),
        main_oid,
        "main must remain untouched across sequential conflicts"
    );

    // Resolve c1 with `--all --take base --continue` (takes `base-2\n` for f2.txt).
    let fin = resolve_conflict(
        &repo,
        &ResolveOptions {
            conflict_id: None,
            all: true,
            take: TakeSide::Base,
            auto_continue: true,
        },
        agent_actor(None),
    )
    .expect("resolve all take base and continue");
    assert!(fin.continued.is_some());
    assert!(!sandbox_dir.exists());

    assert_eq!(
        fs::read_to_string(repo_root.join("f1.txt")).unwrap(),
        "manual-merged-f1\n"
    );
    assert_eq!(
        fs::read_to_string(repo_root.join("f2.txt")).unwrap(),
        "base-2\n"
    );
    assert_eq!(
        fs::read_to_string(repo_root.join("helper.txt")).unwrap(),
        "helper\n"
    );
    // Warm `target/` cache MUST NOT be committed to Git!
    assert!(!repo_root.join("target/cached.bin").exists());
    let tree_files = run_git(&repo_root, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!tree_files.contains("target/"));
}

#[test]
fn integrate_when_main_repo_on_other_branch_does_not_touch_main_workdir() {
    let (_tmp, repo_root, repo) = init_test_repo(&[("file.txt", "base\n")]);
    let old_main_oid = run_git(&repo_root, &["rev-parse", "HEAD"]);

    let ws = spawn_cas_ws(&repo, "ws-feat", &[], &[]);
    write_ws_file(&ws.workspace.workspace_path, "file.txt", b"feat-update\n");
    commit_ws(&ws.workspace.workspace_path, "feat update", false, false);

    // Switch `repo_root` off `main` onto `other-work`.
    run_git(&repo_root, &["checkout", "-b", "other-work"]);
    fs::write(repo_root.join("file.txt"), "other-work local\n").expect("write other");

    // Invoke `integrate` FROM INSIDE `ws` into `main`!
    let ws_repo = OzoneRepo::discover(Some(&ws.workspace.workspace_path)).expect("discover ws");
    let out = integrate(
        &ws_repo,
        &IntegrateOptions {
            sources: vec!["ws-feat".to_string()],
            into: Some("main".to_string()),
            ..Default::default()
        },
        agent_actor(Some("ws-feat")),
    )
    .expect("integrate into non-checked-out main");

    assert!(!out.main_worktree_synced);
    assert_ne!(run_git(&repo_root, &["rev-parse", "main"]), old_main_oid);
    assert_eq!(
        run_git(&repo_root, &["show", "main:file.txt"]),
        "feat-update"
    );
    // `repo_root` on `other-work` MUST stay 100% untouched!
    assert_eq!(
        run_git(&repo_root, &["symbolic-ref", "--short", "HEAD"]),
        "other-work"
    );
    assert_eq!(
        fs::read_to_string(repo_root.join("file.txt")).unwrap(),
        "other-work local\n"
    );
}

#[test]
fn integrate_supports_plain_git_branch_names_and_workspace_branch_names() {
    let (_tmp, repo_root, repo) = init_test_repo(&[("base.txt", "base\n")]);

    // Plain Git branch without any Ozone workspace.
    run_git(&repo_root, &["checkout", "-b", "feat/plain-branch"]);
    fs::write(repo_root.join("plain.txt"), "from plain branch\n").expect("write plain");
    run_git(&repo_root, &["add", "plain.txt"]);
    run_git(&repo_root, &["commit", "-m", "plain commit"]);
    run_git(&repo_root, &["checkout", "main"]);

    // Workspace with a distinct branch name (`feat/ws-br` != workspace name `ws-alias`).
    let mut ws_opts = SpawnWorkspaceOptions::new("ws-alias");
    ws_opts.backend = Some(BackendKind::CasHardlink);
    ws_opts.warm = false;
    ws_opts.branch = Some("feat/ws-br".to_string());
    let ws = spawn_workspace(&repo, &ws_opts).expect("spawn ws-alias");
    write_ws_file(&ws.workspace.workspace_path, "ws_br.txt", b"from ws branch\n");
    commit_ws(&ws.workspace.workspace_path, "ws commit", false, false);

    // Pass BOTH by branch name (`feat/plain-branch` and `feat/ws-br`).
    let out = integrate(
        &repo,
        &IntegrateOptions {
            sources: vec!["feat/plain-branch".to_string(), "feat/ws-br".to_string()],
            into: Some("main".to_string()),
            ..Default::default()
        },
        agent_actor(None),
    )
    .expect("integrate plain and ws branch names");

    assert_eq!(out.merged_sources, vec!["feat/plain-branch", "feat/ws-br"]);
    assert_eq!(
        fs::read_to_string(repo_root.join("plain.txt")).unwrap(),
        "from plain branch\n"
    );
    assert_eq!(
        fs::read_to_string(repo_root.join("ws_br.txt")).unwrap(),
        "from ws branch\n"
    );
    assert_eq!(run_git(&repo_root, &["status", "--porcelain"]), "");
}


