// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Unix socket IPC server (`oz-vfsd`) managing one or more CoW FUSE3 workspace mounts.

use crate::overlay::{
    MountedOverlay, OverlayMountConfig, lazy_unmount_fuse, mount_overlay, scan_dirty_paths,
};
use crate::protocol::{MountInfo, MountSpec, VfsRequest, VfsResponse, encode_line};
use chrono::Utc;
use fs4::{FileExt as Fs4FileExt, TryLockError};
use oz_core::{BackendKind, MountRecord, OzError, OzoneConfig, OzoneLayout, Result, WorkspaceState};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// High-level daemon handle summarizing runtime paths.
#[derive(Debug, Clone)]
pub struct VfsDaemon {
    pub git_dir: PathBuf,
    pub socket_path: PathBuf,
}

impl VfsDaemon {
    /// Constructs a daemon configuration for `git_dir` with an optional custom socket path.
    #[must_use]
    pub fn new(git_dir: impl Into<PathBuf>, socket_path: Option<PathBuf>) -> Self {
        let git_dir = git_dir.into();
        let socket_path =
            socket_path.unwrap_or_else(|| git_dir.join("ozone").join("vfsd.sock"));
        Self {
            git_dir,
            socket_path,
        }
    }
}

struct ActiveMount {
    spec: MountSpec,
    mounted: MountedOverlay,
}

impl ActiveMount {
    fn info(&self) -> MountInfo {
        let dirty_count = scan_dirty_paths(&self.mounted.config).map_or(0, |v| v.len());
        MountInfo {
            workspace: self.spec.workspace.clone(),
            branch: self.spec.branch.clone(),
            mount_point: self.spec.mount_point.clone(),
            upper_dir: self.spec.upper_dir.clone(),
            mounted: true,
            dirty_count,
        }
    }
}

struct DaemonManager {
    git_dir: PathBuf,
    socket_path: PathBuf,
    started_at: Instant,
    mounts: BTreeMap<String, ActiveMount>,
    write_seq: u64,
}

impl DaemonManager {
    fn new(git_dir: PathBuf, socket_path: PathBuf) -> Self {
        Self {
            git_dir,
            socket_path,
            started_at: Instant::now(),
            mounts: BTreeMap::new(),
            write_seq: 0,
        }
    }

    fn ws_dir(&self, workspace: &str) -> PathBuf {
        self.git_dir.join("ozone").join("ws").join(workspace)
    }

    fn write_json_atomic(&mut self, dest: &Path, json_bytes: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.write_seq += 1;
        let file_name = dest
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("record");
        let tmp = dest.with_file_name(format!(
            ".{file_name}.tmp.{}.{}",
            std::process::id(),
            self.write_seq
        ));
        std::fs::write(&tmp, json_bytes)?;
        std::fs::rename(&tmp, dest)?;
        Ok(())
    }

    fn persist_mount_state(&mut self, spec: &MountSpec, mounted_flag: bool) {
        let ws_dir = if !spec.git_dir.as_os_str().is_empty() {
            spec.git_dir.join("ozone").join("ws").join(&spec.workspace)
        } else if let Some(parent) = spec.meta_dir.parent() {
            parent.to_path_buf()
        } else {
            self.ws_dir(&spec.workspace)
        };

        let spec_path = ws_dir.join("spec.json");
        if let Ok(raw) = serde_json::to_vec_pretty(spec) {
            let _ = self.write_json_atomic(&spec_path, &raw);
        }

        let mount_json_path = ws_dir.join("mount.json");
        let now = Utc::now();
        let existing: Option<MountRecord> = std::fs::read_to_string(&mount_json_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());

        let record = MountRecord {
            schema_version: MountRecord::CURRENT_SCHEMA_VERSION,
            workspace: spec.workspace.clone(),
            branch: spec.branch.clone(),
            backend: BackendKind::LinuxFuse3,
            repo_root: spec.repo_root.clone(),
            git_dir: spec.git_dir.clone(),
            mount_point: spec.mount_point.clone(),
            upper_dir: Some(spec.upper_dir.clone()),
            meta_dir: Some(spec.meta_dir.clone()),
            base_tree: spec.base_tree.clone(),
            base_commit: spec.base_commit.clone(),
            socket_path: Some(self.socket_path.clone()),
            daemon_pid: mounted_flag.then_some(std::process::id()),
            mounted: mounted_flag,
            created_at: existing.as_ref().map_or(now, |r| r.created_at),
            updated_at: now,
        };
        if let Ok(raw) = serde_json::to_vec_pretty(&record) {
            let _ = self.write_json_atomic(&mount_json_path, &raw);
        }
    }

