// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cross-platform unit/integration tests for `oz-vfsd` ProjFS layer engine,
//! CoW upper/whiteout/opaque persistence, `windows_setup`, and loopback TCP IPC.

use oz_core::{ChangeKind, OzError};
use oz_vfsd::{
    LayerKind, MountSpec, OverlayMountConfig, PROJFS_ENABLE_POWERSHELL_CMD, ProjFsLayerEngine,
    VfsRequest, VfsResponse, compute_instance_guid, enable_windows_projfs_with,
    generate_windows_startup_cmd, run_tcp_daemon, scan_dirty_paths, send_tcp_request,
};
use std::fs::FileTimes;
use std::path::Path;
use std::process::{ExitStatus, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

#[cfg(unix)]
fn exit_status(code: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(code << 8)
}

#[cfg(windows)]
fn exit_status(code: i32) -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(code as u32)
}

fn make_spec(root: &Path, with_seed: bool, with_base: bool) -> MountSpec {
    let repo_root = root.join("repo");
    let git_dir = repo_root.join(".git");
    let ws_root = git_dir.join("ozone/ws/ws1");
    let upper_dir = ws_root.join("upper");
    let meta_dir = ws_root.join("meta");
    let mount_point = root.join("repo.oz/ws1");
    let baseline_dir = with_base.then(|| git_dir.join("ozone/baselines/base1/root"));

    std::fs::create_dir_all(&repo_root).expect("mkdir repo");
    std::fs::create_dir_all(&git_dir).expect("mkdir git");
    std::fs::create_dir_all(&upper_dir).expect("mkdir upper");
    std::fs::create_dir_all(meta_dir.join("whiteouts")).expect("mkdir whiteouts");
    std::fs::create_dir_all(meta_dir.join("opaque")).expect("mkdir opaque");
    std::fs::create_dir_all(meta_dir.join("seed_whiteouts")).expect("mkdir seed_whiteouts");
    std::fs::create_dir_all(&mount_point).expect("mkdir mount_point");
    if with_seed {
        std::fs::create_dir_all(ws_root.join("seed")).expect("mkdir seed");
    }
    if let Some(ref b) = baseline_dir {
        std::fs::create_dir_all(b).expect("mkdir baseline");
    }

    MountSpec {
        workspace: "ws1".to_string(),
        branch: "feat/ws1".to_string(),
        repo_root,
        git_dir,
        mount_point,
        upper_dir,
        meta_dir,
        base_tree: None,
        base_commit: None,
        baseline_dir,
        allow_other: false,
        writeback_cache: true,
    }
}

#[test]
fn projfs_layer_engine_resolves_layers_hides_markers_and_supports_repo_fallback() {
    let dir = TempDir::new().expect("tempdir");
    let spec = make_spec(dir.path(), true, true);
    let base = spec.baseline_dir.as_ref().expect("base");
    let seed = spec.resolved_seed_dir().expect("seed");

    std::fs::write(base.join("base.txt"), b"from-base").expect("write base");
    std::fs::write(base.join("shared.txt"), b"base-shared").expect("write shared");
    std::fs::write(base.join("seed_deleted.txt"), b"gone-in-seed").expect("write seed_del");
    std::fs::write(seed.join("shared.txt"), b"from-seed").expect("write seed");
    std::fs::write(spec.upper_dir.join("upper.txt"), b"from-upper").expect("write upper");
    std::fs::write(spec.upper_dir.join("stray.oz_wh"), b"").expect("write stray wh");
    std::fs::write(spec.upper_dir.join("stray.oz_opq"), b"").expect("write stray opq");
    std::fs::write(
        spec.meta_dir.join("seed_whiteouts/seed_deleted.txt.oz_wh"),
        b"",
    )
    .expect("write swh");

    let engine = ProjFsLayerEngine::from_spec(&spec);
    let entries = engine.enumerate_dir("", None).expect("enum root");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["base.txt", "shared.txt", "upper.txt"]);

    let shared = engine
        .placeholder_info("shared.txt")
        .expect("ph")
        .expect("exists");
    assert_eq!(shared.layer, LayerKind::Seed);
    assert_eq!(shared.file_size, 9);
    assert!(engine
        .placeholder_info("seed_deleted.txt")
        .expect("ph")
        .is_none());
    assert!(engine.placeholder_info("stray.oz_wh").expect("ph").is_none());
    assert!(engine.placeholder_info("stray.oz_opq").expect("ph").is_none());

    // Without seed or baseline, engine falls back to repo_root while hiding `.git/`.
    let dir2 = TempDir::new().expect("tempdir2");
    let spec2 = make_spec(dir2.path(), false, false);
    std::fs::write(spec2.repo_root.join("README.md"), b"# repo\n").expect("write readme");
    std::fs::write(spec2.git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("write HEAD");
    let engine2 = ProjFsLayerEngine::from_spec(&spec2);
    let root2 = engine2.enumerate_dir("", None).expect("enum fallback");
    assert_eq!(root2.len(), 1);
    assert_eq!(root2[0].name, "README.md");
    assert!(engine2.placeholder_info(".git").expect("git hidden").is_none());
    assert!(
        engine2
            .placeholder_info(".git/HEAD")
            .expect("git child hidden")
            .is_none()
    );
    assert_eq!(
        engine2
            .read_hydration_data("README.md", 0, 16)
            .expect("hydrate readme"),
        b"# repo\n"
    );
}

