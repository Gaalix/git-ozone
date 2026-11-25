// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Synchronous IPC client, auto-spawn, and crash recovery for `oz-vfsd`.

use crate::lookup::find_oz_vfsd_bin;
use crate::overlay::lazy_unmount_fuse;
use crate::protocol::{
    AuthenticatedRequest, DirtyPathEntry, MountInfo, MountSpec, VfsRequest, VfsResponse,
    encode_line, parse_tcp_endpoint,
};
use oz_core::{MountRecord, OzError, OzoneLayout, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Response payload returned by [`ping_daemon`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPong {
    pub version: String,
    pub pid: u32,
}

/// Environment variable overriding the user-wide `oz-vfsd` global socket path.
pub const OZ_VFSD_GLOBAL_SOCKET_ENV: &str = "OZ_VFSD_GLOBAL_SOCKET";

/// Returns the canonical IPC endpoint path for `git_dir` (`.git/ozone/vfsd.sock`, or a
/// short hashed Unix runtime socket path when `.git/ozone/vfsd.sock` would exceed `SUN_LEN`).
#[must_use]
pub fn socket_path_for_git_dir(git_dir: &Path) -> PathBuf {
    OzoneLayout::from_git_common_dir(git_dir).vfsd_socket_path()
}

/// Returns the user-wide global `oz-vfsd` Unix socket path (`$XDG_RUNTIME_DIR/oz-vfsd.sock`
/// or `/tmp/oz-vfsd-<uid>/vfsd.sock` on Unix; `None` on non-Unix unless `OZ_VFSD_GLOBAL_SOCKET` is set).
#[must_use]
pub fn global_daemon_socket_path() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os(OZ_VFSD_GLOBAL_SOCKET_ENV)
        && !raw.is_empty()
    {
        return Some(PathBuf::from(raw));
    }
    #[cfg(unix)]
    {
        if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR")
            && !xdg.is_empty()
        {
            return Some(PathBuf::from(xdg).join("oz-vfsd.sock"));
        }
        // SAFETY: `libc::geteuid()` is always safe to call and has no preconditions.
        let uid = unsafe { libc::geteuid() };
        Some(PathBuf::from(format!("/tmp/oz-vfsd-{uid}/vfsd.sock")))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Resolves the best active `oz-vfsd` socket for `git_dir`:
/// 1. Existing live per-repo socket (`socket_path_for_git_dir(git_dir)`).
/// 2. Existing live user-wide global daemon (`global_daemon_socket_path()`).
/// 3. Fallback to per-repo socket path (used when auto-spawning a local daemon).
#[must_use]
pub fn resolve_active_daemon_socket(git_dir: &Path) -> PathBuf {
    let repo_sock = socket_path_for_git_dir(git_dir);
    if ping_daemon(&repo_sock).is_ok() {
        return repo_sock;
    }
    if let Some(global_sock) = global_daemon_socket_path()
        && ping_daemon(&global_sock).is_ok()
    {
        return global_sock;
    }
    repo_sock
}

/// Returns `true` when either the user-wide global `oz-vfsd` daemon or the repository's
/// per-repo `oz-vfsd` daemon responds to `Ping`.
#[must_use]
pub fn is_any_daemon_running(git_dir: Option<&Path>) -> bool {
    if let Some(gd) = git_dir {
        let repo_sock = socket_path_for_git_dir(gd);
        if ping_daemon(&repo_sock).is_ok() {
            return true;
        }
    }
    if let Some(global_sock) = global_daemon_socket_path()
        && ping_daemon(&global_sock).is_ok()
    {
        return true;
    }
    false
}

#[cfg(unix)]
fn verify_socket_parent_owner_if_exists(socket_path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let Some(parent) = socket_path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    let meta = match std::fs::symlink_metadata(parent) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(OzError::Io(err)),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(OzError::GuardrailViolation {
            kind: "ipc_security".to_string(),
            message: format!(
                "refusing oz-vfsd IPC socket '{}': parent '{}' is not a real directory",
                socket_path.display(),
                parent.display()
            ),
            recovery: Vec::new(),
        });
    }
    // SAFETY: `libc::geteuid()` is always safe to call and has no preconditions.
    let my_uid = unsafe { libc::geteuid() };
    if meta.uid() != my_uid {
        return Err(OzError::GuardrailViolation {
            kind: "ipc_security".to_string(),
            message: format!(
                "refusing oz-vfsd IPC socket '{}': parent '{}' is owned by uid {} (expected {my_uid})",
                socket_path.display(),
                parent.display(),
                meta.uid()
            ),
            recovery: Vec::new(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn verify_socket_endpoint_owner(socket_path: &Path, is_tcp_file: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    // SAFETY: `libc::geteuid()` is always safe to call and has no preconditions.
    let my_uid = unsafe { libc::geteuid() };

    if let Some(parent) = socket_path.parent()
        && !parent.as_os_str().is_empty()
    {
        let parent_meta = std::fs::symlink_metadata(parent)?;
        if parent_meta.file_type().is_symlink() || !parent_meta.is_dir() {
            return Err(OzError::GuardrailViolation {
                kind: "ipc_security".to_string(),
                message: format!(
                    "refusing oz-vfsd IPC socket '{}': parent '{}' is not a real directory",
                    socket_path.display(),
                    parent.display()
                ),
                recovery: Vec::new(),
            });
        }
        if parent_meta.uid() != my_uid {
            return Err(OzError::GuardrailViolation {
                kind: "ipc_security".to_string(),
                message: format!(
                    "refusing oz-vfsd IPC socket '{}': parent '{}' is owned by uid {} (expected {my_uid})",
                    socket_path.display(),
                    parent.display(),
                    parent_meta.uid()
                ),
                recovery: Vec::new(),
            });
        }
    }

    let sock_meta = std::fs::symlink_metadata(socket_path)?;
    if sock_meta.file_type().is_symlink() {
        return Err(OzError::GuardrailViolation {
            kind: "ipc_security".to_string(),
            message: format!(
                "refusing oz-vfsd IPC endpoint '{}': symlink endpoints are not permitted",
                socket_path.display()
            ),
            recovery: Vec::new(),
        });
    }
    if is_tcp_file && !sock_meta.is_file() {
        return Err(OzError::GuardrailViolation {
            kind: "ipc_security".to_string(),
            message: format!(
                "refusing oz-vfsd TCP endpoint '{}': expected a regular file",
                socket_path.display()
            ),
            recovery: Vec::new(),
        });
    }
    if sock_meta.uid() != my_uid {
        return Err(OzError::GuardrailViolation {
            kind: "ipc_security".to_string(),
            message: format!(
                "refusing oz-vfsd IPC endpoint '{}': owned by uid {} (expected {my_uid})",
                socket_path.display(),
                sock_meta.uid()
            ),
            recovery: Vec::new(),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_socket_endpoint_owner(socket_path: &Path, is_tcp_file: bool) -> Result<()> {
    let sock_meta = std::fs::symlink_metadata(socket_path)?;
    if sock_meta.file_type().is_symlink() {
        return Err(OzError::GuardrailViolation {
            kind: "ipc_security".to_string(),
            message: format!(
                "refusing oz-vfsd IPC endpoint '{}': symlink endpoints are not permitted",
                socket_path.display()
            ),
            recovery: Vec::new(),
        });
    }
    if is_tcp_file && !sock_meta.is_file() {
        return Err(OzError::GuardrailViolation {
            kind: "ipc_security".to_string(),
            message: format!(
                "refusing oz-vfsd TCP endpoint '{}': expected a regular file",
                socket_path.display()
            ),
            recovery: Vec::new(),
        });
    }
    Ok(())
}

fn exchange_ipc_line<S: std::io::Read + std::io::Write>(
    mut stream: S,
    socket_path: &Path,
    req: &VfsRequest,
    auth_token: Option<&str>,
) -> Result<VfsResponse> {
    use std::io::{BufRead, BufReader};

    let encoded = match auth_token {
        Some(tok) => encode_line(&AuthenticatedRequest::with_token(
            req.clone(),
            tok.to_string(),
        ))?,
        None => encode_line(req)?,
    };
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

/// Sends a single authenticated [`VfsRequest`] over loopback TCP using the endpoint metadata
/// (`tcp://127.0.0.1:<port>\ntoken=<hex>\n`) stored inside `socket_path`.
pub fn send_tcp_request(socket_path: &Path, req: &VfsRequest) -> Result<VfsResponse> {
    verify_socket_endpoint_owner(socket_path, true)?;
    let raw = std::fs::read_to_string(socket_path)?;
    let endpoint = parse_tcp_endpoint(&raw)
        .map_err(|msg| OzError::Other(format!("{msg} (in {})", socket_path.display())))?;

    let stream = std::net::TcpStream::connect_timeout(&endpoint.addr, Duration::from_secs(2))?;
    let timeout = Some(Duration::from_secs(10));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    exchange_ipc_line(stream, socket_path, req, Some(&endpoint.auth_token))
}

/// Sends a single [`VfsRequest`] over `socket_path` and parses the JSON line [`VfsResponse`].
#[cfg(unix)]
pub fn send_request(socket_path: &Path, req: &VfsRequest) -> Result<VfsResponse> {
    use std::os::unix::net::UnixStream;

    verify_socket_endpoint_owner(socket_path, false)?;
    let stream = UnixStream::connect(socket_path)?;
    let timeout = Some(Duration::from_secs(10));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    exchange_ipc_line(stream, socket_path, req, None)
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
        VfsResponse::Error { code, message } => Err(OzError::Other(format!(
            "oz-vfsd ping error ({code}): {message}"
        ))),
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
    let sock = resolve_active_daemon_socket(git_dir);
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

    #[cfg(unix)]
    verify_socket_parent_owner_if_exists(&sock)?;

    if std::fs::symlink_metadata(&sock).is_ok() {
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
        // SAFETY: The `pre_exec` closure captures no state, performs no heap allocations,
        // and calls only `libc::setsid()` (which is async-signal-safe per POSIX.1).
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

    #[cfg(windows)]
    let no_inherit_guard = win_no_inherit::StdioNoInheritGuard::acquire();
    let spawn_res = cmd.spawn();
    #[cfg(windows)]
    drop(no_inherit_guard);
    let mut child = spawn_res.map_err(|err| {
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

        // Once `ping_daemon` succeeds, `restore_mounts_on_startup()` in `oz-vfsd` has already
        // completed (the IPC listener binds only after startup restore finishes). Break immediately
        // so `verify_or_repair_live_mounts` either returns the live mounts or surfaces any real
        // mount failure without waiting out the 10-second startup deadline.
        if ping_daemon(&sock).is_ok() {
            break;
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
    mount_via_daemon_with_auto_start(git_dir, spec, true)
}

/// Mounts a workspace through `oz-vfsd`, controlling whether the daemon is auto-started when
/// not already listening, and verifies that `spec.mount_point` passes `stat()`.
pub fn mount_via_daemon_with_auto_start(
    git_dir: &Path,
    spec: &MountSpec,
    auto_start: bool,
) -> Result<MountInfo> {
    let mut sock = resolve_active_daemon_socket(git_dir);
    if ping_daemon(&sock).is_err() {
        if auto_start {
            let _ = ensure_daemon_and_mounts(git_dir)?;
            sock = resolve_active_daemon_socket(git_dir);
        } else {
            return Err(OzError::MountUnavailable {
                workspace: spec.workspace.clone(),
                path: spec.mount_point.clone(),
                errno: None,
                message: format!(
                    "oz-vfsd socket {} is not listening and daemon.auto_start is disabled",
                    sock.display()
                ),
            });
        }
    }
    if let Err(err) = std::fs::symlink_metadata(&spec.mount_point)
        && oz_core::is_enotconn(&err)
    {
        lazy_unmount_fuse(&spec.mount_point)?;
    }

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
    let sock = resolve_active_daemon_socket(git_dir);
    if ping_daemon(&sock).is_ok() {
        match send_request(
            &sock,
            &VfsRequest::Unmount {
                workspace: workspace.to_string(),
                force,
                git_dir: Some(git_dir.to_path_buf()),
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
    let sock = resolve_active_daemon_socket(git_dir);
    match send_request(
        &sock,
        &VfsRequest::DirtyPaths {
            workspace: workspace.to_string(),
            git_dir: Some(git_dir.to_path_buf()),
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
    let mut repaired = false;
    for record in records {
        let is_listed = current
            .iter()
            .any(|m| m.workspace == record.workspace && m.mounted);
        let is_healthy = std::fs::symlink_metadata(&record.mount_point).is_ok();
        if !is_listed || !is_healthy {
            if needs_stale_unmount(&record.mount_point) {
                lazy_unmount_fuse(&record.mount_point)?;
            }
            let ws_dir = git_dir.join("ozone").join("ws").join(&record.workspace);
            let spec = crate::server::load_or_reconstruct_spec(git_dir, &ws_dir, record);
            match send_request(
                sock,
                &VfsRequest::Mount {
                    spec: Box::new(spec),
                },
            )? {
                VfsResponse::Mounted { .. } => {
                    repaired = true;
                }
                VfsResponse::Error { code, message } => {
                    return Err(OzError::MountUnavailable {
                        workspace: record.workspace.clone(),
                        path: record.mount_point.clone(),
                        errno: None,
                        message: format!("oz-vfsd mount repair error ({code}): {message}"),
                    });
                }
                other => {
                    return Err(OzError::Other(format!(
                        "unexpected oz-vfsd response while repairing mount '{}': {other:?}",
                        record.workspace
                    )));
                }
            }
        }
    }
    if repaired {
        list_mounts_via_socket(sock)
    } else {
        Ok(current)
    }
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
        _ => crate::overlay::is_fuse_mount_in_proc_mountinfo(mount_point),
    }
}

#[cfg(not(unix))]
fn needs_stale_unmount(_mount_point: &Path) -> bool {
    false
}

#[cfg(windows)]
mod win_no_inherit {
    use std::ffi::c_void;
    use std::sync::{Mutex, MutexGuard, PoisonError};

    const STD_INPUT_HANDLE: u32 = (-10i32) as u32;
    const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;
    const STD_ERROR_HANDLE: u32 = (-12i32) as u32;
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    const INVALID_HANDLE_VALUE: *mut c_void = (-1isize) as *mut c_void;

    unsafe extern "system" {
        fn GetStdHandle(nStdHandle: u32) -> *mut c_void;
        fn GetHandleInformation(hObject: *mut c_void, lpdwFlags: *mut u32) -> i32;
        fn SetHandleInformation(hObject: *mut c_void, dwMask: u32, dwFlags: u32) -> i32;
    }

    static GUARD_LOCK: Mutex<()> = Mutex::new(());

    /// Temporarily clears `HANDLE_FLAG_INHERIT` on the process standard handles so a detached
    /// `oz-vfsd.exe` child does not keep parent `Command::output()` stdout/stderr pipes open.
    pub(super) struct StdioNoInheritGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(*mut c_void, u32)>,
    }

    impl StdioNoInheritGuard {
        pub(super) fn acquire() -> Self {
            let lock = GUARD_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            let mut saved = Vec::with_capacity(3);
            for std_id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                // SAFETY: `GetStdHandle` is safe to call with standard Win32 `STD_*_HANDLE` IDs.
                let handle = unsafe { GetStdHandle(std_id) };
                if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                    continue;
                }
                let mut flags = 0u32;
                // SAFETY: `handle` is non-null and not `INVALID_HANDLE_VALUE`, and `&mut flags`
                // points to valid writable stack memory while `GUARD_LOCK` serializes access.
                if unsafe { GetHandleInformation(handle, &mut flags) } != 0
                    && (flags & HANDLE_FLAG_INHERIT) != 0
                {
                    // SAFETY: `handle` is a valid standard handle owned by the process and
                    // `GUARD_LOCK` serializes toggling `HANDLE_FLAG_INHERIT` across spawns.
                    let _ = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
                    saved.push((handle, flags));
                }
            }
            Self { _lock: lock, saved }
        }
    }

    impl Drop for StdioNoInheritGuard {
        fn drop(&mut self) {
            for (handle, orig_flags) in self.saved.drain(..) {
                // SAFETY: `handle` was captured from `GetStdHandle` in `acquire` while holding
                // `GUARD_LOCK` (still held until after `drop` finishes), restoring its prior flag.
                let _ = unsafe {
                    SetHandleInformation(
                        handle,
                        HANDLE_FLAG_INHERIT,
                        orig_flags & HANDLE_FLAG_INHERIT,
                    )
                };
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn send_tcp_request_rejects_missing_token_and_non_loopback() {
        let dir = TempDir::new().expect("tempdir");
        let missing_tok = dir.path().join("no_token.sock");
        std::fs::write(&missing_tok, "tcp://127.0.0.1:12345\n").expect("write");
        assert!(send_tcp_request(&missing_tok, &VfsRequest::Ping).is_err());

        let non_loopback = dir.path().join("remote.sock");
        std::fs::write(&non_loopback, "tcp://10.0.0.1:12345\ntoken=abcdef\n").expect("write");
        assert!(send_tcp_request(&non_loopback, &VfsRequest::Ping).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn client_verifies_socket_parent_and_rejects_symlink_endpoints() {
        let dir = TempDir::new().expect("tempdir");
        let real_parent = dir.path().join("real");
        std::fs::create_dir_all(&real_parent).expect("mkdir");
        let real_file = real_parent.join("vfsd.sock");
        std::fs::write(&real_file, "tcp://127.0.0.1:12345\ntoken=abc\n").expect("write");

        // Symlinked endpoint file must be rejected before connecting/reading.
        let link_file = real_parent.join("link.sock");
        std::os::unix::fs::symlink(&real_file, &link_file).expect("symlink file");
        assert!(verify_socket_endpoint_owner(&link_file, true).is_err());
        assert!(verify_socket_endpoint_owner(&link_file, false).is_err());

        // Symlinked parent directory must be rejected.
        let link_parent = dir.path().join("link_parent");
        std::os::unix::fs::symlink(&real_parent, &link_parent).expect("symlink parent");
        assert!(verify_socket_endpoint_owner(&link_parent.join("vfsd.sock"), true).is_err());
        assert!(verify_socket_parent_owner_if_exists(&link_parent.join("vfsd.sock")).is_err());

        // Non-UID-owned parent directory (e.g. root-owned `/tmp`) must be rejected when non-root.
        // SAFETY: `libc::geteuid()` is always safe to call and has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            let root_parent_sock = Path::new("/tmp/oz-vfsd-unowned-test.sock");
            assert!(verify_socket_parent_owner_if_exists(root_parent_sock).is_err());
            assert!(send_request(root_parent_sock, &VfsRequest::Ping).is_err());
        }

        // Owned real directory and regular file passes.
        verify_socket_endpoint_owner(&real_file, true).expect("valid owned tcp endpoint");
    }
}
