// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg(target_os = "linux")]

//! Live `/dev/fuse` + `fusermount3` integration tests for `oz-vfsd`.

use oz_core::{ChangeKind, MountHealth, MountRecord, OzoneLayout, OzoneRepo, is_enotconn};
use oz_vfsd::{
    MountInfo, MountSpec, VfsRequest, VfsResponse, dirty_paths_via_daemon,
    ensure_daemon_and_mounts, lazy_unmount_fuse, mount_overlay, mount_via_daemon, ping_daemon,
    send_request, shutdown_daemon, socket_path_for_git_dir, unmount_via_daemon,
};
use std::fs::{FileTimes, OpenOptions, Permissions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

struct OverlayGuard {
    mounted: Option<oz_vfsd::MountedOverlay>,
    mount_point: PathBuf,
}

impl OverlayGuard {
    fn new(spec: &MountSpec) -> Self {
        let mounted = mount_overlay(spec).expect("mount live fuse overlay");
        Self {
            mounted: Some(mounted),
            mount_point: spec.mount_point.clone(),
        }
    }
}

impl Drop for OverlayGuard {
    fn drop(&mut self) {
        if let Some(m) = self.mounted.take() {
            let _ = m.unmount(true);
        } else {
            let _ = lazy_unmount_fuse(&self.mount_point);
        }
    }
}

struct DaemonGuard {
    git_dirs: Vec<PathBuf>,
    mount_points: Vec<PathBuf>,
    reap_pids: Vec<u32>,
}

impl DaemonGuard {
    fn new() -> Self {
        Self {
            git_dirs: Vec::new(),
            mount_points: Vec::new(),
            reap_pids: Vec::new(),
        }
    }

    fn track(&mut self, git_dir: PathBuf, mount_points: Vec<PathBuf>) {
        self.git_dirs.push(git_dir);
        self.mount_points.extend(mount_points);
    }

    fn track_pid(&mut self, pid: u32) {
        self.reap_pids.push(pid);
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        for git_dir in &self.git_dirs {
            let sock = socket_path_for_git_dir(git_dir);
            if let Ok(pong) = ping_daemon(&sock) {
                self.reap_pids.push(pong.pid);
            }
            let _ = shutdown_daemon(git_dir);
        }
        for mp in &self.mount_points {
            if std::fs::symlink_metadata(mp).err().is_some_and(|e| is_enotconn(&e)) {
                let _ = lazy_unmount_fuse(mp);
            }
            // Also check /proc/self/mountinfo in case a fuse mount is still attached.
            if let Ok(raw) = std::fs::read_to_string("/proc/self/mountinfo")
                && raw.contains(mp.to_string_lossy().as_ref())
            {
                let _ = lazy_unmount_fuse(mp);
            }
        }
        for pid in &self.reap_pids {
            let mut status: libc::c_int = 0;
            unsafe {
                let _ = libc::waitpid(*pid as libc::pid_t, &mut status, libc::WNOHANG);
            }
        }
    }
}

fn make_spec(root: &Path, ws: &str, with_seed: bool, with_base: bool) -> MountSpec {
    let git_dir = root.join("repo/.git");
    let ws_meta_parent = git_dir.join("ozone/ws").join(ws);
    let upper_dir = ws_meta_parent.join("upper");
    let meta_dir = ws_meta_parent.join("meta");
    let mount_point = root.join("repo.oz").join(ws);
    let baseline_dir = with_base.then(|| git_dir.join("ozone/baselines/base1/root"));

    std::fs::create_dir_all(&upper_dir).expect("mkdir upper");
    std::fs::create_dir_all(meta_dir.join("whiteouts")).expect("mkdir whiteouts");
    std::fs::create_dir_all(meta_dir.join("opaque")).expect("mkdir opaque");
    std::fs::create_dir_all(meta_dir.join("seed_whiteouts")).expect("mkdir seed_whiteouts");
    std::fs::create_dir_all(&mount_point).expect("mkdir mount_point");
    if with_seed {
        std::fs::create_dir_all(ws_meta_parent.join("seed")).expect("mkdir seed");
    }
    if let Some(ref b) = baseline_dir {
        std::fs::create_dir_all(b).expect("mkdir baseline");
    }

    MountSpec {
        workspace: ws.to_string(),
        branch: format!("feat/{ws}"),
        repo_root: root.join("repo"),
        git_dir,
        mount_point,
        upper_dir,
        meta_dir,
        base_tree: None,
        base_commit: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        baseline_dir,
        allow_other: false,
        writeback_cache: true,
    }
}

#[test]
fn live_fuse_three_layer_read_through_and_cow_writes() {
    let dir = TempDir::new().expect("tempdir");
    let spec = make_spec(dir.path(), "ws1", true, true);
    let base = spec.baseline_dir.as_ref().expect("base");
    let seed = spec.resolved_seed_dir().expect("seed");

    std::fs::write(base.join("base.txt"), b"from-base\n").expect("write base");
    std::fs::write(base.join("shared.txt"), b"base-shared\n").expect("write shared");
    std::fs::write(base.join("deleted_in_seed.txt"), b"stale").expect("write deleted");
    std::fs::create_dir_all(base.join("sub")).expect("mkdir sub");
    std::fs::write(base.join("sub/nested.txt"), b"base-nested").expect("write nested");
    std::fs::write(base.join("exec.sh"), b"#!/bin/sh\necho ok\n").expect("write exec");
    std::fs::set_permissions(base.join("exec.sh"), Permissions::from_mode(0o755)).expect("chmod");
    std::os::unix::fs::symlink("base.txt", base.join("link.txt")).expect("symlink");

    std::fs::write(seed.join("shared.txt"), b"from-seed\n").expect("write seed shared");
    std::fs::write(seed.join("seed_only.txt"), b"seed-only\n").expect("write seed only");
    std::fs::write(
        spec.meta_dir
            .join("seed_whiteouts/deleted_in_seed.txt.oz_wh"),
        b"",
    )
    .expect("write seed_whiteout");

    let _guard = OverlayGuard::new(&spec);
    let mnt = &spec.mount_point;

    assert_eq!(
        std::fs::read_to_string(mnt.join("base.txt")).expect("read base"),
        "from-base\n"
    );
    assert_eq!(
        std::fs::read_to_string(mnt.join("shared.txt")).expect("read seed shadow"),
        "from-seed\n"
    );
    assert!(!mnt.join("deleted_in_seed.txt").exists());
    assert_eq!(
        std::fs::read_to_string(mnt.join("seed_only.txt")).expect("read seed_only"),
        "seed-only\n"
    );
    assert_eq!(
        std::fs::read_to_string(mnt.join("sub/nested.txt")).expect("read nested"),
        "base-nested"
    );
    let exec_mode = std::fs::symlink_metadata(mnt.join("exec.sh"))
        .expect("stat exec")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(exec_mode, 0o755);
    assert_eq!(
        std::fs::read_link(mnt.join("link.txt")).expect("readlink"),
        PathBuf::from("base.txt")
    );

    // CoW modification writes to upper_dir without mutating baseline or seed.
    std::fs::write(mnt.join("base.txt"), b"from-upper\n").expect("write upper base");
    std::fs::write(mnt.join("shared.txt"), b"upper-shared\n").expect("write upper shared");
    std::fs::create_dir_all(mnt.join("new/dir")).expect("mkdir new/dir");
    let mut new_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(mnt.join("new/dir/file.txt"))
        .expect("create new file");
    new_file.write_all(b"created-in-upper").expect("write new");
    drop(new_file);
    std::fs::set_permissions(mnt.join("new/dir/file.txt"), Permissions::from_mode(0o640))
        .expect("chmod 640");

    // Truncate and mtime update on baseline file trigger copy-up without mutating baseline.
    let f = OpenOptions::new()
        .write(true)
        .open(mnt.join("sub/nested.txt"))
        .expect("open nested");
    f.set_len(4).expect("truncate");
    let fixed_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    f.set_times(FileTimes::new().set_modified(fixed_time))
        .expect("set mtime");
    drop(f);
    assert_eq!(
        std::fs::symlink_metadata(mnt.join("sub/nested.txt"))
            .expect("stat nested")
            .modified()
            .expect("mtime"),
        fixed_time
    );

    assert_eq!(
        std::fs::read_to_string(mnt.join("sub/nested.txt")).expect("read truncated"),
        "base"
    );
    assert_eq!(
        std::fs::read_to_string(base.join("sub/nested.txt")).expect("baseline intact"),
        "base-nested"
    );
    assert_eq!(
        std::fs::read_to_string(base.join("base.txt")).expect("baseline base intact"),
        "from-base\n"
    );
    assert_eq!(
        std::fs::read_to_string(seed.join("shared.txt")).expect("seed shared intact"),
        "from-seed\n"
    );
    assert_eq!(
        std::fs::read_to_string(spec.upper_dir.join("base.txt")).expect("upper persisted"),
        "from-upper\n"
    );
    assert_eq!(
        std::fs::symlink_metadata(mnt.join("new/dir/file.txt"))
            .expect("stat new")
            .permissions()
            .mode()
            & 0o777,
        0o640
    );

    // Append mode (O_APPEND) on a baseline file triggers copy-up and appends at EOF.
    std::fs::write(base.join("append.txt"), b"hello-").expect("write append base");
    let mut app_f = OpenOptions::new()
        .append(true)
        .open(mnt.join("append.txt"))
        .expect("open append");
    app_f.write_all(b"world").expect("append world");
    drop(app_f);
    assert_eq!(
        std::fs::read_to_string(mnt.join("append.txt")).expect("read append"),
        "hello-world"
    );
    assert_eq!(
        std::fs::read_to_string(base.join("append.txt")).expect("baseline append intact"),
        "hello-"
    );

    // Ensure upper_dir stays 100% pure user files (no whiteout/opaque artifacts).
    for entry in walkdir::WalkDir::new(&spec.upper_dir)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        let name = entry.file_name().to_string_lossy();
        assert!(!name.ends_with(".oz_wh") && !name.ends_with(".oz_opaque"));
    }
}