#[test]
fn projfs_enumerate_dir_sorts_case_insensitively_and_matches_dos_wildcards() {
    let dir = TempDir::new().expect("tempdir");
    let spec = make_spec(dir.path(), false, true);
    let base = spec.baseline_dir.as_ref().expect("base");

    for name in [
        "_meta.txt",
        "B.txt",
        "a.txt",
        "alpha.b.txt",
        "foo",
        "foo.",
        "foo.txt",
        "foobar",
    ] {
        std::fs::write(base.join(name), name.as_bytes()).expect("write");
    }
    std::fs::create_dir_all(base.join("sub/nested")).expect("mkdir sub");
    std::fs::write(base.join("sub/nested/Item.rs"), b"fn x() {}\n").expect("write item");

    let engine = ProjFsLayerEngine::from_spec(&spec);
    let all: Vec<String> = engine
        .enumerate_dir("", None)
        .expect("enum all")
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(
        all,
        vec![
            "a.txt",
            "alpha.b.txt",
            "B.txt",
            "foo",
            "foo.",
            "foo.txt",
            "foobar",
            "sub",
            "_meta.txt",
        ]
    );

    let txt_dos_star: Vec<String> = engine
        .enumerate_dir("", Some("<.TXT"))
        .expect("enum <.TXT")
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(
        txt_dos_star,
        vec!["a.txt", "alpha.b.txt", "B.txt", "foo.txt", "_meta.txt"]
    );

    let foo_dos_dot: Vec<String> = engine
        .enumerate_dir("", Some("foo\"<"))
        .expect("enum foo\"<")
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(foo_dos_dot, vec!["foo", "foo.", "foo.txt"]);

    // Windows backslash relative directory paths work cross-platform.
    let nested = engine
        .enumerate_dir("sub\\nested", Some("????.RS"))
        .expect("enum backslash");
    assert_eq!(nested.len(), 1);
    assert_eq!(nested[0].name, "Item.rs");
    let ph = engine
        .placeholder_info("sub\\nested\\Item.rs")
        .expect("ph")
        .expect("exists");
    assert_eq!(ph.windows_rel_path, "sub\\nested\\Item.rs");
}

