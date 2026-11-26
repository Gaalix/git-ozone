// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Non-mount POSIX & VFS daemon IPC soak tests for `oz-vfsd`.
//!
//! These tests exercise advisory locking (`flock` + `fcntl`), `mmap`/`msync`, atomic `rename`,
//! `fsync`/`fsyncdir`, `scan_dirty_paths` under concurrent mutations, stale socket/symlink
//! cleanup, daemon lock mutual exclusion, and IPC concurrency without requiring `/dev/fuse`.

#![cfg(unix)]
#![allow(clippy::too_many_lines)]

use fs4::{FileExt as Fs4FileExt, TryLockError};
use oz_core::{BackendKind, ChangeKind, MountRecord, OzoneLayout};
use oz_vfsd::{
    MountSpec, OverlayMountConfig, VfsRequest, VfsResponse, dirty_paths_via_daemon,
    is_fuse_mount_in_proc_mountinfo, lazy_unmount_fuse, ping_daemon, run_daemon, run_tcp_daemon,
    scan_dirty_paths, send_request, send_tcp_request, shutdown_daemon, socket_path_for_git_dir,
    unmount_via_daemon,
};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn make_overlay_cfg(root: &Path, ws: &str) -> OverlayMountConfig {
    let git_dir = root.join("repo/.git");
    let ws_meta = git_dir.join("ozone/ws").join(ws);
    let cfg = OverlayMountConfig {
        workspace: ws.to_string(),
        mount_point: root.join("repo.oz").join(ws),
        upper_dir: ws_meta.join("upper"),
        seed_dir: Some(ws_meta.join("seed")),
        baseline_dir: Some(git_dir.join("ozone/baselines/b1/root")),
        meta_dir: ws_meta.join("meta"),
        allow_other: false,
        writeback_cache: true,
    };
    std::fs::create_dir_all(cfg.seed_dir.as_ref().expect("seed")).expect("mkdir seed");
    std::fs::create_dir_all(cfg.baseline_dir.as_ref().expect("base")).expect("mkdir base");
    cfg.ensure_directories().expect("ensure dirs");
    cfg
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if std::fs::symlink_metadata(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {}", path.display());
}

#[test]
fn posix_flock_and_fcntl_advisory_lock_soak_including_cross_process() {
    let dir = TempDir::new().expect("tempdir");
    let cfg = make_overlay_cfg(dir.path(), "ws-locks");
    let lock_path = cfg.upper_dir.join("target/.cargo-lock");
    std::fs::create_dir_all(lock_path.parent().expect("parent")).expect("mkdir target");
    std::fs::write(&lock_path, b"").expect("create .cargo-lock");

    // 1. Multi-thread flock mutual exclusion + shared/exclusive transition soak.
    let active_writers = Arc::new(AtomicUsize::new(0));
    let threads = 8usize;
    let rounds = 40usize;
    let barrier = Arc::new(Barrier::new(threads));

    std::thread::scope(|s| {
        for tid in 0..threads {
            let b = Arc::clone(&barrier);
            let lp = &lock_path;
            let aw = Arc::clone(&active_writers);
            s.spawn(move || {
                b.wait();
                for r in 0..rounds {
                    let f = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(lp)
                        .expect("open lock");
                    Fs4FileExt::lock(&f).expect("exclusive flock");
                    let prev = aw.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(prev, 0, "exclusive flock violated at tid={tid} round={r}");
                    std::thread::yield_now();
                    aw.fetch_sub(1, Ordering::SeqCst);
                    Fs4FileExt::unlock(&f).expect("unlock flock");
                }
            });
        }
    });

    // 2. Non-blocking flock contention check.
    let holder = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open holder");
    Fs4FileExt::lock(&holder).expect("lock holder");
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open contender");
    assert!(matches!(
        Fs4FileExt::try_lock(&contender),
        Err(TryLockError::WouldBlock)
    ));
    Fs4FileExt::unlock(&holder).expect("unlock holder");
    Fs4FileExt::try_lock(&contender).expect("acquire after unlock");
    Fs4FileExt::unlock(&contender).expect("unlock contender");
    drop(holder);
    drop(contender);

    // 3. Cross-process POSIX fcntl (F_SETLK / F_GETLK) + flock contention via python3 if present.
    if let Ok(py_status) = Command::new("python3").arg("--version").status()
        && py_status.success()
    {
        let fcntl_file = cfg.upper_dir.join("target/.cargo-build-lock");
        let f = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&fcntl_file)
            .expect("open fcntl file");

        // SAFETY: `libc::flock` is a plain C struct for which all-zeroes is a valid initial state.
        let mut fl: libc::flock = unsafe { std::mem::zeroed() };
        fl.l_type = libc::c_short::try_from(libc::F_WRLCK).expect("c_short");
        fl.l_whence = libc::c_short::try_from(libc::SEEK_SET).expect("c_short");
        fl.l_start = 0;
        fl.l_len = 64;
        // SAFETY: `f.as_raw_fd()` is an open writable file descriptor and `&fl` is a valid `libc::flock`.
        assert_eq!(unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETLK, &fl) }, 0);

        let probe_out = Command::new("python3")
            .args([
                "-c",
                "import fcntl, os, struct, sys; \
                 fd = os.open(sys.argv[1], os.O_RDWR); \
                 fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB) if sys.argv[2] == 'flock' else fcntl.lockf(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)",
                fcntl_file.to_str().expect("utf8"),
                "fcntl",
            ])
            .output()
            .expect("run python fcntl probe");
        assert!(
            !probe_out.status.success(),
            "child fcntl lock should fail while parent holds F_WRLCK"
        );

        fl.l_type = libc::c_short::try_from(libc::F_UNLCK).expect("c_short");
        // SAFETY: `f.as_raw_fd()` is still open and `&fl` is a valid initialized `libc::flock`.
        assert_eq!(unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETLK, &fl) }, 0);

        let after_unlock = Command::new("python3")
            .args([
                "-c",
                "import fcntl, os, sys; \
                 fd = os.open(sys.argv[1], os.O_RDWR); \
                 fcntl.lockf(fd, fcntl.LOCK_EX | fcntl.LOCK_NB); \
                 fcntl.lockf(fd, fcntl.LOCK_UN)",
                fcntl_file.to_str().expect("utf8"),
            ])
            .output()
            .expect("run python fcntl after unlock");
        assert!(
            after_unlock.status.success(),
            "child fcntl lock failed after unlock: {}",
            String::from_utf8_lossy(&after_unlock.stderr)
        );
    }
}