#[test]
fn live_fuse_unlink_rmdir_opaque_rename_link_readdir_and_open_unlinked() {
    let dir = TempDir::new().expect("tempdir");
    let spec = make_spec(dir.path(), "ws-ops", true, true);
    let base = spec.baseline_dir.as_ref().expect("base");
    let seed = spec.resolved_seed_dir().expect("seed");

    std::fs::create_dir_all(base.join("pkg")).expect("mkdir base pkg");
    std::fs::write(base.join("pkg/a.txt"), b"a").expect("write a");
    std::fs::write(base.join("pkg/b.txt"), b"b").expect("write b");
    std::fs::write(base.join("to_unlink.txt"), b"gone").expect("write unlink");
    std::fs::write(base.join("file_to_dir"), b"was_file").expect("write file_to_dir");
    std::fs::create_dir_all(base.join("mv_dir/sub")).expect("mkdir mv_dir");
    std::fs::write(base.join("mv_dir/x.txt"), b"x").expect("write x");
    std::fs::write(base.join("mv_dir/sub/y.txt"), b"y").expect("write y");
    std::fs::write(base.join("hard_src.txt"), b"hard-content").expect("write hard_src");

    // Large directory to exercise multi-chunk readdir offsets.
    std::fs::create_dir_all(base.join("many")).expect("mkdir many");
    for i in 0..220 {
        std::fs::write(base.join(format!("many/item_{i:03}.txt")), b"1").expect("write many");
    }

    std::fs::create_dir_all(seed.join("pkg")).expect("mkdir seed pkg");
    std::fs::write(seed.join("pkg/c.txt"), b"c").expect("write c");

    let _guard = OverlayGuard::new(&spec);
    let mnt = &spec.mount_point;

    // Merged readdir deduplicates across layers.
    let mut pkg_names: Vec<String> = std::fs::read_dir(mnt.join("pkg"))
        .expect("readdir pkg")
        .map(|e| e.expect("ent").file_name().to_string_lossy().into_owned())
        .collect();
    pkg_names.sort();
    assert_eq!(pkg_names, vec!["a.txt", "b.txt", "c.txt"]);

    // Many-entry readdir pagination.
    let many_count = std::fs::read_dir(mnt.join("many")).expect("readdir many").count();
    assert_eq!(many_count, 220);

    // Unlink creates immediate whiteout without modifying baseline.
    std::fs::remove_file(mnt.join("to_unlink.txt")).expect("unlink");
    assert!(!mnt.join("to_unlink.txt").exists());
    assert!(spec.meta_dir.join("whiteouts/to_unlink.txt.oz_wh").is_file());
    assert!(base.join("to_unlink.txt").is_file());

    // Unlink file then mkdir same name creates opaque directory.
    std::fs::remove_file(mnt.join("file_to_dir")).expect("unlink file_to_dir");
    std::fs::create_dir(mnt.join("file_to_dir")).expect("mkdir file_to_dir");
    assert!(mnt.join("file_to_dir").is_dir());
    assert!(!spec.meta_dir.join("whiteouts/file_to_dir.oz_wh").exists());
    assert!(spec.meta_dir.join("opaque/file_to_dir.oz_opaque").is_file());

    // rmdir non-empty fails; deleting children then rmdir creates dir whiteout; mkdir creates opaque dir.
    assert!(std::fs::remove_dir(mnt.join("pkg")).is_err());
    std::fs::remove_dir_all(mnt.join("pkg")).expect("rm -rf pkg");
    assert!(!mnt.join("pkg").exists());
    assert!(spec.meta_dir.join("whiteouts/pkg.oz_wh").is_file());
    assert!(base.join("pkg/a.txt").is_file());

    std::fs::create_dir(mnt.join("pkg")).expect("mkdir pkg again");
    assert!(spec.meta_dir.join("opaque/pkg.oz_opaque").is_file());
    assert!(!spec.meta_dir.join("whiteouts/pkg.oz_wh").exists());
    let recreated_entries: Vec<_> = std::fs::read_dir(mnt.join("pkg"))
        .expect("readdir recreated pkg")
        .collect();
    assert!(recreated_entries.is_empty());

    std::fs::write(mnt.join("pkg/fresh.txt"), b"fresh").expect("write fresh");
    std::fs::rename(mnt.join("pkg/fresh.txt"), mnt.join("renamed_fresh.txt")).expect("rename file");
    assert!(!mnt.join("pkg/fresh.txt").exists());
    assert_eq!(
        std::fs::read_to_string(mnt.join("renamed_fresh.txt")).expect("read renamed"),
        "fresh"
    );

    // Rename directory containing both baseline and upper files.
    std::fs::write(mnt.join("mv_dir/z.txt"), b"z").expect("write z");
    std::fs::rename(mnt.join("mv_dir"), mnt.join("moved_dir")).expect("rename dir");
    assert!(!mnt.join("mv_dir").exists());
    assert_eq!(
        std::fs::read_to_string(mnt.join("moved_dir/x.txt")).expect("read moved x"),
        "x"
    );
    assert_eq!(
        std::fs::read_to_string(mnt.join("moved_dir/sub/y.txt")).expect("read moved y"),
        "y"
    );
    assert_eq!(
        std::fs::read_to_string(mnt.join("moved_dir/z.txt")).expect("read moved z"),
        "z"
    );
    assert!(base.join("mv_dir/x.txt").is_file());

    // Symlink & hardlink copy-up.
    std::os::unix::fs::symlink("renamed_fresh.txt", mnt.join("sym.txt")).expect("symlink");
    assert_eq!(
        std::fs::read_link(mnt.join("sym.txt")).expect("readlink"),
        PathBuf::from("renamed_fresh.txt")
    );
    std::fs::hard_link(mnt.join("hard_src.txt"), mnt.join("hard_dst.txt")).expect("hard_link");
    std::fs::write(mnt.join("hard_dst.txt"), b"updated-via-link").expect("write hardlink");
    assert_eq!(
        std::fs::read_to_string(mnt.join("hard_src.txt")).expect("read via hard_src"),
        "updated-via-link"
    );
    assert_eq!(
        std::fs::read_to_string(base.join("hard_src.txt")).expect("baseline hard_src intact"),
        "hard-content"
    );

    // statfs + lchown (current uid/gid) + rename overwriting existing target.
    let c_mnt = std::ffi::CString::new(mnt.to_string_lossy().as_bytes()).expect("cstr");
    let mut svfs: libc::statvfs = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::statvfs(c_mnt.as_ptr(), &mut svfs) }, 0);
    assert!(svfs.f_namemax > 0 && svfs.f_bsize > 0);

    std::os::unix::fs::lchown(
        mnt.join("renamed_fresh.txt"),
        Some(unsafe { libc::geteuid() }),
        Some(unsafe { libc::getegid() }),
    )
    .expect("lchown");

    std::fs::write(mnt.join("overwrite_src.txt"), b"new-body").expect("write src");
    std::fs::rename(mnt.join("overwrite_src.txt"), mnt.join("renamed_fresh.txt"))
        .expect("rename overwrite");
    assert_eq!(
        std::fs::read_to_string(mnt.join("renamed_fresh.txt")).expect("read overwritten"),
        "new-body"
    );

    // Open-while-unlinked file keeps working via open FileHandle.
    let mut temp_f = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(mnt.join("ephemeral.txt"))
        .expect("create ephemeral");
    std::fs::remove_file(mnt.join("ephemeral.txt")).expect("unlink while open");
    assert!(!mnt.join("ephemeral.txt").exists());
    temp_f.write_all(b"still-alive").expect("write unlinked");
    temp_f.sync_data().expect("fsync unlinked");
    temp_f.seek(SeekFrom::Start(0)).expect("seek");
    let mut read_back = String::new();
    temp_f
        .read_to_string(&mut read_back)
        .expect("read unlinked");
    assert_eq!(read_back, "still-alive");
    assert_eq!(temp_f.metadata().expect("fstat unlinked").len(), 11);
}