#[test]
fn projfs_preserves_mtime_as_windows_filetime_and_hydrates_byte_ranges() {
    let dir = TempDir::new().expect("tempdir");
    let spec = make_spec(dir.path(), false, true);
    let base = spec.baseline_dir.as_ref().expect("base");

    let payload: Vec<u8> = (0..(150 * 1024)).map(|i| (i % 251) as u8).collect();
    let p = base.join("big.bin");
    std::fs::write(&p, &payload).expect("write big.bin");
    let fixed_mtime = UNIX_EPOCH + Duration::new(1_700_000_000, 500_000_000);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&p)
        .expect("open")
        .set_times(FileTimes::new().set_modified(fixed_mtime))
        .expect("set_times");

    let engine = ProjFsLayerEngine::from_spec(&spec);
    let ph = engine
        .placeholder_info("big.bin")
        .expect("ph")
        .expect("exists");
    assert_eq!(ph.file_size, payload.len() as u64);
    assert_eq!(ph.last_write_time, 133_444_736_005_000_000);

    let dir_entries = engine.enumerate_dir("", Some("*.bin")).expect("enum");
    assert_eq!(dir_entries[0].last_write_time, 133_444_736_005_000_000);

    let c1 = engine
        .read_hydration_data("big.bin", 0, 65_536)
        .expect("chunk1");
    assert_eq!(c1, &payload[0..65_536]);

    let c2 = engine
        .read_hydration_data("big.bin", 65_536, 65_536)
        .expect("chunk2");
    assert_eq!(c2, &payload[65_536..131_072]);

    let c3 = engine
        .read_hydration_data("big.bin", 131_072, 65_536)
        .expect("chunk3");
    assert_eq!(c3, &payload[131_072..]);

    let past_eof = engine
        .read_hydration_data("big.bin", payload.len() as u64 + 100, 1024)
        .expect("past eof");
    assert!(past_eof.is_empty());

    assert!(engine.read_hydration_data("missing.bin", 0, 16).is_err());
}