    fn mark_mount_record_unmounted(&mut self, workspace: &str) {
        let mount_json_path = self.ws_dir(workspace).join("mount.json");
        let Ok(raw) = std::fs::read_to_string(&mount_json_path) else {
            return;
        };
        let Ok(mut record) = serde_json::from_str::<MountRecord>(&raw) else {
            return;
        };
        record.mounted = false;
        record.daemon_pid = None;
        record.updated_at = Utc::now();
        if let Ok(bytes) = serde_json::to_vec_pretty(&record) {
            let _ = self.write_json_atomic(&mount_json_path, &bytes);
        }
    }

    fn mount_workspace(&mut self, spec: MountSpec) -> Result<MountInfo> {
        if let Some(existing) = self.mounts.get(&spec.workspace) {
            let healthy = std::fs::symlink_metadata(&existing.spec.mount_point).is_ok();
            if healthy && existing.spec == spec {
                return Ok(existing.info());
            }
        }
        if let Some(old) = self.mounts.remove(&spec.workspace) {
            let _ = old.mounted.unmount(true);
        }

        let mounted = mount_overlay(&spec)?;
        let active = ActiveMount {
            spec: spec.clone(),
            mounted,
        };
        let info = active.info();
        self.mounts.insert(spec.workspace.clone(), active);
        self.persist_mount_state(&spec, true);
        Ok(info)
    }

    fn unmount_workspace(&mut self, workspace: &str, force: bool) -> Result<()> {
        if let Some(active) = self.mounts.remove(workspace) {
            active.mounted.unmount(force)?;
        } else {
            let mount_json = self.ws_dir(workspace).join("mount.json");
            if let Ok(raw) = std::fs::read_to_string(&mount_json)
                && let Ok(record) = serde_json::from_str::<MountRecord>(&raw)
            {
                let needs_lazy = force
                    || std::fs::symlink_metadata(&record.mount_point)
                        .err()
                        .is_some_and(|e| oz_core::is_enotconn(&e));
                if needs_lazy {
                    lazy_unmount_fuse(&record.mount_point)?;
                }
            }
        }
        self.mark_mount_record_unmounted(workspace);
        Ok(())
    }

    fn dirty_paths(&self, workspace: &str) -> Result<Vec<crate::protocol::DirtyPathEntry>> {
        if let Some(active) = self.mounts.get(workspace) {
            return scan_dirty_paths(&active.mounted.config).map_err(OzError::Io);
        }
        let spec_path = self.ws_dir(workspace).join("spec.json");
        if let Ok(raw) = std::fs::read_to_string(&spec_path)
            && let Ok(spec) = serde_json::from_str::<MountSpec>(&raw)
        {
            let cfg = OverlayMountConfig::from_spec(&spec);
            return scan_dirty_paths(&cfg).map_err(OzError::Io);
        }
        let mount_json = self.ws_dir(workspace).join("mount.json");
        if let Ok(raw) = std::fs::read_to_string(&mount_json)
            && let Ok(record) = serde_json::from_str::<MountRecord>(&raw)
        {
            let spec = spec_from_mount_record(&self.git_dir, &record);
            let cfg = OverlayMountConfig::from_spec(&spec);
            return scan_dirty_paths(&cfg).map_err(OzError::Io);
        }
        Err(OzError::WorkspaceNotFound {
            name: workspace.to_string(),
        })
    }

    fn list_mounts(&self) -> Vec<MountInfo> {
        self.mounts.values().map(ActiveMount::info).collect()
    }

