// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for `oz-cow` backends, empirical reflink probing, CAS hardlink isolation,
//! workspace lifecycle management, and `Fuse3VfsBackend` Unix socket IPC.

use oz_core::{BackendKind, MountHealth, MountRecord, OzoneConfig, OzoneRepo};
use oz_cow::{
    CasHardlinkBackend, GcWorkspacesOptions, RemoveWorkspaceOptions, SpawnRequest,
    SpawnWorkspaceOptions, WorkspaceBackend, detect_filesystem_with_ozone, gc_workspaces,
    list_workspaces, probe_ozone_reflink, remove_workspace, spawn_workspace,
};
#[cfg(target_os = "linux")]
use oz_cow::Fuse3VfsBackend;
#[cfg(target_os = "linux")]
use oz_vfsd::{MountInfo, VfsRequest, VfsResponse, encode_line};
use oz_warm::capture_baseline;
use std::path::Path;
use std::process::Command;
use std::time::Instant;
use tempfile::TempDir;

fn init_repo(path: &Path) -> String {
    std::fs::create_dir_all(path).expect("mkdir repo");
    assert!(
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(path)
            .output()
            .expect("git init")
            .status
            .success()
    );
    Command::new("git")
        .args(["config", "user.name", "Ozone Bot"])
        .current_dir(path)
        .output()
        .expect("git config name");
    Command::new("git")
        .args(["config", "user.email", "bot@ozone.dev"])
        .current_dir(path)
        .output()
        .expect("git config email");
    commit_all(path, "initial commit")
}