#[test]
fn posix_mmap_msync_atomic_rename_and_fsync_soak_with_dirty_scan() {
    let dir = TempDir::new().expect("tempdir");
    let cfg = make_overlay_cfg(dir.path(), "ws-soak");
    let base = cfg.baseline_dir.as_ref().expect("base");
    std::fs::write(base.join("base.rs"), b"pub fn base() -> u32 { 1 }\n").expect("write base");

    // 1. MAP_SHARED read/write mmap + msync + fsync + fsyncdir.
    let mmap_path = cfg.upper_dir.join("mapped.bin");
    let f = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&mmap_path)
        .expect("open mmap");
    f.set_len(4096).expect("set_len");

    // SAFETY: `f` is an open 4096-byte read/write file descriptor; passing a null address lets
    // the kernel pick a valid page-aligned mapping of length 4096.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            f.as_raw_fd(),
            0,
        )
    };
    assert_ne!(ptr, libc::MAP_FAILED, "mmap failed");
    // SAFETY: `ptr` is a valid non-`MAP_FAILED` 4096-byte writable mapping exclusively accessed
    // by this thread until `munmap` releases it below.
    unsafe {
        let slice = std::slice::from_raw_parts_mut(ptr.cast::<u8>(), 4096);
        slice[..11].copy_from_slice(b"MMAP_SOAK_!");
        assert_eq!(libc::msync(ptr, 4096, libc::MS_SYNC), 0);
        assert_eq!(libc::munmap(ptr, 4096), 0);
    }
    f.sync_all().expect("fsync mapped");
    drop(f);

    let dir_fd = File::open(&cfg.upper_dir).expect("open upper dir");
    // SAFETY: `dir_fd.as_raw_fd()` is an open directory file descriptor.
    assert_eq!(unsafe { libc::fsync(dir_fd.as_raw_fd()) }, 0);
    drop(dir_fd);

    let mut verified = vec![0u8; 11];
    File::open(&mmap_path)
        .expect("reopen")
        .read_exact(&mut verified)
        .expect("read");
    assert_eq!(&verified, b"MMAP_SOAK_!");

    // 2. Concurrent atomic write-fsync-rename soak alongside scan_dirty_paths.
    let writers = 6usize;
    let iters = 50usize;
    let barrier = Arc::new(Barrier::new(writers + 1));

    std::thread::scope(|s| {
        for wid in 0..writers {
            let b = Arc::clone(&barrier);
            let cfg_ref = &cfg;
            s.spawn(move || {
                b.wait();
                let target = cfg_ref.upper_dir.join(format!("slot_{wid}.txt"));
                for it in 0..iters {
                    let tmp = cfg_ref.upper_dir.join(format!(".tmp_{wid}_{it}"));
                    let payload = format!("writer={wid}:iter={it:04}:ok\n");
                    {
                        let mut tf = OpenOptions::new()
                            .create_new(true)
                            .write(true)
                            .open(&tmp)
                            .expect("create tmp");
                        tf.write_all(payload.as_bytes()).expect("write tmp");
                        tf.sync_all().expect("fsync tmp");
                    }
                    std::fs::rename(&tmp, &target).expect("atomic rename");
                }
            });
        }

        let b_scan = Arc::clone(&barrier);
        let cfg_scan = &cfg;
        s.spawn(move || {
            b_scan.wait();
            for _ in 0..40 {
                let entries = scan_dirty_paths(cfg_scan).expect("scan_dirty_paths during soak");
                assert!(entries.iter().any(|e| e.path == "mapped.bin"));
                std::thread::yield_now();
            }
        });
    });

    let final_dirty = scan_dirty_paths(&cfg).expect("final dirty scan");
    for wid in 0..writers {
        let fname = format!("slot_{wid}.txt");
        let content = std::fs::read_to_string(cfg.upper_dir.join(&fname)).expect("read slot");
        assert_eq!(content, format!("writer={wid}:iter={:04}:ok\n", iters - 1));
        assert!(
            final_dirty
                .iter()
                .any(|d| d.path == fname && d.kind == ChangeKind::Added)
        );
    }
}