#[test]
fn live_fuse_host_side_undo_restores_are_visible_immediately() {
    let dir = TempDir::new().expect("tempdir");
    let spec = make_spec(dir.path(), "ws-ttl", false, true);
    let base = spec.baseline_dir.as_ref().expect("base");
    std::fs::write(base.join("doc.txt"), b"v1").expect("write v1");

    let _guard = OverlayGuard::new(&spec);
    let mnt = &spec.mount_point;

    assert_eq!(
        std::fs::read_to_string(mnt.join("doc.txt")).expect("read v1"),
        "v1"
    );
    std::fs::write(mnt.join("doc.txt"), b"v2-longer").expect("write v2");
    assert_eq!(
        std::fs::read_to_string(mnt.join("doc.txt")).expect("read v2"),
        "v2-longer"
    );

    // Host-side write directly into upper_dir (simulating `oz undo`) must appear immediately.
    std::fs::write(spec.upper_dir.join("doc.txt"), b"v1_undo").expect("host write");
    assert_eq!(
        std::fs::read_to_string(mnt.join("doc.txt")).expect("read host undo"),
        "v1_undo"
    );

    // Host-side whiteout creation directly in meta_dir/whiteouts must hide the file immediately.
    std::fs::remove_file(spec.upper_dir.join("doc.txt")).expect("host rm upper");
    std::fs::write(spec.meta_dir.join("whiteouts/doc.txt.oz_wh"), b"").expect("host wh");
    assert!(!mnt.join("doc.txt").exists());
}