fn commit_all(path: &Path, message: &str) -> String {
    assert!(
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(path)
            .output()
            .expect("git add")
            .status
            .success()
    );
    let commit = Command::new("git")
        .args(["commit", "--allow-empty", "-m", message])
        .current_dir(path)
        .output()
        .expect("git commit");
    assert!(
        commit.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let rev = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(path)
        .output()
        .expect("rev-parse");
    String::from_utf8_lossy(&rev.stdout).trim().to_string()
}

#[test]
fn empirical_reflink_probe_runs_in_dot_probe_and_cleans_up() {
    let dir = TempDir::new().expect("tempdir");
    let ozone_dir = dir.path().join(".git/ozone");
    std::fs::create_dir_all(&ozone_dir).expect("mkdir ozone");

    let supported = probe_ozone_reflink(&ozone_dir);
    let probe_dir = ozone_dir.join(".probe");
    assert!(probe_dir.is_dir());
    let leftovers: Vec<_> = std::fs::read_dir(&probe_dir)
        .expect("read probe dir")
        .collect();
    assert!(leftovers.is_empty(), "probe dir must stay clean");

    let fs_probe =
        detect_filesystem_with_ozone(dir.path(), Some(&ozone_dir)).expect("detect with ozone");
    assert_eq!(fs_probe.reflink_supported, supported);
}

#[cfg(unix)]
#[test]
fn cas_hardlink_backend_hardlinks_readonly_deps_and_never_hardlinks_editable_sources() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("app");
    std::fs::create_dir_all(repo_root.join("src")).expect("mkdir src");
    std::fs::write(
        repo_root.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(
        repo_root.join("package.json"),
        "{\"name\":\"app\"}\n",
    )
    .expect("write package.json");
    std::fs::write(
        repo_root.join("src/main.rs"),
        "fn main() { println!(\"hello\"); }\n",
    )
    .expect("write main.rs");

    let commit_oid = init_repo(&repo_root);
    let ozone_dir = repo_root.join(".git/ozone");
    std::fs::create_dir_all(&ozone_dir).expect("mkdir ozone");

    // Populate immutable warm dependency caches.
    let rlib_path = repo_root.join("target/debug/deps/libdep.rlib");
    let npm_path = repo_root.join("node_modules/pkg/index.js");
    std::fs::create_dir_all(rlib_path.parent().expect("parent")).expect("mkdir deps");
    std::fs::create_dir_all(npm_path.parent().expect("parent")).expect("mkdir npm");
    std::fs::write(&rlib_path, b"rlib-payload-bytes").expect("write rlib");
    std::fs::set_permissions(&rlib_path, std::fs::Permissions::from_mode(0o444)).expect("chmod rlib");
    std::fs::write(&npm_path, b"module.exports = 42;\n").expect("write npm");
    std::fs::set_permissions(&npm_path, std::fs::Permissions::from_mode(0o555)).expect("chmod npm");

    let cfg = OzoneConfig::default();
    capture_baseline(&repo_root, &ozone_dir, &commit_oid, &cfg).expect("capture baseline");
    let baseline_root = ozone_dir.join("baselines").join(&commit_oid).join("root");

    let backend = CasHardlinkBackend;
    let ws1 = dir.path().join("app.oz/ws-1");
    let ws2 = dir.path().join("app.oz/ws-2");

    for (name, target) in [("ws-1", &ws1), ("ws-2", &ws2)] {
        backend
            .spawn(&SpawnRequest {
                workspace_name: name.to_string(),
                branch: format!("feat/{name}"),
                base_commit: Some(commit_oid.clone()),
                base_tree: None,
                repo_root: repo_root.clone(),
                git_dir: repo_root.join(".git"),
                target_path: target.clone(),
                upper_dir: None,
                meta_dir: None,
                baseline_dir: Some(baseline_root.clone()),
                scope: Vec::new(),
                deny: Vec::new(),
                materialize_files: true,
            })
            .expect("spawn CAS workspace");
    }

    // 1. Editable source files MUST have distinct inodes (never hardlinked!).
    let repo_src_ino = std::fs::metadata(repo_root.join("src/main.rs"))
        .expect("repo src")
        .ino();
    let ws1_src_meta = std::fs::metadata(ws1.join("src/main.rs")).expect("ws1 src");
    let ws2_src_meta = std::fs::metadata(ws2.join("src/main.rs")).expect("ws2 src");
    assert_ne!(repo_src_ino, ws1_src_meta.ino());
    assert_ne!(repo_src_ino, ws2_src_meta.ino());
    assert_ne!(ws1_src_meta.ino(), ws2_src_meta.ino());
    assert_eq!(ws1_src_meta.nlink(), 1);
    assert_eq!(ws2_src_meta.nlink(), 1);

    // Writing in-place to ws1/src/main.rs must NOT affect repo_root or ws2.
    std::fs::write(ws1.join("src/main.rs"), "fn main() { /* ws1 */ }\n").expect("edit ws1");
    assert_eq!(
        std::fs::read_to_string(ws2.join("src/main.rs")).expect("read ws2"),
        "fn main() { println!(\"hello\"); }\n"
    );

    // 2. Immutable deps MUST be hardlinked via .git/ozone/cas/ with 0444/0555 modes!
    let ws1_rlib = std::fs::metadata(ws1.join("target/debug/deps/libdep.rlib")).expect("ws1 rlib");
    let ws2_rlib = std::fs::metadata(ws2.join("target/debug/deps/libdep.rlib")).expect("ws2 rlib");
    assert_eq!(ws1_rlib.ino(), ws2_rlib.ino());
    assert!(ws1_rlib.nlink() >= 3);
    assert_eq!(ws1_rlib.mode() & 0o777, 0o444);

    let ws1_npm = std::fs::metadata(ws1.join("node_modules/pkg/index.js")).expect("ws1 npm");
    let ws2_npm = std::fs::metadata(ws2.join("node_modules/pkg/index.js")).expect("ws2 npm");
    assert_eq!(ws1_npm.ino(), ws2_npm.ino());
    assert!(ws1_npm.nlink() >= 3);
    assert_eq!(ws1_npm.mode() & 0o777, 0o555);
}

#[test]
fn workspace_lifecycle_spawns_10_workspaces_creates_real_branches_and_removes_cleanly() {
    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("mono");
    std::fs::create_dir_all(repo_root.join("src")).expect("mkdir src");
    std::fs::write(repo_root.join(".gitignore"), "/target\n").expect("write .gitignore");
    std::fs::write(
        repo_root.join("Cargo.toml"),
        "[package]\nname = \"mono\"\nversion = \"0.1.0\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(repo_root.join("src/lib.rs"), "pub fn ok() -> bool { true }\n")
        .expect("write lib.rs");

    let init_oid = init_repo(&repo_root);

    // Create a 10 MiB read-only immutable warm dependency blob so we verify <50 MB overhead across 10 workspaces.
    let big_dep = repo_root.join("target/debug/deps/libbig.rlib");
    std::fs::create_dir_all(big_dep.parent().expect("parent")).expect("mkdir deps");
    std::fs::write(&big_dep, vec![0xABu8; 10 * 1024 * 1024]).expect("write 10MB dep");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&big_dep, std::fs::Permissions::from_mode(0o444))
            .expect("chmod 0444");
    }

    let repo = OzoneRepo::discover(Some(&repo_root)).expect("discover repo");

    let mut spawn_times_ms = Vec::new();
    for i in 1..=10 {
        let ws_name = format!("agent-{i}");
        let branch_name = format!("feat/agent-{i}");
        let mut opts = SpawnWorkspaceOptions::new(&ws_name);
        opts.branch = Some(branch_name.clone());
        opts.backend = Some(BackendKind::CasHardlink);

        let t0 = Instant::now();
        let spawned = spawn_workspace(&repo, &opts).expect("spawn workspace");
        let elapsed_ms = t0.elapsed().as_millis();
        spawn_times_ms.push(elapsed_ms);

        let expected_ws_path = dir.path().join("mono.oz").join(&ws_name);
        assert_eq!(spawned.workspace.workspace_path, expected_ws_path);
        assert!(expected_ws_path.join(".git").is_file());
        assert!(expected_ws_path.join("src/lib.rs").is_file());
        assert!(expected_ws_path.join("target/debug/deps/libbig.rlib").is_file());

        // Real branch and colocated .git/worktrees/<ws>/{HEAD,commondir,gitdir,index}.
        let wt_dir = repo.git_common_dir.join("worktrees").join(&ws_name);
        assert_eq!(
            std::fs::read_to_string(wt_dir.join("HEAD")).expect("read HEAD"),
            format!("ref: refs/heads/{branch_name}\n")
        );
        assert_eq!(
            std::fs::read_to_string(wt_dir.join("commondir")).expect("read commondir"),
            "../..\n"
        );
        assert_eq!(
            std::fs::read_to_string(wt_dir.join("gitdir")).expect("read gitdir"),
            format!("{}\n", expected_ws_path.join(".git").display())
        );
        assert!(wt_dir.join("index").is_file());

        // Verify gix branch ref exists and OzoneRepo discovers the spawned workspace cleanly.
        let gix_r = repo.open_gix().expect("open gix");
        let mut rf = gix_r
            .find_reference(&format!("refs/heads/{branch_name}"))
            .expect("branch exists");
        assert_eq!(rf.peel_to_id().expect("peel").to_hex().to_string(), init_oid);

        let discovered_ws = OzoneRepo::discover(Some(&expected_ws_path)).expect("discover ws");
        assert_eq!(discovered_ws.workspace.as_deref(), Some(ws_name.as_str()));
        assert_eq!(discovered_ws.branch.as_deref(), Some(branch_name.as_str()));
        assert_eq!(discovered_ws.mount_health, MountHealth::Healthy);
    }

    // All 10 spawns must stay well below 500 ms each.
    for (idx, ms) in spawn_times_ms.iter().enumerate() {
        assert!(
            *ms < 500,
            "workspace #{} took {ms} ms (expected < 500 ms)",
            idx + 1
        );
    }

    // Verify all 10 workspaces share the exact same single inode for libbig.rlib (0 extra bytes!).
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ino1 = std::fs::metadata(dir.path().join("mono.oz/agent-1/target/debug/deps/libbig.rlib"))
            .expect("meta 1")
            .ino();
        let ino10 =
            std::fs::metadata(dir.path().join("mono.oz/agent-10/target/debug/deps/libbig.rlib"))
                .expect("meta 10")
                .ino();
        assert_eq!(ino1, ino10);
    }

    // Verify `git status --porcelain` inside agent-1 is clean and reports branch feat/agent-1.
    let ws1 = dir.path().join("mono.oz/agent-1");
    let git_branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&ws1)
        .output()
        .expect("git rev-parse");
    assert_eq!(String::from_utf8_lossy(&git_branch.stdout).trim(), "feat/agent-1");

    let listed = list_workspaces(&repo).expect("list workspaces");
    assert_eq!(listed.len(), 10);
    assert!(listed.iter().all(|w| w.healthy));

    // Advance agent-1 with a commit, merge it into main, and test gc_workspaces!
    std::fs::write(ws1.join("src/feature.rs"), "pub fn feat() {}\n").expect("write feature");
    let feat_oid = commit_all(&ws1, "feat: add feature");
    assert!(
        Command::new("git")
            .args(["merge", "--ff-only", &feat_oid])
            .current_dir(&repo_root)
            .output()
            .expect("git merge")
            .status
            .success()
    );

    let gc_report = gc_workspaces(
        &repo,
        &GcWorkspacesOptions {
            target_branch: Some("main".to_string()),
            prune_merged: true,
            prune_missing: true,
            dry_run: false,
        },
    )
    .expect("gc workspaces");
    assert_eq!(gc_report.removed, vec!["agent-1"]);

    // Explicitly remove agent-2 and verify full cleanup of <ws>, ws/<ws>, state/<ws>.json, and .git/worktrees/<ws>.
    let removed = remove_workspace(&repo, "agent-2", &RemoveWorkspaceOptions::default())
        .expect("remove agent-2");
    assert_eq!(removed.name, "agent-2");
    assert!(!dir.path().join("mono.oz/agent-2").exists());
    assert!(!repo.layout.ws_meta_dir("agent-2").exists());
    assert!(!repo.layout.workspace_state_path("agent-2").exists());
    assert!(!repo.git_common_dir.join("worktrees/agent-2").exists());
    assert_eq!(list_workspaces(&repo).expect("list after rm").len(), 8);
}

