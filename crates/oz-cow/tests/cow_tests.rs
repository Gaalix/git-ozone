// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for `oz-cow` backends, empirical reflink probing, CAS hardlink isolation,
//! workspace lifecycle management, and `Fuse3VfsBackend` Unix socket IPC.

use oz_core::{BackendKind, MountHealth, OzoneConfig, OzoneRepo};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use oz_core::MountRecord;
use oz_cow::{
    CasHardlinkBackend, GcWorkspacesOptions, RemoveWorkspaceOptions, SpawnRequest,
    SpawnWorkspaceOptions, WorkspaceBackend, backend_for_kind, detect_filesystem_with_ozone,
    gc_workspaces, list_workspaces, prepare_vfs_seed_layer, probe_ozone_reflink, remove_workspace,
    spawn_workspace,
};
#[cfg(target_os = "linux")]
use oz_cow::Fuse3VfsBackend;
#[cfg(target_os = "windows")]
use oz_cow::ProjFsVfsBackend;
use oz_vfsd::{MountSpec, OverlayMountConfig, ProjFsLayerEngine};
#[cfg(any(target_os = "linux", target_os = "windows"))]
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
        let mut active_mounts: Vec<MountInfo> = Vec::new();
        for stream in listener.incoming() {
            let mut stream = stream.expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request line");
            let req: VfsRequest = serde_json::from_str(line.trim()).expect("parse VfsRequest");

            let mut done = false;
            let resp = match &req {
                VfsRequest::Ping => VfsResponse::Pong {
                    version: "0.1.0".to_string(),
                    pid: std::process::id(),
                },
                VfsRequest::ListMounts => VfsResponse::MountList {
                    mounts: active_mounts.clone(),
                },
                VfsRequest::Mount { spec } => {
                    let info = MountInfo {
                        workspace: spec.workspace.clone(),
                        branch: spec.branch.clone(),
                        mount_point: spec.mount_point.clone(),
                        upper_dir: spec.upper_dir.clone(),
                        mounted: true,
                        dirty_count: 0,
                    };
                    active_mounts.push(info.clone());
                    seen_clone.lock().expect("lock").push(req.clone());
                    VfsResponse::Mounted { mount: info }
                }
                VfsRequest::Unmount { workspace, .. } => {
                    active_mounts.retain(|m| m.workspace != *workspace);
                    seen_clone.lock().expect("lock").push(req.clone());
                    done = true;
                    VfsResponse::Unmounted {
                        workspace: workspace.clone(),
                    }
                }
                other => VfsResponse::Error {
                    code: "unexpected".to_string(),
                    message: format!("unexpected request: {other:?}"),
                },
            };

            let encoded = encode_line(&resp).expect("encode resp");
            stream.write_all(encoded.as_bytes()).expect("write resp");
            stream.flush().expect("flush");
            if done {
                break;
            }
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

#[test]
fn windows_projfs_backend_availability_and_shared_seed_layer() {
    #[cfg(not(target_os = "windows"))]
    {
        let err = match backend_for_kind(BackendKind::WindowsProjFs) {
            Ok(_) => panic!("WindowsProjFs must not be constructed on non-Windows"),
            Err(e) => e,
        };
        match err {
            oz_core::OzError::InvalidUsage { message, .. } => {
                assert_eq!(message, "windows_proj_fs backend is only available on Windows");
            }
            other => panic!("expected InvalidUsage, got {other:?}"),
        }
    }

    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("seed-repo");
    std::fs::create_dir_all(repo_root.join("src")).expect("mkdir src");
    std::fs::write(repo_root.join("README.md"), "# Base\n").expect("write readme");
    std::fs::write(repo_root.join("src/keep.rs"), "pub fn v() -> u32 { 1 }\n").expect("write keep");
    std::fs::write(repo_root.join("src/old.rs"), "pub fn old() {}\n").expect("write old");

    let c1 = init_repo(&repo_root);
    let ozone_dir = repo_root.join(".git/ozone");
    std::fs::create_dir_all(&ozone_dir).expect("mkdir ozone");
    capture_baseline(&repo_root, &ozone_dir, &c1, &OzoneConfig::default()).expect("capture c1");

    assert!(
        Command::new("git")
            .args(["checkout", "-b", "feat/projfs-seed"])
            .current_dir(&repo_root)
            .output()
            .expect("git checkout -b")
            .status
            .success()
    );
    std::fs::write(repo_root.join("src/keep.rs"), "pub fn v() -> u32 { 2 }\n").expect("mod keep");
    std::fs::write(repo_root.join("src/added.rs"), "pub fn added() {}\n").expect("add");
    std::fs::remove_file(repo_root.join("src/old.rs")).expect("del old");
    let c2 = commit_all(&repo_root, "branch changes");
    let tree_out = Command::new("git")
        .args(["rev-parse", &format!("{c2}^{{tree}}")])
        .current_dir(&repo_root)
        .output()
        .expect("rev-parse tree");
    let target_tree = String::from_utf8_lossy(&tree_out.stdout).trim().to_string();

    let git_dir = repo_root.join(".git");
    let ws_meta = git_dir.join("ozone/ws/ws-shared");
    let upper_dir = ws_meta.join("upper");
    let seed_dir = ws_meta.join("seed");
    let meta_dir = ws_meta.join("meta");
    let seed_whiteouts_dir = meta_dir.join("seed_whiteouts");
    let baseline_dir = ozone_dir.join("baselines").join(&c1).join("root");
    let mount_point = dir.path().join("seed-repo.oz/ws-shared");

    std::fs::create_dir_all(&upper_dir).expect("mkdir upper");
    std::fs::create_dir_all(&seed_dir).expect("mkdir seed");
    std::fs::create_dir_all(&seed_whiteouts_dir).expect("mkdir swh");
    std::fs::create_dir_all(meta_dir.join("whiteouts")).expect("mkdir wh");
    std::fs::create_dir_all(meta_dir.join("opaque")).expect("mkdir opq");
    std::fs::create_dir_all(&mount_point).expect("mkdir mnt");
    std::fs::create_dir_all(git_dir.join("worktrees/ws-shared")).expect("mkdir wt");

    let req = SpawnRequest {
        workspace_name: "ws-shared".to_string(),
        branch: "feat/projfs-seed".to_string(),
        base_commit: Some(c2.clone()),
        base_tree: Some(target_tree.clone()),
        repo_root: repo_root.clone(),
        git_dir: git_dir.clone(),
        target_path: mount_point.clone(),
        upper_dir: Some(upper_dir.clone()),
        meta_dir: Some(meta_dir.clone()),
        baseline_dir: Some(baseline_dir.clone()),
        scope: Vec::new(),
        deny: Vec::new(),
        materialize_files: true,
    };

    prepare_vfs_seed_layer(&req, &git_dir, &seed_dir, &seed_whiteouts_dir)
        .expect("prepare_vfs_seed_layer");

    assert!(seed_dir.join(".git").is_file());
    assert!(seed_dir.join("src/keep.rs").is_file());
    assert!(seed_dir.join("src/added.rs").is_file());
    assert!(!seed_dir.join("README.md").exists());
    assert!(seed_whiteouts_dir.join("src/old.rs.oz_wh").is_file());

    let spec = MountSpec {
        workspace: "ws-shared".to_string(),
        branch: "feat/projfs-seed".to_string(),
        repo_root,
        git_dir,
        mount_point,
        upper_dir,
        meta_dir,
        base_tree: Some(target_tree),
        base_commit: Some(c2),
        baseline_dir: Some(baseline_dir),
        allow_other: false,
        writeback_cache: false,
    };
    let engine = ProjFsLayerEngine::new(OverlayMountConfig::from_spec(&spec));
    let src_names: Vec<String> = engine
        .enumerate_dir("src", None)
        .expect("enum src")
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(src_names, vec!["added.rs", "keep.rs"]);
    assert_eq!(
        engine
            .read_hydration_data("src/keep.rs", 0, 64)
            .expect("read keep"),
        b"pub fn v() -> u32 { 2 }\n"
    );
}

#[cfg(unix)]
#[test]
fn concurrent_workspace_spawns_reuse_index_and_cas_digest_cache_safely() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::{Arc, Barrier};

    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("par-spawn");
    std::fs::create_dir_all(repo_root.join("src")).expect("mkdir src");
    std::fs::write(repo_root.join(".gitignore"), "/target\n").expect("write .gitignore");
    std::fs::write(
        repo_root.join("Cargo.toml"),
        "[package]\nname = \"par-spawn\"\nversion = \"0.1.0\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(repo_root.join("src/lib.rs"), "pub fn ok() -> u32 { 7 }\n").expect("write lib");

    let commit_oid = init_repo(&repo_root);
    let dep = repo_root.join("target/debug/deps/libshared.rlib");
    std::fs::create_dir_all(dep.parent().expect("parent")).expect("mkdir deps");
    std::fs::write(&dep, vec![0x5Au8; 1024 * 1024]).expect("write 1MB dep");
    std::fs::set_permissions(&dep, std::fs::Permissions::from_mode(0o444)).expect("chmod");

    let repo = OzoneRepo::discover(Some(&repo_root)).expect("discover");
    let count = 8usize;
    let barrier = Arc::new(Barrier::new(count));

    let spawned_infos: Vec<_> = std::thread::scope(|s| {
        let mut handles = Vec::new();
        for i in 1..=count {
            let b = Arc::clone(&barrier);
            let r = &repo;
            handles.push(s.spawn(move || {
                let mut opts = SpawnWorkspaceOptions::new(format!("par-{i}"));
                opts.branch = Some(format!("feat/par-{i}"));
                opts.backend = Some(BackendKind::CasHardlink);
                b.wait();
                spawn_workspace(r, &opts)
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("join").expect("spawn ok"))
            .collect()
    });

    assert_eq!(spawned_infos.len(), count);
    // Digest cache and index cache must both exist cleanly.
    let cas_digest_file = repo
        .git_common_dir
        .join("ozone/cas")
        .join(format!(".digests_{commit_oid}.json"));
    assert!(cas_digest_file.is_file(), "CAS digest cache should be persisted");

    // All 8 workspaces share the exact same CAS inode for `libshared.rlib` and distinct inodes for `src/lib.rs`.
    let first_dep_ino = std::fs::metadata(
        spawned_infos[0]
            .workspace
            .workspace_path
            .join("target/debug/deps/libshared.rlib"),
    )
    .expect("dep meta")
    .ino();
    let mut src_inos = std::collections::HashSet::new();
    for info in &spawned_infos {
        let ws_path = &info.workspace.workspace_path;
        assert!(info.git_worktree_dir.join("index").is_file());
        let dep_ino = std::fs::metadata(ws_path.join("target/debug/deps/libshared.rlib"))
            .expect("dep meta")
            .ino();
        assert_eq!(dep_ino, first_dep_ino);
        let src_ino = std::fs::metadata(ws_path.join("src/lib.rs"))
            .expect("src meta")
            .ino();
        assert!(src_inos.insert(src_ino), "editable source files must have distinct inodes");
    }
}

#[cfg(target_os = "windows")]
#[test]
fn projfs_backend_fails_fast_when_auto_start_disabled_and_socket_missing() {
    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("no-daemon-win");
    std::fs::create_dir_all(repo_root.join(".git/ozone")).expect("mkdir");

    let backend = ProjFsVfsBackend::new(false);
    let err = backend
        .spawn(&SpawnRequest {
            workspace_name: "ws1".to_string(),
            branch: "feat/ws1".to_string(),
            base_commit: None,
            base_tree: None,
            repo_root: repo_root.clone(),
            git_dir: repo_root.join(".git"),
            target_path: dir.path().join("no-daemon-win.oz/ws1"),
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

#[cfg(target_os = "windows")]
#[test]
fn projfs_vfs_backend_ipc_roundtrips_mount_and_unmount_against_mock_tcp_listener() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("win-vfs-repo");
    std::fs::create_dir_all(repo_root.join("src")).expect("mkdir src");
    std::fs::write(repo_root.join("README.md"), "# Win VFS\n").expect("write readme");
    let c1 = init_repo(&repo_root);
    let ozone_dir = repo_root.join(".git/ozone");
    std::fs::create_dir_all(&ozone_dir).expect("mkdir ozone");
    capture_baseline(&repo_root, &ozone_dir, &c1, &OzoneConfig::default()).expect("capture c1");

    assert!(
        Command::new("git")
            .args(["branch", "feat/win-vfs"])
            .current_dir(&repo_root)
            .output()
            .expect("git branch")
            .status
            .success()
    );

    let repo = OzoneRepo::discover(Some(&repo_root)).expect("discover");
    let sock_path = repo.layout.vfsd_socket_path();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock tcp");
    let port = listener.local_addr().expect("addr").port();
    std::fs::write(&sock_path, format!("tcp://127.0.0.1:{port}\n")).expect("write endpoint");

    let seen_requests = Arc::new(Mutex::new(Vec::<VfsRequest>::new()));
    let seen_clone = Arc::clone(&seen_requests);
    let server_handle = std::thread::spawn(move || {
        let mut active_mounts: Vec<MountInfo> = Vec::new();
        for stream in listener.incoming() {
            let mut stream = stream.expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read line");
            let req: VfsRequest = serde_json::from_str(line.trim()).expect("parse req");

            let mut done = false;
            let resp = match &req {
                VfsRequest::Ping => VfsResponse::Pong {
                    version: "0.1.0".to_string(),
                    pid: std::process::id(),
                },
                VfsRequest::ListMounts => VfsResponse::MountList {
                    mounts: active_mounts.clone(),
                },
                VfsRequest::Mount { spec } => {
                    let info = MountInfo {
                        workspace: spec.workspace.clone(),
                        branch: spec.branch.clone(),
                        mount_point: spec.mount_point.clone(),
                        upper_dir: spec.upper_dir.clone(),
                        mounted: true,
                        dirty_count: 0,
                    };
                    active_mounts.push(info.clone());
                    seen_clone.lock().expect("lock").push(req.clone());
                    VfsResponse::Mounted { mount: info }
                }
                VfsRequest::Unmount { workspace, .. } => {
                    active_mounts.retain(|m| m.workspace != *workspace);
                    seen_clone.lock().expect("lock").push(req.clone());
                    done = true;
                    VfsResponse::Unmounted {
                        workspace: workspace.clone(),
                    }
                }
                other => VfsResponse::Error {
                    code: "unexpected".to_string(),
                    message: format!("unexpected request: {other:?}"),
                },
            };

            let encoded = encode_line(&resp).expect("encode resp");
            stream.write_all(encoded.as_bytes()).expect("write resp");
            stream.flush().expect("flush");
            if done {
                break;
            }
        }
    });

    let mut opts = SpawnWorkspaceOptions::new("agent-win");
    opts.branch = Some("feat/win-vfs".to_string());
    opts.backend = Some(BackendKind::WindowsProjFs);
    let spawned = spawn_workspace(&repo, &opts).expect("spawn projfs workspace");
    assert_eq!(spawned.state.backend, BackendKind::WindowsProjFs);

    let mount_json: MountRecord = serde_json::from_str(
        &std::fs::read_to_string(repo.layout.ws_mount_record_path("agent-win")).expect("read"),
    )
    .expect("parse mount.json");
    assert!(mount_json.mounted);

    remove_workspace(&repo, "agent-win", &RemoveWorkspaceOptions::default())
        .expect("remove projfs workspace");

    server_handle.join().expect("server thread join");
    let requests = seen_requests.lock().expect("lock");
    assert_eq!(requests.len(), 2);
    assert!(matches!(&requests[0], VfsRequest::Mount { .. }));
    assert!(matches!(&requests[1], VfsRequest::Unmount { workspace, force: true } if workspace == "agent-win"));
}