#[test]
fn daemon_unix_ipc_soak_stale_socket_lock_exclusion_and_clean_shutdown() {
    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("repo");
    let git_dir = repo_root.join(".git");
    let layout = OzoneLayout::from_git_common_dir(&git_dir);
    layout.ensure_initialized().expect("init layout");

    let sock = socket_path_for_git_dir(&git_dir);
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent).expect("mkdir sock parent");
    }

    // Plant a dangling symlink at vfsd.sock to verify stale-endpoint cleanup on daemon start.
    let dangling_target = dir.path().join("nonexistent-target");
    std::os::unix::fs::symlink(&dangling_target, &sock).expect("plant dangling socket symlink");

    // Seed a workspace upper_dir + spec.json + mounted:false record for DirtyPaths IPC testing.
    let ws_dir = layout.ws_meta_dir("ws1");
    let upper_dir = layout.ws_upper_dir("ws1");
    let meta_dir = layout.ws_tombstone_dir("ws1");
    std::fs::create_dir_all(&upper_dir).expect("mkdir upper");
    std::fs::create_dir_all(meta_dir.join("whiteouts")).expect("mkdir wh");
    std::fs::create_dir_all(meta_dir.join("opaque")).expect("mkdir opq");
    std::fs::create_dir_all(meta_dir.join("seed_whiteouts")).expect("mkdir swh");
    std::fs::write(upper_dir.join("edited.rs"), b"pub fn ok() {}\n").expect("write upper");

    let spec = MountSpec {
        workspace: "ws1".to_string(),
        branch: "feat/ws1".to_string(),
        repo_root: repo_root.clone(),
        git_dir: git_dir.clone(),
        mount_point: dir.path().join("repo.oz/ws1"),
        upper_dir: upper_dir.clone(),
        meta_dir: meta_dir.clone(),
        base_tree: None,
        base_commit: None,
        baseline_dir: None,
        allow_other: false,
        writeback_cache: true,
    };
    std::fs::write(
        ws_dir.join("spec.json"),
        serde_json::to_vec_pretty(&spec).expect("json"),
    )
    .expect("write spec.json");
    let now = chrono::Utc::now();
    let rec = MountRecord {
        schema_version: MountRecord::CURRENT_SCHEMA_VERSION,
        workspace: "ws1".to_string(),
        branch: "feat/ws1".to_string(),
        backend: BackendKind::LinuxFuse3,
        repo_root: repo_root.clone(),
        git_dir: git_dir.clone(),
        mount_point: spec.mount_point.clone(),
        upper_dir: Some(upper_dir),
        meta_dir: Some(meta_dir),
        base_tree: None,
        base_commit: None,
        socket_path: Some(sock.clone()),
        daemon_pid: None,
        mounted: false,
        created_at: now,
        updated_at: now,
    };
    std::fs::write(
        layout.ws_mount_record_path("ws1"),
        serde_json::to_vec_pretty(&rec).expect("rec json"),
    )
    .expect("write mount.json");

    let git_dir_srv = git_dir.clone();
    let sock_srv = sock.clone();
    let daemon_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(run_daemon(&git_dir_srv, &sock_srv))
    });

    // Wait for daemon socket to accept connections.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if ping_daemon(&sock).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not start in time");
        std::thread::sleep(Duration::from_millis(10));
    }

    // Starting a second daemon on the same git_dir must fail on vfsd.lock without clobbering sock.
    let rt2 = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt2");
    let dup_res = rt2.block_on(run_daemon(&git_dir, &sock));
    assert!(dup_res.is_err(), "duplicate daemon must reject vfsd.lock");

    // Concurrent IPC soak across 6 threads.
    std::thread::scope(|s| {
        for _ in 0..6 {
            let sock_ref = &sock;
            let gd_ref = &git_dir;
            s.spawn(move || {
                for _ in 0..25 {
                    let pong = ping_daemon(sock_ref).expect("ping");
                    assert_eq!(pong.pid, std::process::id());
                    assert!(matches!(
                        send_request(sock_ref, &VfsRequest::Status).expect("status"),
                        VfsResponse::DaemonStatus { .. }
                    ));
                    let dirty = dirty_paths_via_daemon(gd_ref, "ws1").expect("dirty");
                    assert_eq!(dirty.len(), 1);
                    assert_eq!(dirty[0].path, "edited.rs");
                }
            });
        }
    });

    unmount_via_daemon(&git_dir, "ws1", false).expect("unmount ws1");
    shutdown_daemon(&git_dir).expect("shutdown");
    daemon_thread.join().expect("join").expect("daemon exit ok");
    assert!(
        std::fs::symlink_metadata(&sock).is_err(),
        "socket cleaned up"
    );
}