#[test]
fn projfs_cow_persistence_produces_identical_scan_dirty_paths_to_fuse3() {
    let dir = TempDir::new().expect("tempdir");
    let spec = make_spec(dir.path(), true, true);
    let base = spec.baseline_dir.as_ref().expect("base");
    let seed = spec.resolved_seed_dir().expect("seed");

    std::fs::write(base.join("tracked.txt"), b"original").expect("write tracked");
    std::fs::write(base.join("to_delete.txt"), b"bye").expect("write to_delete");
    std::fs::create_dir_all(base.join("pkg/sub")).expect("mkdir pkg");
    std::fs::write(base.join("pkg/a.txt"), b"a").expect("write a");
    std::fs::write(base.join("pkg/sub/b.txt"), b"b").expect("write b");
    std::fs::create_dir_all(base.join("mv_dir")).expect("mkdir mv_dir");
    std::fs::write(base.join("mv_dir/base_item.txt"), b"mv-base").expect("write mv_base");
    std::fs::write(seed.join("mv_dir_seed_only.txt"), b"seed").expect("write seed");
    std::fs::create_dir_all(spec.upper_dir.join("mv_dir")).expect("mkdir upper mv_dir");
    std::fs::write(spec.upper_dir.join("mv_dir/upper_item.txt"), b"mv-upper")
        .expect("write mv_upper");

    let cfg = OverlayMountConfig::from_spec(&spec);
    let engine = ProjFsLayerEngine::from_spec(&spec);

    // 1. Create new file & modify existing file in mount_point.
    std::fs::write(spec.mount_point.join("brand_new.txt"), b"fresh-content").expect("write new");
    engine
        .sync_written_file("brand_new.txt")
        .expect("sync brand_new");
    std::fs::write(spec.mount_point.join("tracked.txt"), b"modified-content").expect("write mod");
    engine
        .sync_written_file("tracked.txt")
        .expect("sync tracked");
    assert_eq!(
        std::fs::read_to_string(base.join("tracked.txt")).expect("baseline intact"),
        "original"
    );

    // 2. Delete baseline file -> whiteout created.
    engine
        .record_deleted_path("to_delete.txt")
        .expect("delete to_delete");
    assert!(spec.meta_dir.join("whiteouts/to_delete.txt.oz_wh").is_file());

    // 3. Delete directory pkg then recreate pkg -> opaque marker hides lower files.
    engine.record_deleted_path("pkg").expect("delete pkg");
    assert!(spec.meta_dir.join("whiteouts/pkg.oz_wh").is_file());
    std::fs::create_dir_all(spec.mount_point.join("pkg")).expect("mkdir mnt pkg");
    engine.sync_written_file("pkg").expect("sync recreated pkg");
    assert!(!spec.meta_dir.join("whiteouts/pkg.oz_wh").exists());
    assert!(spec.meta_dir.join("opaque/pkg.oz_opaque").is_file());
    std::fs::write(spec.mount_point.join("pkg/fresh.txt"), b"fresh").expect("write pkg/fresh");
    engine
        .sync_written_file("pkg/fresh.txt")
        .expect("sync pkg/fresh");
    let pkg_entries: Vec<String> = engine
        .enumerate_dir("pkg", None)
        .expect("enum pkg")
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(pkg_entries, vec!["fresh.txt"]);

    // 4. Rename mixed directory (baseline + upper) -> moved_dir in upper + whiteouts for mv_dir.
    engine
        .record_renamed_path("mv_dir", "moved_dir")
        .expect("rename dir");
    assert!(!spec.upper_dir.join("mv_dir").exists());
    assert_eq!(
        std::fs::read_to_string(spec.upper_dir.join("moved_dir/base_item.txt")).expect("moved base"),
        "mv-base"
    );
    assert_eq!(
        std::fs::read_to_string(spec.upper_dir.join("moved_dir/upper_item.txt"))
            .expect("moved upper"),
        "mv-upper"
    );

    // 5. Exercise dispatch_projfs_notification across all 6 ProjFS notification masks.
    std::fs::write(spec.mount_point.join("notif.txt"), b"v1").expect("write notif");
    oz_vfsd::projfs::dispatch_projfs_notification(
        &engine,
        oz_vfsd::projfs::PRJ_NOTIFY_NEW_FILE_CREATED,
        "notif.txt",
        None,
    )
    .expect("notif created");
    std::fs::write(spec.mount_point.join("notif.txt"), b"v2").expect("overwrite notif");
    oz_vfsd::projfs::dispatch_projfs_notification(
        &engine,
        oz_vfsd::projfs::PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED,
        "notif.txt",
        None,
    )
    .expect("notif modified");
    assert_eq!(
        std::fs::read_to_string(spec.upper_dir.join("notif.txt")).expect("upper notif"),
        "v2"
    );
    oz_vfsd::projfs::dispatch_projfs_notification(
        &engine,
        oz_vfsd::projfs::PRJ_NOTIFY_FILE_RENAMED,
        "notif.txt",
        Some("notif_renamed.txt"),
    )
    .expect("notif renamed");
    oz_vfsd::projfs::dispatch_projfs_notification(
        &engine,
        oz_vfsd::projfs::PRJ_NOTIFY_PRE_DELETE,
        "notif_renamed.txt",
        None,
    )
    .expect("notif pre_delete");
    oz_vfsd::projfs::dispatch_projfs_notification(
        &engine,
        oz_vfsd::projfs::PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED,
        "notif_renamed.txt",
        None,
    )
    .expect("notif deleted");
    assert!(!spec.upper_dir.join("notif_renamed.txt").exists());

    let dirty = scan_dirty_paths(&cfg).expect("scan_dirty_paths");
    let summary: Vec<(String, ChangeKind)> =
        dirty.into_iter().map(|d| (d.path, d.kind)).collect();
    assert_eq!(
        summary,
        vec![
            ("brand_new.txt".to_string(), ChangeKind::Added),
            ("moved_dir/base_item.txt".to_string(), ChangeKind::Added),
            ("moved_dir/upper_item.txt".to_string(), ChangeKind::Added),
            ("mv_dir/base_item.txt".to_string(), ChangeKind::Deleted),
            ("pkg/a.txt".to_string(), ChangeKind::Deleted),
            ("pkg/fresh.txt".to_string(), ChangeKind::Added),
            ("pkg/sub/b.txt".to_string(), ChangeKind::Deleted),
            ("to_delete.txt".to_string(), ChangeKind::Deleted),
            ("tracked.txt".to_string(), ChangeKind::Modified),
        ]
    );
}

