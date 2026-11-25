// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use oz_core::{BackendKind, OzError, OzoneRepo, compute_status};
use oz_cow::{SpawnWorkspaceOptions, spawn_workspace};
use oz_warm::resolve_or_capture_baseline;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn run_git_isolated(dir: &Path, home: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed:\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn spawn_workspace_smudges_git_lfs_and_creates_gitlink_dirs_across_cas_and_baseline() {
    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();

    // Seed submodule repo
    let sub_src = tmp.path().join("sub_repo");
    fs::create_dir_all(&sub_src).unwrap();
    run_git_isolated(&sub_src, &fake_home, &["init", "-b", "main"]);
    fs::write(sub_src.join("mod.txt"), "sub\n").unwrap();
    run_git_isolated(&sub_src, &fake_home, &["add", "."]);
    run_git_isolated(&sub_src, &fake_home, &["commit", "-m", "sub init"]);

    // Seed parent repo with git-lfs + submodule
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    run_git_isolated(&repo_dir, &fake_home, &["init", "-b", "main"]);
    run_git_isolated(&repo_dir, &fake_home, &["lfs", "install", "--local"]);
    run_git_isolated(&repo_dir, &fake_home, &["lfs", "track", "*.bin"]);
    run_git_isolated(
        &repo_dir,
        &fake_home,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub_src.to_str().unwrap(),
            "vendor/sub",
        ],
    );

    let lfs_payload: Vec<u8> = (0..1024u32).flat_map(|i| i.to_le_bytes()).collect();
    fs::write(repo_dir.join("weights.bin"), &lfs_payload).unwrap();
    fs::write(repo_dir.join("crlf.txt"), b"one\r\ntwo\r\n").unwrap();
    fs::write(
        repo_dir.join(".gitattributes"),
        "*.bin filter=lfs diff=lfs merge=lfs -text\ncrlf.txt text eol=crlf\n",
    )
    .unwrap();
    run_git_isolated(&repo_dir, &fake_home, &["add", "."]);
    run_git_isolated(&repo_dir, &fake_home, &["commit", "-m", "init lfs + sub"]);

    let repo = OzoneRepo::discover(Some(&repo_dir)).unwrap();

    // 1. Spawn CasHardlink workspace -> LFS smudged, eol=crlf smudged, gitlink directory present & clean!
    let ws1 = spawn_workspace(
        &repo,
        &SpawnWorkspaceOptions {
            name: "ws-cas".to_string(),
            branch: Some("feat/cas".to_string()),
            from_ref: Some("main".to_string()),
            backend: Some(BackendKind::CasHardlink),
            warm: false,
            scope: Vec::new(),
            deny: Vec::new(),
        },
    )
    .expect("spawn cas workspace");

    assert!(ws1.workspace.workspace_path.join("vendor/sub").is_dir());
    assert_eq!(
        fs::read(ws1.workspace.workspace_path.join("weights.bin")).unwrap(),
        lfs_payload
    );
    assert_eq!(
        fs::read(ws1.workspace.workspace_path.join("crlf.txt")).unwrap(),
        b"one\r\ntwo\r\n"
    );

    let ws1_oz = OzoneRepo::discover(Some(&ws1.workspace.workspace_path)).unwrap();
    let ws1_status = compute_status(&ws1_oz).unwrap();
    assert!(ws1_status.is_clean(), "{:?}", ws1_status.changes);

    // 2. Capture warm baseline (raw blobs, no LFS smudge / no gitlink dir) and spawn on top!
    let baseline = resolve_or_capture_baseline(
        &repo_dir,
        &repo.layout.ozone_dir,
        repo.head_oid.as_deref().unwrap(),
        &repo.config,
    )
    .expect("capture baseline");
    assert!(baseline.is_some());

    let ws2 = spawn_workspace(
        &repo,
        &SpawnWorkspaceOptions {
            name: "ws-warm".to_string(),
            branch: Some("feat/warm".to_string()),
            from_ref: Some("main".to_string()),
            backend: Some(BackendKind::CasHardlink),
            warm: true,
            scope: Vec::new(),
            deny: Vec::new(),
        },
    )
    .expect("spawn warm workspace");

    assert!(ws2.workspace.workspace_path.join("vendor/sub").is_dir());
    assert_eq!(
        fs::read(ws2.workspace.workspace_path.join("weights.bin")).unwrap(),
        lfs_payload
    );

    // 3. Unconfiguring `filter.lfs.smudge` fails safely instead of writing raw LFS pointer!
    run_git_isolated(
        &repo_dir,
        &fake_home,
        &["config", "--local", "filter.lfs.smudge", "false"],
    );
    let err = spawn_workspace(
        &repo,
        &SpawnWorkspaceOptions {
            name: "ws-fail".to_string(),
            branch: Some("feat/fail".to_string()),
            from_ref: Some("main".to_string()),
            backend: Some(BackendKind::CasHardlink),
            warm: false,
            scope: Vec::new(),
            deny: Vec::new(),
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, OzError::CheckFailed { .. }),
        "expected CheckFailed when smudge fails, got: {err:?}"
    );
    assert!(!tmp.path().join("repo.oz/ws-fail").exists());
}
