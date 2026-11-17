// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Synchronous IPC client, auto-spawn, and crash recovery for `oz-vfsd`.

use crate::lookup::find_oz_vfsd_bin;
use crate::overlay::lazy_unmount_fuse;
use crate::protocol::{DirtyPathEntry, MountInfo, MountSpec, VfsRequest, VfsResponse, encode_line};
use oz_core::{MountRecord, OzError, OzoneLayout, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Response payload returned by [`ping_daemon`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPong {
    pub version: String,
    pub pid: u32,
}

/// Returns the canonical IPC endpoint path `.git/ozone/vfsd.sock` for `git_dir`.
#[must_use]
pub fn socket_path_for_git_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("ozone").join("vfsd.sock")
}

fn exchange_ipc_line<S: std::io::Read + std::io::Write>(
    mut stream: S,
    socket_path: &Path,
    req: &VfsRequest,
) -> Result<VfsResponse> {
    use std::io::{BufRead, BufReader};

    let encoded = encode_line(req)?;
    stream.write_all(encoded.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Err(OzError::Other(format!(
            "oz-vfsd closed IPC socket {} without responding",
            socket_path.display()
        )));
    }
    let resp: VfsResponse = serde_json::from_str(line.trim())?;
    Ok(resp)
}

/// Sends a single [`VfsRequest`] over loopback TCP using the `"tcp://127.0.0.1:<port>"`
/// endpoint stored inside `socket_path`.
pub fn send_tcp_request(socket_path: &Path, req: &VfsRequest) -> Result<VfsResponse> {
    let raw = std::fs::read_to_string(socket_path)?;
    let addr_raw = raw.trim().strip_prefix("tcp://").ok_or_else(|| {
        OzError::Other(format!(
            "invalid oz-vfsd TCP endpoint in {} (expected 'tcp://127.0.0.1:<port>')",
            socket_path.display()
        ))
    })?;
    let addr: std::net::SocketAddr = addr_raw.parse().map_err(|err| {
        OzError::Other(format!(
            "invalid oz-vfsd TCP socket address '{addr_raw}' in {}: {err}",
            socket_path.display()
        ))
    })?;
    if !addr.ip().is_loopback() {
        return Err(OzError::Other(format!(
            "refusing non-loopback oz-vfsd IPC address {addr} in {}",
            socket_path.display()
        )));
    }

    let stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    let timeout = Some(Duration::from_secs(10));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    exchange_ipc_line(stream, socket_path, req)
}

/// Sends a single [`VfsRequest`] over `socket_path` and parses the JSON line [`VfsResponse`].
#[cfg(unix)]
pub fn send_request(socket_path: &Path, req: &VfsRequest) -> Result<VfsResponse> {
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(socket_path)?;
    let timeout = Some(Duration::from_secs(10));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    exchange_ipc_line(stream, socket_path, req)
}

/// Sends a single [`VfsRequest`] over loopback TCP using the endpoint stored in `socket_path`.
#[cfg(windows)]
pub fn send_request(socket_path: &Path, req: &VfsRequest) -> Result<VfsResponse> {
    send_tcp_request(socket_path, req)
}

/// Unsupported platform compile-time stub for [`send_request`].
#[cfg(not(any(unix, windows)))]
pub fn send_request(_socket_path: &Path, _req: &VfsRequest) -> Result<VfsResponse> {
    Err(OzError::Other(
        "oz-vfsd IPC client is only supported on Unix and Windows".to_string(),
    ))
}

/// Pings a running `oz-vfsd` instance at `socket_path`.
pub fn ping_daemon(socket_path: &Path) -> Result<DaemonPong> {
    match send_request(socket_path, &VfsRequest::Ping)? {
        VfsResponse::Pong { version, pid } => Ok(DaemonPong { version, pid }),
        VfsResponse::Error { code, message } => {
            Err(OzError::Other(format!("oz-vfsd ping error ({code}): {message}")))
        }
        other => Err(OzError::Other(format!(
            "unexpected oz-vfsd response to Ping: {other:?}"
        ))),
    }
}

/// Ensures `oz-vfsd` is running for `git_dir` and all `mounted: true` daemon-backed workspaces
/// from `<git_dir>/ozone/ws/*/mount.json` are healthy and mounted.
///
/// On Unix, cleans up dead `vfsd.sock` sockets and `ENOTCONN` (`errno == 107`) stale FUSE
/// mountpoints via `fusermount3 -u -z`, spawns `oz-vfsd --git-dir <git_dir>` detached with
/// `setsid()`. On Windows, spawns `oz-vfsd.exe` detached (`CREATE_NO_WINDOW | DETACHED_PROCESS`)
/// listening on loopback TCP and virtualizing ProjFS mountpoints.
#[cfg(any(unix, windows))]
pub fn ensure_daemon_and_mounts(git_dir: &Path) -> Result<Vec<MountInfo>> {
    use fs4::FileExt as Fs4FileExt;
    use std::fs::OpenOptions;
    use std::process::{Command, Stdio};

    let layout = OzoneLayout::from_git_common_dir(git_dir);
    std::fs::create_dir_all(&layout.ozone_dir)?;
    std::fs::create_dir_all(layout.ws_dir())?;
    let sock = socket_path_for_git_dir(git_dir);
    let records = load_expected_daemon_mounts(&layout);

    if ping_daemon(&sock).is_ok() {
        return verify_or_repair_live_mounts(git_dir, &sock, &records);
    }

    // Serialize concurrent auto-spawn attempts across processes.
    let spawn_lock_path = layout.ozone_dir.join("vfsd.spawn.lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&spawn_lock_path)?;
    Fs4FileExt::lock(&lock_file)?;

    // Re-check once lock is held in case a peer process finished spawning.
    if ping_daemon(&sock).is_ok() {
        return verify_or_repair_live_mounts(git_dir, &sock, &records);
    }

    for record in &records {
        if needs_stale_unmount(&record.mount_point) {
            lazy_unmount_fuse(&record.mount_point)?;
        }
    }

    if sock.exists() {
        let _ = std::fs::remove_file(&sock);
    }

    let vfsd_bin = find_oz_vfsd_bin()?;
    let log_path = layout.ozone_dir.join("vfsd.log");
    let stdout_stdio = open_daemon_log_stdio(&log_path);
    let stderr_stdio = open_daemon_log_stdio(&log_path);

    let mut cmd = Command::new(&vfsd_bin);
    cmd.arg("--git-dir")
        .arg(git_dir)
        .arg("--socket")
        .arg(&sock)
        .stdin(Stdio::null())
        .stdout(stdout_stdio)
        .stderr(stderr_stdio);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS (0x0000_0008) | CREATE_NO_WINDOW (0x0800_0000)
        cmd.creation_flags(0x0800_0008);
    }

    let mut child = cmd.spawn().map_err(|err| {
        OzError::Other(format!(
            "failed to spawn oz-vfsd ({}): {err}",
            vfsd_bin.display()
        ))
    })?;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            // Another daemon might have raced just before lock, or daemon crashed on startup.
            if ping_daemon(&sock).is_ok() {
                break;
            }
            let log_tail = read_log_tail(&log_path);
            return Err(OzError::Other(format!(
                "oz-vfsd exited prematurely with status {status}{log_tail}"
            )));
        }

        if ping_daemon(&sock).is_ok()
            && all_mount_points_stat_healthy(&records)
            && daemon_lists_all_mounts(&sock, &records)
        {
            return list_mounts_via_socket(&sock);
        }

        if Instant::now() >= deadline {
            let log_tail = read_log_tail(&log_path);
            return Err(OzError::Other(format!(
                "timed out waiting for oz-vfsd socket {} and mount points to become healthy{log_tail}",
                sock.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    verify_or_repair_live_mounts(git_dir, &sock, &records)
}

/// Unsupported platform compile-time stub for [`ensure_daemon_and_mounts`].
#[cfg(not(any(unix, windows)))]
pub fn ensure_daemon_and_mounts(_git_dir: &Path) -> Result<Vec<MountInfo>> {
    Err(OzError::Other(
        "oz-vfsd auto-spawn is only supported on Unix and Windows".to_string(),
    ))
}

/// Mounts a workspace through `oz-vfsd` (auto-spawning the daemon if needed) and verifies
/// that `spec.mount_point` passes `stat()`.
pub fn mount_via_daemon(git_dir: &Path, spec: &MountSpec) -> Result<MountInfo> {
    let _ = ensure_daemon_and_mounts(git_dir)?;
    if let Err(err) = std::fs::symlink_metadata(&spec.mount_point)
        && oz_core::is_enotconn(&err)
    {
        lazy_unmount_fuse(&spec.mount_point)?;
    }

    let sock = socket_path_for_git_dir(git_dir);
    let resp = send_request(
        &sock,
        &VfsRequest::Mount {
            spec: Box::new(spec.clone()),
        },
    )?;
    let info = match resp {
        VfsResponse::Mounted { mount } => mount,
        VfsResponse::Error { code, message } => {
            return Err(OzError::MountUnavailable {
                workspace: spec.workspace.clone(),
                path: spec.mount_point.clone(),
                errno: None,
                message: format!("oz-vfsd mount error ({code}): {message}"),
            });
        }
        other => {
            return Err(OzError::Other(format!(
                "unexpected oz-vfsd response to Mount: {other:?}"
            )));
        }
    };

    // Confirm mountpoint responds to stat() before returning.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::symlink_metadata(&spec.mount_point).is_ok() {
            return Ok(info);
        }
        if Instant::now() >= deadline {
            return Err(OzError::MountUnavailable {
                workspace: spec.workspace.clone(),
                path: spec.mount_point.clone(),
                errno: std::fs::symlink_metadata(&spec.mount_point)
                    .err()
                    .and_then(|e| e.raw_os_error()),
                message: "mountpoint failed stat() after oz-vfsd Mount".to_string(),
            });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Unmounts `workspace` via `oz-vfsd` (or falls back to `fusermount3 -u -z` if the daemon
/// is dead or `force` is requested).
pub fn unmount_via_daemon(git_dir: &Path, workspace: &str, force: bool) -> Result<()> {
    let sock = socket_path_for_git_dir(git_dir);
    if ping_daemon(&sock).is_ok() {
        match send_request(
            &sock,
            &VfsRequest::Unmount {
                workspace: workspace.to_string(),
                force,
            },
        )? {
            VfsResponse::Unmounted { .. } => return Ok(()),
            VfsResponse::Error { code, message } if !force => {
                return Err(OzError::Other(format!(
                    "oz-vfsd unmount error ({code}): {message}"
                )));
            }
            _ => {}
        }
    }

    let layout = OzoneLayout::from_git_common_dir(git_dir);
    let mount_json = layout.ws_mount_record_path(workspace);
    if let Ok(raw) = std::fs::read_to_string(&mount_json)
        && let Ok(mut record) = serde_json::from_str::<MountRecord>(&raw)
    {
        let _ = lazy_unmount_fuse(&record.mount_point);
        record.mounted = false;
        record.daemon_pid = None;
        record.updated_at = chrono::Utc::now();
        if let Ok(bytes) = serde_json::to_vec_pretty(&record) {
            let _ = std::fs::write(&mount_json, bytes);
        }
    }
    Ok(())
}

/// Queries `oz-vfsd` for `O(dirty_files)` dirty paths in `workspace` (auto-recovering
/// the daemon and mounts first if necessary).
pub fn dirty_paths_via_daemon(git_dir: &Path, workspace: &str) -> Result<Vec<DirtyPathEntry>> {
    let _ = ensure_daemon_and_mounts(git_dir)?;
    let sock = socket_path_for_git_dir(git_dir);
    match send_request(
        &sock,
        &VfsRequest::DirtyPaths {
            workspace: workspace.to_string(),
        },
    )? {
        VfsResponse::DirtySet { paths, .. } => Ok(paths),
        VfsResponse::Error { code, message } => Err(OzError::Other(format!(
            "oz-vfsd dirty_paths error ({code}): {message}"
        ))),
        other => Err(OzError::Other(format!(
            "unexpected oz-vfsd response to DirtyPaths: {other:?}"
        ))),
    }
}

/// Requests a clean daemon shutdown over IPC if the daemon is currently reachable.
pub fn shutdown_daemon(git_dir: &Path) -> Result<()> {
    let sock = socket_path_for_git_dir(git_dir);
    if ping_daemon(&sock).is_err() {
        return Ok(());
    }
    let _ = send_request(&sock, &VfsRequest::Shutdown);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !sock.exists() && ping_daemon(&sock).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn verify_or_repair_live_mounts(
    git_dir: &Path,
    sock: &Path,
    records: &[MountRecord],
) -> Result<Vec<MountInfo>> {
    let current = list_mounts_via_socket(sock)?;
    for record in records {
        let is_listed = current.iter().any(|m| m.workspace == record.workspace && m.mounted);
        let is_healthy = std::fs::symlink_metadata(&record.mount_point).is_ok();
        if !is_listed || !is_healthy {
            if needs_stale_unmount(&record.mount_point) {
                lazy_unmount_fuse(&record.mount_point)?;
            }
            let ws_dir = git_dir.join("ozone").join("ws").join(&record.workspace);
            let spec = crate::server::load_or_reconstruct_spec(git_dir, &ws_dir, record);
            let _ = send_request(
                sock,
                &VfsRequest::Mount {
                    spec: Box::new(spec),
                },
            )?;
        }
    }
    list_mounts_via_socket(sock)
}

#[cfg(any(unix, windows))]
fn list_mounts_via_socket(sock: &Path) -> Result<Vec<MountInfo>> {
    match send_request(sock, &VfsRequest::ListMounts)? {
        VfsResponse::MountList { mounts } => Ok(mounts),
        VfsResponse::Error { code, message } => Err(OzError::Other(format!(
            "oz-vfsd list_mounts error ({code}): {message}"
        ))),
        other => Err(OzError::Other(format!(
            "unexpected oz-vfsd response to ListMounts: {other:?}"
        ))),
    }
}

#[cfg(any(unix, windows))]
fn load_expected_daemon_mounts(layout: &OzoneLayout) -> Vec<MountRecord> {
    let mut records = Vec::new();
    let Ok(entries) = std::fs::read_dir(layout.ws_dir()) else {
        return records;
    };
    for entry in entries.flatten() {
        let ws_dir = entry.path();
        if !ws_dir.is_dir() {
            continue;
        }
        let mount_json = ws_dir.join("mount.json");
        let Ok(raw) = std::fs::read_to_string(&mount_json) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<MountRecord>(&raw) else {
            continue;
        };
        if record.mounted && record.backend.requires_daemon() {
            records.push(record);
        }
    }
    records
}

#[cfg(unix)]
fn needs_stale_unmount(mount_point: &Path) -> bool {
    match std::fs::symlink_metadata(mount_point) {
        Err(err) if oz_core::is_enotconn(&err) => true,
        _ => is_fuse_mount_in_proc_mountinfo(mount_point),
    }
}

#[cfg(not(unix))]
fn needs_stale_unmount(_mount_point: &Path) -> bool {
    false
}

#[cfg(unix)]
fn is_fuse_mount_in_proc_mountinfo(mount_point: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let target = mount_point.to_string_lossy();
    raw.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let Some(mp) = fields.nth(4) else {
            return false;
        };
        if mp != target {
            return false;
        }
        let Some((_, after_sep)) = line.split_once(" - ") else {
            return false;
        };
        after_sep.starts_with("fuse")
    })
}

#[cfg(any(unix, windows))]
fn all_mount_points_stat_healthy(records: &[MountRecord]) -> bool {
    records
        .iter()
        .all(|r| std::fs::symlink_metadata(&r.mount_point).is_ok())
}

#[cfg(any(unix, windows))]
fn daemon_lists_all_mounts(sock: &Path, records: &[MountRecord]) -> bool {
    if records.is_empty() {
        return true;
    }
    let Ok(mounts) = list_mounts_via_socket(sock) else {
        return false;
    };
    records
        .iter()
        .all(|r| mounts.iter().any(|m| m.workspace == r.workspace && m.mounted))
}

#[cfg(any(unix, windows))]
fn open_daemon_log_stdio(log_path: &Path) -> std::process::Stdio {
    use std::fs::OpenOptions;
    use std::process::Stdio;

    match OpenOptions::new().create(true).append(true).open(log_path) {
        Ok(file) => Stdio::from(file),
        Err(_) => Stdio::null(),
    }
}

#[cfg(any(unix, windows))]
fn read_log_tail(log_path: &Path) -> String {
    let Ok(raw) = std::fs::read_to_string(log_path) else {
        return String::new();
    };
    let tail: Vec<&str> = raw.lines().rev().take(5).collect();
    if tail.is_empty() {
        String::new()
    } else {
        let joined: Vec<&str> = tail.into_iter().rev().collect();
        format!(" (log: {})", joined.join(" | "))
    }
}