#[test]
fn projfs_guid_and_windows_setup_helpers_behave_deterministically() {
    assert_eq!(
        compute_instance_guid(Path::new("/repo/.git"), "alpha"),
        compute_instance_guid(Path::new("/repo/.git/"), "alpha")
    );
    assert_ne!(
        compute_instance_guid(Path::new("/repo/.git"), "alpha"),
        compute_instance_guid(Path::new("/repo/.git"), "beta")
    );

    assert_eq!(
        PROJFS_ENABLE_POWERSHELL_CMD,
        "Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart"
    );

    let cmd = generate_windows_startup_cmd(
        Path::new("C:\\Ozone\\oz-vfsd.exe"),
        Path::new("C:\\Work\\repo\\.git"),
    );
    assert!(cmd.contains("oz-vfsd.exe"));
    assert!(cmd.contains("--git-dir"));
    assert!(cmd.contains("--socket"));
    assert!(cmd.contains("vfsd.sock"));
    assert!(cmd.contains("--foreground"));

    let sys_dir = TempDir::new().expect("sys_dir");
    let err = enable_windows_projfs_with(sys_dir.path(), |_| {
        Ok(Output {
            status: exit_status(1),
            stdout: Vec::new(),
            stderr: b"Access is denied".to_vec(),
        })
    })
    .expect_err("must fail without admin");
    assert!(matches!(err, OzError::CheckFailed { .. }));
    assert!(err
        .recovery_commands()
        .iter()
        .any(|r| r.contains(PROJFS_ENABLE_POWERSHELL_CMD)));
}

#[test]
fn tcp_loopback_daemon_round_trips_ipc_and_cleans_up_endpoint_on_shutdown() {
    let dir = TempDir::new().expect("tempdir");
    let git_dir = dir.path().join("repo/.git");
    let sock_path = git_dir.join("ozone/vfsd.sock");
    std::fs::create_dir_all(&git_dir).expect("mkdir git");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio rt");

    let git_srv = git_dir.clone();
    let sock_srv = sock_path.clone();
    let server_task = rt.spawn(async move { run_tcp_daemon(&git_srv, &sock_srv).await });

    let deadline = SystemTime::now() + Duration::from_secs(5);
    while !sock_path.is_file() {
        assert!(
            SystemTime::now() < deadline,
            "timed out waiting for TCP endpoint file {}",
            sock_path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let endpoint = std::fs::read_to_string(&sock_path).expect("read endpoint");
    assert!(
        endpoint.starts_with("tcp://127.0.0.1:") && endpoint.ends_with('\n'),
        "unexpected endpoint file content: {endpoint:?}"
    );

    match send_tcp_request(&sock_path, &VfsRequest::Ping).expect("ping") {
        VfsResponse::Pong { pid, .. } => assert_eq!(pid, std::process::id()),
        other => panic!("unexpected Ping response: {other:?}"),
    }

    match send_tcp_request(&sock_path, &VfsRequest::Status).expect("status") {
        VfsResponse::DaemonStatus {
            pid, active_mounts, ..
        } => {
            assert_eq!(pid, std::process::id());
            assert!(active_mounts.is_empty());
        }
        other => panic!("unexpected Status response: {other:?}"),
    }

    match send_tcp_request(&sock_path, &VfsRequest::ListMounts).expect("list mounts") {
        VfsResponse::MountList { mounts } => assert!(mounts.is_empty()),
        other => panic!("unexpected ListMounts response: {other:?}"),
    }

    match send_tcp_request(
        &sock_path,
        &VfsRequest::DirtyPaths {
            workspace: "missing".to_string(),
        },
    )
    .expect("dirty missing")
    {
        VfsResponse::Error { code, .. } => assert_eq!(code, "workspace_not_found"),
        other => panic!("unexpected DirtyPaths response: {other:?}"),
    }

    match send_tcp_request(&sock_path, &VfsRequest::Shutdown).expect("shutdown") {
        VfsResponse::ShuttingDown => {}
        other => panic!("unexpected Shutdown response: {other:?}"),
    }

    let daemon_res = rt.block_on(server_task).expect("join daemon task");
    daemon_res.expect("run_tcp_daemon exited cleanly");
    assert!(!sock_path.exists());
}