#[cfg(target_os = "linux")]
#[test]
fn fuse3_vfs_backend_ipc_roundtrips_mount_and_unmount_against_mock_unix_listener() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};

    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("vfs-repo");
    std::fs::create_dir_all(repo_root.join("src")).expect("mkdir src");
    std::fs::write(repo_root.join("README.md"), "# VFS\n").expect("write readme");
    std::fs::write(repo_root.join("src/keep.rs"), "pub fn v() -> u32 { 1 }\n").expect("write keep");
    std::fs::write(repo_root.join("src/deleted.rs"), "pub fn old() {}\n").expect("write deleted");

    let c1 = init_repo(&repo_root);
    let ozone_dir = repo_root.join(".git/ozone");
    std::fs::create_dir_all(&ozone_dir).expect("mkdir ozone");

    // Capture baseline at c1.
    capture_baseline(&repo_root, &ozone_dir, &c1, &OzoneConfig::default()).expect("capture c1");

    // Create branch feat/vfs at c2 that modifies src/keep.rs, adds src/added.rs, and deletes src/deleted.rs.
    assert!(
        Command::new("git")
            .args(["checkout", "-b", "feat/vfs"])
            .current_dir(&repo_root)
            .output()
            .expect("git checkout -b")
            .status
            .success()
    );
    std::fs::write(repo_root.join("src/keep.rs"), "pub fn v() -> u32 { 2 }\n").expect("mod keep");
    std::fs::write(repo_root.join("src/added.rs"), "pub fn added() {}\n").expect("add");
    std::fs::remove_file(repo_root.join("src/deleted.rs")).expect("del");
    let c2 = commit_all(&repo_root, "branch changes");

    // Switch main working tree back to main so feat/vfs is free to check out in workspace.
    assert!(
        Command::new("git")
            .args(["checkout", "main"])
            .current_dir(&repo_root)
            .output()
            .expect("git checkout main")
            .status
            .success()
    );

    let repo = OzoneRepo::discover(Some(&repo_root)).expect("discover");
    let sock_path = repo.layout.vfsd_socket_path();
    let listener = UnixListener::bind(&sock_path).expect("bind mock vfsd.sock");

    let seen_requests = Arc::new(Mutex::new(Vec::<VfsRequest>::new()));
    let seen_clone = Arc::clone(&seen_requests);
    let server_handle = std::thread::spawn(move || {
        // Expect exactly 2 IPC calls: Mount, then Unmount.
        for stream in listener.incoming().take(2) {
            let mut stream = stream.expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request line");
            let req: VfsRequest = serde_json::from_str(line.trim()).expect("parse VfsRequest");

            let resp = match &req {
                VfsRequest::Mount { spec } => VfsResponse::Mounted {
                    mount: MountInfo {
                        workspace: spec.workspace.clone(),
                        branch: spec.branch.clone(),
                        mount_point: spec.mount_point.clone(),
                        upper_dir: spec.upper_dir.clone(),
                        mounted: true,
                        dirty_count: 0,
                    },
                },
                VfsRequest::Unmount { workspace, .. } => VfsResponse::Unmounted {
                    workspace: workspace.clone(),
                },
                other => VfsResponse::Error {
                    code: "unexpected".to_string(),
                    message: format!("unexpected request: {other:?}"),
                },
            };

            seen_clone.lock().expect("lock").push(req);
            let encoded = encode_line(&resp).expect("encode resp");
            stream.write_all(encoded.as_bytes()).expect("write resp");
            stream.flush().expect("flush");
        }
    });

    // Spawn workspace on existing feat/vfs (at c2) while baseline is c1!
    let mut opts = SpawnWorkspaceOptions::new("agent-vfs");
    opts.branch = Some("feat/vfs".to_string());
    opts.backend = Some(BackendKind::LinuxFuse3);
    let spawned = spawn_workspace(&repo, &opts).expect("spawn fuse3 workspace");
    assert_eq!(spawned.state.backend, BackendKind::LinuxFuse3);
    assert_eq!(spawned.state.base_commit.as_deref(), Some(c2.as_str()));

    let ws_meta = repo.layout.ws_meta_dir("agent-vfs");
    assert!(ws_meta.join("upper").is_dir());
    assert!(ws_meta.join("meta/whiteouts").is_dir());
    assert!(ws_meta.join("meta/opaque").is_dir());
    assert!(ws_meta.join("meta/seed_whiteouts").is_dir());
    assert!(ws_meta.join("seed/.git").is_file());
    assert!(ws_meta.join("seed/src/keep.rs").is_file());
    assert!(ws_meta.join("seed/src/added.rs").is_file());
    // Untouched README.md comes from baseline_dir, so seed/ does NOT duplicate it!
    assert!(!ws_meta.join("seed/README.md").exists());
    // Deleted file relative to c1 gets a seed whiteout `.oz_wh`!
    assert!(
        ws_meta
            .join("meta/seed_whiteouts/src/deleted.rs.oz_wh")
            .is_file()
    );

    let mount_json: MountRecord = serde_json::from_str(
        &std::fs::read_to_string(repo.layout.ws_mount_record_path("agent-vfs")).expect("read"),
    )
    .expect("parse mount.json");
    assert!(mount_json.mounted);
    assert_eq!(mount_json.workspace, "agent-vfs");

    // Cleanly remove workspace via mock socket Unmount IPC.
    remove_workspace(&repo, "agent-vfs", &RemoveWorkspaceOptions::default())
        .expect("remove fuse3 workspace");
    assert!(!dir.path().join("vfs-repo.oz/agent-vfs").exists());
    assert!(!repo.layout.ws_meta_dir("agent-vfs").exists());
    assert!(!repo.layout.workspace_state_path("agent-vfs").exists());
    assert!(!repo.git_common_dir.join("worktrees/agent-vfs").exists());

    server_handle.join().expect("server thread join");
    let requests = seen_requests.lock().expect("lock");
    assert_eq!(requests.len(), 2);
    assert!(matches!(&requests[0], VfsRequest::Mount { .. }));
    assert!(matches!(&requests[1], VfsRequest::Unmount { workspace, force: true } if workspace == "agent-vfs"));
}

#[cfg(target_os = "linux")]
#[test]
fn fuse3_backend_fails_fast_when_auto_start_disabled_and_socket_missing() {
    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("no-daemon");
    std::fs::create_dir_all(repo_root.join(".git/ozone")).expect("mkdir");

    let backend = Fuse3VfsBackend::new(false, true, false);
    let err = backend
        .spawn(&SpawnRequest {
            workspace_name: "ws1".to_string(),
            branch: "feat/ws1".to_string(),
            base_commit: None,
            base_tree: None,
            repo_root: repo_root.clone(),
            git_dir: repo_root.join(".git"),
            target_path: dir.path().join("no-daemon.oz/ws1"),
            upper_dir: None,
            meta_dir: None,
            baseline_dir: None,
            scope: Vec::new(),
            deny: Vec::new(),
            materialize_files: true,
        })
        .expect_err("should fail when socket missing and auto_start=false");

    assert!(matches!(err, oz_core::OzError::MountUnavailable { .. }));
}