    fn restore_mounts_on_startup(&mut self) {
        let ws_root = self.git_dir.join("ozone").join("ws");
        let Ok(entries) = std::fs::read_dir(&ws_root) else {
            return;
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
            if !record.mounted || record.backend != BackendKind::LinuxFuse3 {
                continue;
            }

            let spec = load_or_reconstruct_spec(&self.git_dir, &ws_dir, &record);
            if let Err(err) = std::fs::symlink_metadata(&spec.mount_point)
                && oz_core::is_enotconn(&err)
            {
                let _ = lazy_unmount_fuse(&spec.mount_point);
            }

            if let Err(err) = self.mount_workspace(spec) {
                eprintln!(
                    "oz-vfsd: failed to restore mount for '{}': {err}",
                    record.workspace
                );
            }
        }
    }

    fn unmount_all_clean(&mut self) {
        let keys: Vec<String> = self.mounts.keys().cloned().collect();
        for key in keys {
            if let Some(active) = self.mounts.remove(&key) {
                let mp = active.spec.mount_point.clone();
                if active.mounted.unmount(false).is_err() {
                    let _ = lazy_unmount_fuse(&mp);
                }
            }
        }
    }

    fn handle_request(&mut self, req: VfsRequest) -> (VfsResponse, bool) {
        match req {
            VfsRequest::Ping => (
                VfsResponse::Pong {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    pid: std::process::id(),
                },
                false,
            ),
            VfsRequest::Status => (
                VfsResponse::DaemonStatus {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    pid: std::process::id(),
                    uptime_secs: self.started_at.elapsed().as_secs(),
                    active_mounts: self.list_mounts(),
                },
                false,
            ),
            VfsRequest::Mount { spec } => match self.mount_workspace(*spec) {
                Ok(mount) => (VfsResponse::Mounted { mount }, false),
                Err(err) => (
                    VfsResponse::Error {
                        code: err.error_code().to_string(),
                        message: err.to_string(),
                    },
                    false,
                ),
            },
            VfsRequest::Unmount { workspace, force } => {
                match self.unmount_workspace(&workspace, force) {
                    Ok(()) => (VfsResponse::Unmounted { workspace }, false),
                    Err(err) => (
                        VfsResponse::Error {
                            code: err.error_code().to_string(),
                            message: err.to_string(),
                        },
                        false,
                    ),
                }
            }
            VfsRequest::DirtyPaths { workspace } => match self.dirty_paths(&workspace) {
                Ok(paths) => (VfsResponse::DirtySet { workspace, paths }, false),
                Err(err) => (
                    VfsResponse::Error {
                        code: err.error_code().to_string(),
                        message: err.to_string(),
                    },
                    false,
                ),
            },
            VfsRequest::ListMounts => (
                VfsResponse::MountList {
                    mounts: self.list_mounts(),
                },
                false,
            ),
            VfsRequest::Shutdown => (VfsResponse::ShuttingDown, true),
        }
    }
}

pub(crate) fn load_or_reconstruct_spec(
    git_dir: &Path,
    ws_dir: &Path,
    record: &MountRecord,
) -> MountSpec {
    let spec_path = ws_dir.join("spec.json");
    if let Ok(raw) = std::fs::read_to_string(&spec_path)
        && let Ok(spec) = serde_json::from_str::<MountSpec>(&raw)
        && spec.workspace == record.workspace
    {
        return spec;
    }
    spec_from_mount_record(git_dir, record)
}

pub(crate) fn spec_from_mount_record(git_dir: &Path, record: &MountRecord) -> MountSpec {
    let layout = OzoneLayout::from_git_common_dir(git_dir);
    let upper_dir = record
        .upper_dir
        .clone()
        .unwrap_or_else(|| layout.ws_upper_dir(&record.workspace));
    let meta_dir = record
        .meta_dir
        .clone()
        .unwrap_or_else(|| layout.ws_tombstone_dir(&record.workspace));

    let baseline_dir = resolve_baseline_dir_for_record(&layout, record);
    let allow_other = OzoneConfig::load_for_repo(&record.repo_root, &layout.ozone_dir)
        .is_ok_and(|c| c.daemon.allow_other);

    MountSpec {
        workspace: record.workspace.clone(),
        branch: record.branch.clone(),
        repo_root: record.repo_root.clone(),
        git_dir: record.git_dir.clone(),
        mount_point: record.mount_point.clone(),
        upper_dir,
        meta_dir,
        base_tree: record.base_tree.clone(),
        base_commit: record.base_commit.clone(),
        baseline_dir,
        allow_other,
        writeback_cache: true,
    }
}