fn init_real_git_repo(repo_root: &Path) -> String {
    std::fs::create_dir_all(repo_root).expect("mkdir repo");
    assert!(
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(repo_root)
            .status()
            .expect("git init")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "user.name", "Ozone Test"])
            .current_dir(repo_root)
            .status()
            .expect("git config name")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "user.email", "test@ozone.dev"])
            .current_dir(repo_root)
            .status()
            .expect("git config email")
            .success()
    );
    std::fs::write(repo_root.join("README.md"), "# repo\n").expect("write readme");
    assert!(
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(repo_root)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(repo_root)
            .status()
            .expect("git commit")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["branch", "feat/ws1"])
            .current_dir(repo_root)
            .status()
            .expect("git branch ws1")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["branch", "feat/ws2"])
            .current_dir(repo_root)
            .status()
            .expect("git branch ws2")
            .success()
    );
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .expect("rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn live_daemon_multi_mount_dirty_paths_and_sigkill_recovery() {
    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("repo");
    let head_oid = init_real_git_repo(&repo_root);
    let git_dir = repo_root.join(".git");
    let layout = OzoneLayout::from_git_common_dir(&git_dir);
    layout.ensure_initialized().expect("init ozone layout");

    let mut spec1 = make_spec(dir.path(), "ws1", true, true);
    spec1.base_commit = Some(head_oid.clone());
    let mut spec2 = make_spec(dir.path(), "ws2", false, true);
    spec2.base_commit = Some(head_oid);

    let base1 = spec1.baseline_dir.as_ref().expect("base1");
    std::fs::write(base1.join("existing.txt"), b"initial").expect("write existing");
    std::fs::write(base1.join("to_delete.txt"), b"remove-me").expect("write to_delete");
    std::fs::create_dir_all(base1.join("sub")).expect("mkdir sub");
    std::fs::write(base1.join("sub/file.txt"), b"sub-file").expect("write sub");

    let base2 = spec2.baseline_dir.as_ref().expect("base2");
    std::fs::write(base2.join("ws2.txt"), b"from-ws2-base").expect("write ws2");

    let mut guard = DaemonGuard::new();
    guard.track(
        git_dir.clone(),
        vec![spec1.mount_point.clone(), spec2.mount_point.clone()],
    );

    let info1: MountInfo = mount_via_daemon(&git_dir, &spec1).expect("mount ws1");
    let info2: MountInfo = mount_via_daemon(&git_dir, &spec2).expect("mount ws2");
    assert!(info1.mounted && info2.mounted);

    let sock = socket_path_for_git_dir(&git_dir);
    let first_pong = ping_daemon(&sock).expect("ping live daemon");
    guard.track_pid(first_pong.pid);

    match send_request(&sock, &VfsRequest::Status).expect("daemon status") {
        VfsResponse::DaemonStatus {
            pid, active_mounts, ..
        } => {
            assert_eq!(pid, first_pong.pid);
            assert_eq!(active_mounts.len(), 2);
        }
        other => panic!("unexpected Status response: {other:?}"),
    }

    match send_request(&sock, &VfsRequest::ListMounts).expect("list mounts") {
        VfsResponse::MountList { mounts } => {
            assert_eq!(mounts.len(), 2);
            assert!(mounts.iter().any(|m| m.workspace == "ws1" && m.mounted));
            assert!(mounts.iter().any(|m| m.workspace == "ws2" && m.mounted));
        }
        other => panic!("unexpected ListMounts response: {other:?}"),
    }

    assert_eq!(
        std::fs::read_to_string(spec2.mount_point.join("ws2.txt")).expect("read ws2"),
        "from-ws2-base"
    );

    // Mid-edit mutations in ws1 (including an unclosed open file written directly via write(2)).
    std::fs::write(
        spec1.mount_point.join("existing.txt"),
        b"edited-before-sigkill",
    )
    .expect("edit existing");
    std::fs::write(
        spec1.mount_point.join("new_file.rs"),
        b"pub fn durable() -> u32 { 42 }\n",
    )
    .expect("create new_file.rs");
    std::fs::remove_file(spec1.mount_point.join("to_delete.txt")).expect("unlink to_delete");

    let mut open_mid = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(spec1.mount_point.join("open_mid_edit.txt"))
        .expect("open mid edit");
    open_mid.write_all(b"chunk1-").expect("write chunk1");
    open_mid.write_all(b"chunk2").expect("write chunk2");

    let dirty_before = dirty_paths_via_daemon(&git_dir, "ws1").expect("dirty before sigkill");
    let dirty_summary: Vec<(String, ChangeKind)> =
        dirty_before.into_iter().map(|d| (d.path, d.kind)).collect();
    assert_eq!(
        dirty_summary,
        vec![
            ("existing.txt".to_string(), ChangeKind::Modified),
            ("new_file.rs".to_string(), ChangeKind::Added),
            ("open_mid_edit.txt".to_string(), ChangeKind::Added),
            ("to_delete.txt".to_string(), ChangeKind::Deleted),
        ]
    );

    // SIGKILL oz-vfsd directly while open_mid is still open!
    let kill_rc = unsafe { libc::kill(first_pong.pid as libc::pid_t, libc::SIGKILL) };
    assert_eq!(kill_rc, 0, "SIGKILL failed");
    let mut status: libc::c_int = 0;
    let waited = unsafe { libc::waitpid(first_pong.pid as libc::pid_t, &mut status, 0) };
    assert_eq!(waited, first_pong.pid as libc::pid_t);
    assert!(libc::WIFSIGNALED(status));

    drop(open_mid);

    // Confirm dead FUSE mount returns ENOTCONN and OzoneRepo::discover still succeeds cleanly.
    let stat_err = std::fs::symlink_metadata(&spec1.mount_point).expect_err("expect ENOTCONN");
    assert!(
        is_enotconn(&stat_err),
        "expected ENOTCONN after SIGKILL, got {stat_err:?}"
    );
    let discovered =
        OzoneRepo::discover(Some(&spec1.mount_point.join("sub"))).expect("discover across ENOTCONN");
    assert_eq!(discovered.mount_health, MountHealth::DisconnectedEnotconn);
    assert_eq!(discovered.workspace.as_deref(), Some("ws1"));

    // Auto-recover daemon & remount both workspaces transparently.
    let recovered = ensure_daemon_and_mounts(&git_dir).expect("recover daemon and mounts");
    assert_eq!(recovered.len(), 2);

    let second_pong = ping_daemon(&sock).expect("ping recovered daemon");
    assert_ne!(second_pong.pid, first_pong.pid);
    guard.track_pid(second_pong.pid);

    // Zero lost edits after SIGKILL!
    assert_eq!(
        std::fs::read_to_string(spec1.mount_point.join("existing.txt")).expect("read existing"),
        "edited-before-sigkill"
    );
    assert_eq!(
        std::fs::read_to_string(spec1.mount_point.join("new_file.rs")).expect("read new_file"),
        "pub fn durable() -> u32 { 42 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(spec1.mount_point.join("open_mid_edit.txt")).expect("read mid"),
        "chunk1-chunk2"
    );
    assert!(!spec1.mount_point.join("to_delete.txt").exists());
    assert_eq!(
        std::fs::read_to_string(base1.join("existing.txt")).expect("baseline intact"),
        "initial"
    );
    assert_eq!(
        std::fs::read_to_string(spec2.mount_point.join("ws2.txt")).expect("ws2 recovered"),
        "from-ws2-base"
    );

    let dirty_after = dirty_paths_via_daemon(&git_dir, "ws1").expect("dirty after sigkill");
    assert_eq!(dirty_after.len(), 4);

    // SIGTERM cleanly unmounts while keeping mount.json mounted: true, so re-running
    // ensure_daemon_and_mounts restores active workspaces again.
    let term_rc = unsafe { libc::kill(second_pong.pid as libc::pid_t, libc::SIGTERM) };
    assert_eq!(term_rc, 0, "SIGTERM failed");
    let mut term_status: libc::c_int = 0;
    let waited_term = unsafe { libc::waitpid(second_pong.pid as libc::pid_t, &mut term_status, 0) };
    assert_eq!(waited_term, second_pong.pid as libc::pid_t);
    assert!(libc::WIFEXITED(term_status) && libc::WEXITSTATUS(term_status) == 0);

    let restored_after_term = ensure_daemon_and_mounts(&git_dir).expect("restore after SIGTERM");
    assert_eq!(restored_after_term.len(), 2);
    let third_pong = ping_daemon(&sock).expect("ping after SIGTERM restore");
    guard.track_pid(third_pong.pid);
    assert_eq!(
        std::fs::read_to_string(spec1.mount_point.join("open_mid_edit.txt")).expect("read"),
        "chunk1-chunk2"
    );

    // Clean unmount ws2 and verify mount.json updated to mounted: false.
    unmount_via_daemon(&git_dir, "ws2", false).expect("unmount ws2");
    let rec2: MountRecord = serde_json::from_str(
        &std::fs::read_to_string(layout.ws_mount_record_path("ws2")).expect("read mount.json"),
    )
    .expect("parse mount.json");
    assert!(!rec2.mounted);

    shutdown_daemon(&git_dir).expect("shutdown daemon");
    let mut status2: libc::c_int = 0;
    let _ = unsafe { libc::waitpid(third_pong.pid as libc::pid_t, &mut status2, 0) };
    // Verify ws1 mountpoint is cleanly unmounted (not ENOTCONN).
    assert!(std::fs::symlink_metadata(&spec1.mount_point).is_ok());
}