#[test]
fn daemon_tcp_ipc_and_non_hanging_stale_path_probes() {
    let dir = TempDir::new().expect("tempdir");
    let git_dir = dir.path().join("tcp-repo/.git");
    std::fs::create_dir_all(&git_dir).expect("mkdir git");
    let sock = dir.path().join("tcp-repo/.git/ozone/vfsd.sock");

    let git_srv = git_dir.clone();
    let sock_srv = sock.clone();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(run_tcp_daemon(&git_srv, &sock_srv))
    });

    wait_for_file(&sock, Duration::from_secs(5));
    let pong = match send_tcp_request(&sock, &VfsRequest::Ping).expect("tcp ping") {
        VfsResponse::Pong { pid, .. } => pid,
        other => panic!("unexpected ping response: {other:?}"),
    };
    assert_eq!(pong, std::process::id());

    let mounts = match send_tcp_request(&sock, &VfsRequest::ListMounts).expect("list mounts") {
        VfsResponse::MountList { mounts } => mounts,
        other => panic!("unexpected list response: {other:?}"),
    };
    assert_eq!(mounts, &[]);

    assert!(matches!(
        send_tcp_request(&sock, &VfsRequest::Shutdown).expect("shutdown"),
        VfsResponse::ShuttingDown
    ));
    handle.join().expect("join").expect("tcp daemon clean exit");

    // Probe non-mounted paths including spaces to verify fast non-hanging return.
    let space_path = dir.path().join("My Repo.oz/ws 1");
    std::fs::create_dir_all(&space_path).expect("mkdir space path");
    let t0 = Instant::now();
    assert!(!is_fuse_mount_in_proc_mountinfo(&space_path));
    lazy_unmount_fuse(&space_path).expect("lazy unmount non-mount");
    assert!(t0.elapsed() < Duration::from_secs(2));
}