fn resolve_baseline_dir_for_record(layout: &OzoneLayout, record: &MountRecord) -> Option<PathBuf> {
    let state_path = layout.workspace_state_path(&record.workspace);
    if let Ok(raw) = std::fs::read_to_string(&state_path)
        && let Ok(st) = serde_json::from_str::<WorkspaceState>(&raw)
        && let Some(ref baseline_id) = st.baseline_id
    {
        let root_cand = layout.baselines_dir().join(baseline_id).join("root");
        if root_cand.is_dir() {
            return Some(root_cand);
        }
        let direct_cand = layout.baselines_dir().join(baseline_id);
        if direct_cand.is_dir() {
            return Some(direct_cand);
        }
    }

    if let Some(ref commit) = record.base_commit {
        let root_cand = layout.baselines_dir().join(commit).join("root");
        if root_cand.is_dir() {
            return Some(root_cand);
        }
        let direct_cand = layout.baselines_dir().join(commit);
        if direct_cand.is_dir() {
            return Some(direct_cand);
        }
    }
    None
}

fn acquire_daemon_lock(git_dir: &Path) -> Result<File> {
    let ozone_dir = git_dir.join("ozone");
    std::fs::create_dir_all(&ozone_dir)?;
    let lock_path = ozone_dir.join("vfsd.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    match Fs4FileExt::try_lock(&file) {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(OzError::Other(format!(
            "another oz-vfsd instance already holds {}",
            lock_path.display()
        ))),
        Err(TryLockError::Error(err)) => Err(OzError::Io(err)),
    }
}

#[cfg(unix)]
mod unix_signal {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::sync::atomic::{AtomicI32, Ordering};
    use tokio::net::UnixStream;

    static SIGNAL_WAKEUP_FD: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn handle_termination_signal(_sig: libc::c_int) {
        let fd = SIGNAL_WAKEUP_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            let byte: u8 = 1;
            unsafe {
                let _ = libc::write(fd, (&raw const byte).cast(), 1);
            }
        }
    }

    pub(super) struct SignalWatcher {
        rx: UnixStream,
        _tx: StdUnixStream,
        prev_term: libc::sigaction,
        prev_int: libc::sigaction,
    }

    impl SignalWatcher {
        pub(super) fn install() -> std::io::Result<Self> {
            let (tx, rx) = StdUnixStream::pair()?;
            tx.set_nonblocking(true)?;
            rx.set_nonblocking(true)?;
            SIGNAL_WAKEUP_FD.store(tx.as_raw_fd(), Ordering::SeqCst);

            let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
            sa.sa_sigaction = handle_termination_signal as *const () as usize;
            sa.sa_flags = libc::SA_RESTART;
            unsafe {
                libc::sigemptyset(&raw mut sa.sa_mask);
            }

            let mut prev_term: libc::sigaction = unsafe { std::mem::zeroed() };
            let mut prev_int: libc::sigaction = unsafe { std::mem::zeroed() };
            if unsafe { libc::sigaction(libc::SIGTERM, &raw const sa, &raw mut prev_term) } != 0 {
                SIGNAL_WAKEUP_FD.store(-1, Ordering::SeqCst);
                return Err(std::io::Error::last_os_error());
            }
            if unsafe { libc::sigaction(libc::SIGINT, &raw const sa, &raw mut prev_int) } != 0 {
                unsafe {
                    libc::sigaction(libc::SIGTERM, &raw const prev_term, std::ptr::null_mut());
                }
                SIGNAL_WAKEUP_FD.store(-1, Ordering::SeqCst);
                return Err(std::io::Error::last_os_error());
            }

            let rx = UnixStream::from_std(rx)?;
            Ok(Self {
                rx,
                _tx: tx,
                prev_term,
                prev_int,
            })
        }

        pub(super) async fn wait_signal(&self) {
            let _ = self.rx.readable().await;
        }
    }

    impl Drop for SignalWatcher {
        fn drop(&mut self) {
            SIGNAL_WAKEUP_FD.store(-1, Ordering::SeqCst);
            unsafe {
                libc::sigaction(libc::SIGTERM, &raw const self.prev_term, std::ptr::null_mut());
                libc::sigaction(libc::SIGINT, &raw const self.prev_int, std::ptr::null_mut());
            }
        }
    }
}

/// Runs the `oz-vfsd` Unix socket daemon until `Shutdown` IPC or `SIGTERM`/`SIGINT`.
#[cfg(unix)]
pub async fn run_daemon(git_dir: &Path, socket_path: &Path) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::sync::Notify;

    let ozone_dir = git_dir.join("ozone");
    std::fs::create_dir_all(&ozone_dir)?;
    std::fs::create_dir_all(ozone_dir.join("ws"))?;
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let _lock_file = acquire_daemon_lock(git_dir)?;

    if socket_path.exists() {
        let _ = std::fs::remove_file(socket_path);
    }

    let manager = Arc::new(Mutex::new(DaemonManager::new(
        git_dir.to_path_buf(),
        socket_path.to_path_buf(),
    )));

    // Restore previously active FUSE mounts before opening the IPC socket so clients
    // connecting after startup immediately observe live mounts.
    {
        let mgr = Arc::clone(&manager);
        tokio::task::spawn_blocking(move || {
            mgr.lock().expect("daemon lock poisoned").restore_mounts_on_startup();
        })
        .await
        .map_err(|e| OzError::Other(format!("startup restore panicked: {e}")))?;
    }

    let listener = UnixListener::bind(socket_path)?;
    let shutdown_notify = Arc::new(Notify::new());
    let signal_watcher = unix_signal::SignalWatcher::install()?;

    loop {
        tokio::select! {
            _ = shutdown_notify.notified() => {
                break;
            }
            () = signal_watcher.wait_signal() => {
                break;
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    break;
                };
                let mgr = Arc::clone(&manager);
                let notify = Arc::clone(&shutdown_notify);
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.into_split();
                    let mut reader = BufReader::new(read_half);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) => break,
                            Ok(_) => {
                                let trimmed = line.trim();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                let req: VfsRequest = match serde_json::from_str(trimmed) {
                                    Ok(r) => r,
                                    Err(err) => {
                                        let resp = VfsResponse::Error {
                                            code: "invalid_request".to_string(),
                                            message: err.to_string(),
                                        };
                                        if let Ok(encoded) = encode_line(&resp) {
                                            let _ = write_half.write_all(encoded.as_bytes()).await;
                                        }
                                        continue;
                                    }
                                };
                                let mgr_call = Arc::clone(&mgr);
                                let call_res = tokio::task::spawn_blocking(move || {
                                    mgr_call.lock().expect("daemon lock poisoned").handle_request(req)
                                }).await;

                                let (resp, should_shutdown) = match call_res {
                                    Ok(pair) => pair,
                                    Err(err) => (
                                        VfsResponse::Error {
                                            code: "internal_error".to_string(),
                                            message: err.to_string(),
                                        },
                                        false,
                                    ),
                                };
                                if let Ok(encoded) = encode_line(&resp)
                                    && write_half.write_all(encoded.as_bytes()).await.is_err()
                                {
                                    break;
                                }
                                let _ = write_half.flush().await;
                                if should_shutdown {
                                    notify.notify_waiters();
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
            }
        }
    }

    drop(listener);
    let _ = std::fs::remove_file(socket_path);
    let mgr_cleanup = Arc::clone(&manager);
    let _ = tokio::task::spawn_blocking(move || {
        mgr_cleanup
            .lock()
            .expect("daemon lock poisoned")
            .unmount_all_clean();
    })
    .await;

    Ok(())
}

/// Non-Unix compile-time stub for [`run_daemon`].
#[cfg(not(unix))]
pub async fn run_daemon(_git_dir: &Path, _socket_path: &Path) -> Result<()> {
    Err(OzError::Other(
        "oz-vfsd Unix socket daemon is only supported on Unix (Windows ProjFS lands in Milestone 2)"
            .to_string(),
    ))
}
