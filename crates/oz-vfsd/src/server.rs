// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Local IPC server (`oz-vfsd`) managing CoW FUSE3 (Linux) and ProjFS (Windows) mounts.

#[cfg(not(target_os = "windows"))]
use crate::overlay::{MountedOverlay, mount_overlay};
use crate::overlay::{OverlayMountConfig, lazy_unmount_fuse, scan_dirty_paths};
#[cfg(target_os = "windows")]
use crate::projfs::{MountedProjFs, mount_projfs};
use crate::protocol::{
    AuthenticatedRequest, MAX_IPC_LINE_BYTES, MountInfo, MountSpec, VfsRequest, VfsResponse,
    encode_line, format_tcp_endpoint,
};
use chrono::Utc;
use fs4::{FileExt as Fs4FileExt, TryLockError};
use oz_core::{
    BackendKind, MountRecord, OzError, OzoneConfig, OzoneLayout, Result, WorkspaceState,
    normalize_lexical, validate_workspace_name,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

/// Maximum number of concurrent IPC client connections served simultaneously by `oz-vfsd`.
pub const MAX_CONCURRENT_IPC_CONNECTIONS: usize = 64;

static ENDPOINT_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

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
            socket_path.unwrap_or_else(|| crate::client::socket_path_for_git_dir(&git_dir));
        Self {
            git_dir,
            socket_path,
        }
    }
}

enum MountedVfs {
    #[cfg(not(target_os = "windows"))]
    Overlay(MountedOverlay),
    #[cfg(target_os = "windows")]
    ProjFs(MountedProjFs),
}

impl MountedVfs {
    fn config(&self) -> &OverlayMountConfig {
        match self {
            #[cfg(not(target_os = "windows"))]
            Self::Overlay(m) => &m.config,
            #[cfg(target_os = "windows")]
            Self::ProjFs(m) => &m.config,
        }
    }

    fn unmount(self, force: bool) -> Result<()> {
        match self {
            #[cfg(not(target_os = "windows"))]
            Self::Overlay(m) => m.unmount(force),
            #[cfg(target_os = "windows")]
            Self::ProjFs(m) => m.unmount(force),
        }
    }
}

fn mount_platform_vfs(spec: &MountSpec) -> Result<(BackendKind, MountedVfs)> {
    #[cfg(target_os = "windows")]
    {
        let mounted = mount_projfs(spec)?;
        Ok((BackendKind::WindowsProjFs, MountedVfs::ProjFs(mounted)))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mounted = mount_overlay(spec)?;
        Ok((BackendKind::LinuxFuse3, MountedVfs::Overlay(mounted)))
    }
}

struct ActiveMount {
    spec: MountSpec,
    backend: BackendKind,
    mounted: MountedVfs,
}

impl ActiveMount {
    fn is_healthy(&self) -> bool {
        if std::fs::symlink_metadata(&self.spec.mount_point).is_err() {
            return false;
        }
        #[cfg(target_os = "linux")]
        if self.backend == BackendKind::LinuxFuse3
            && !crate::overlay::is_fuse_mount_in_proc_mountinfo(&self.spec.mount_point)
        {
            return false;
        }
        true
    }

    fn info(&self) -> MountInfo {
        let dirty_count = scan_dirty_paths(self.mounted.config()).map_or(0, |v| v.len());
        MountInfo {
            workspace: self.spec.workspace.clone(),
            branch: self.spec.branch.clone(),
            mount_point: self.spec.mount_point.clone(),
            upper_dir: self.spec.upper_dir.clone(),
            mounted: self.is_healthy(),
            dirty_count,
        }
    }
}

struct DaemonManager {
    /// Default repository `.git` dir in per-repo mode; `None` in `--global` user-wide mode.
    git_dir: Option<PathBuf>,
    global_mode: bool,
    socket_path: PathBuf,
    started_at: Instant,
    /// Keyed by `(canonical_or_clean_git_dir, workspace_name)` so multiple repos never collide.
    mounts: BTreeMap<(PathBuf, String), ActiveMount>,
    /// Repositories with active/historical mounts (persisted next to global socket for restart restore).
    known_repos: BTreeSet<PathBuf>,
    write_seq: u64,
}

fn canonical_git_key(git_dir: &Path) -> PathBuf {
    std::fs::canonicalize(git_dir).unwrap_or_else(|_| normalize_lexical(git_dir))
}

fn global_repos_registry_path(socket_path: &Path) -> PathBuf {
    socket_path.with_file_name("oz-vfsd-repos.json")
}

impl DaemonManager {
    fn new(git_dir: Option<PathBuf>, global_mode: bool, socket_path: PathBuf) -> Self {
        let mut known_repos = BTreeSet::new();
        if let Some(ref gd) = git_dir {
            known_repos.insert(canonical_git_key(gd));
        } else if let Ok(raw) = std::fs::read_to_string(global_repos_registry_path(&socket_path))
            && let Ok(list) = serde_json::from_str::<Vec<PathBuf>>(&raw)
        {
            for p in list {
                if p.is_dir() {
                    known_repos.insert(canonical_git_key(&p));
                }
            }
        }
        Self {
            git_dir,
            global_mode,
            socket_path,
            started_at: Instant::now(),
            mounts: BTreeMap::new(),
            known_repos,
            write_seq: 0,
        }
    }

    fn ws_dir_for(&self, git_dir: Option<&Path>, workspace: &str) -> Option<PathBuf> {
        let base = git_dir.or(self.git_dir.as_deref())?;
        Some(base.join("ozone").join("ws").join(workspace))
    }

    fn persist_known_repos(&mut self) {
        if !self.global_mode {
            return;
        }
        self.known_repos.retain(|p| p.is_dir());
        let list: Vec<PathBuf> = self.known_repos.iter().cloned().collect();
        if let Ok(bytes) = serde_json::to_vec_pretty(&list) {
            let reg = global_repos_registry_path(&self.socket_path);
            let _ = self.write_json_atomic(&reg, &bytes);
        }
    }

    fn mount_key_for(&self, git_dir: Option<&Path>, workspace: &str) -> Option<(PathBuf, String)> {
        if let Some(gd) = git_dir {
            return Some((canonical_git_key(gd), workspace.to_string()));
        }
        if let Some(ref default_gd) = self.git_dir {
            return Some((canonical_git_key(default_gd), workspace.to_string()));
        }
        let matches: Vec<_> = self
            .mounts
            .keys()
            .filter(|(_, ws)| ws == workspace)
            .cloned()
            .collect();
        if matches.len() == 1 {
            return matches.into_iter().next();
        }
        None
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

    fn persist_mount_state(&mut self, spec: &MountSpec, backend: BackendKind, mounted_flag: bool) {
        let ws_dir = if !spec.git_dir.as_os_str().is_empty() {
            spec.git_dir.join("ozone").join("ws").join(&spec.workspace)
        } else if let Some(parent) = spec.meta_dir.parent() {
            parent.to_path_buf()
        } else if let Some(d) = self.ws_dir_for(None, &spec.workspace) {
            d
        } else {
            return;
        };

        if self.global_mode && !spec.git_dir.as_os_str().is_empty() {
            let key = canonical_git_key(&spec.git_dir);
            if self.known_repos.insert(key) {
                self.persist_known_repos();
            }
        }

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
            backend,
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

    fn mark_mount_record_unmounted(&mut self, git_dir: Option<&Path>, workspace: &str) {
        let Some(ws_dir) = self.ws_dir_for(git_dir, workspace) else {
            return;
        };
        let mount_json_path = ws_dir.join("mount.json");
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

    fn mount_workspace(&mut self, mut spec: MountSpec) -> Result<MountInfo> {
        if self.global_mode {
            validate_global_mount_spec(&spec, false)?;
            // Never allow client IPC requests to escalate to `allow_other` on the shared daemon.
            spec.allow_other = false;
        }
        let key = (canonical_git_key(&spec.git_dir), spec.workspace.clone());
        if let Some(existing) = self.mounts.get(&key)
            && existing.is_healthy()
            && existing.spec == spec
        {
            return Ok(existing.info());
        }
        if let Some(old) = self.mounts.remove(&key) {
            let _ = old.mounted.unmount(true);
        }

        let (backend, mounted) = mount_platform_vfs(&spec)?;
        let active = ActiveMount {
            spec: spec.clone(),
            backend,
            mounted,
        };
        let info = active.info();
        let active_backend = active.backend;
        self.mounts.insert(key, active);
        self.persist_mount_state(&spec, active_backend, true);
        Ok(info)
    }

    fn unmount_workspace(
        &mut self,
        git_dir: Option<&Path>,
        workspace: &str,
        force: bool,
    ) -> Result<()> {
        let key_opt = self.mount_key_for(git_dir, workspace);
        let resolved_git_dir = key_opt
            .as_ref()
            .map(|(gd, _)| gd.clone())
            .or_else(|| git_dir.map(Path::to_path_buf))
            .or_else(|| self.git_dir.clone());

        if let Some(key) = key_opt
            && let Some(active) = self.mounts.remove(&key)
        {
            let mp = active.spec.mount_point.clone();
            let backend = active.backend;
            if let Err(err) = active.mounted.unmount(force) {
                #[cfg(target_os = "linux")]
                if !force
                    && backend == BackendKind::LinuxFuse3
                    && !crate::overlay::is_fuse_mount_in_proc_mountinfo(&mp)
                    && std::fs::symlink_metadata(&mp)
                        .err()
                        .is_none_or(|e| !oz_core::is_enotconn(&e))
                {
                    // Mount was already detached externally; still finalize mount.json state.
                } else {
                    return Err(err);
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = (mp, backend);
                    return Err(err);
                }
            }
        } else if let Some(ws_dir) = self.ws_dir_for(resolved_git_dir.as_deref(), workspace) {
            let mount_json = ws_dir.join("mount.json");
            if let Ok(raw) = std::fs::read_to_string(&mount_json)
                && let Ok(record) = serde_json::from_str::<MountRecord>(&raw)
            {
                let needs_lazy = force
                    || std::fs::symlink_metadata(&record.mount_point)
                        .err()
                        .is_some_and(|e| oz_core::is_enotconn(&e))
                    || {
                        #[cfg(unix)]
                        {
                            crate::overlay::is_fuse_mount_in_proc_mountinfo(&record.mount_point)
                        }
                        #[cfg(not(unix))]
                        {
                            false
                        }
                    };
                if needs_lazy {
                    lazy_unmount_fuse(&record.mount_point)?;
                }
            }
        }
        self.mark_mount_record_unmounted(resolved_git_dir.as_deref(), workspace);
        Ok(())
    }

    fn dirty_paths(
        &self,
        git_dir: Option<&Path>,
        workspace: &str,
    ) -> Result<Vec<crate::protocol::DirtyPathEntry>> {
        if let Some(key) = self.mount_key_for(git_dir, workspace)
            && let Some(active) = self.mounts.get(&key)
        {
            return scan_dirty_paths(active.mounted.config()).map_err(OzError::Io);
        }
        let resolved_gd = git_dir.or(self.git_dir.as_deref());
        if let Some(ws_dir) = self.ws_dir_for(resolved_gd, workspace) {
            let spec_path = ws_dir.join("spec.json");
            if let Ok(raw) = std::fs::read_to_string(&spec_path)
                && let Ok(spec) = serde_json::from_str::<MountSpec>(&raw)
            {
                let cfg = OverlayMountConfig::from_spec(&spec);
                return scan_dirty_paths(&cfg).map_err(OzError::Io);
            }
            let mount_json = ws_dir.join("mount.json");
            if let Ok(raw) = std::fs::read_to_string(&mount_json)
                && let Ok(record) = serde_json::from_str::<MountRecord>(&raw)
                && let Some(gd) = resolved_gd
            {
                let spec = spec_from_mount_record(gd, &record);
                let cfg = OverlayMountConfig::from_spec(&spec);
                return scan_dirty_paths(&cfg).map_err(OzError::Io);
            }
        }
        Err(OzError::WorkspaceNotFound {
            name: workspace.to_string(),
        })
    }

    fn list_mounts(&self) -> Vec<MountInfo> {
        self.mounts.values().map(ActiveMount::info).collect()
    }

    fn restore_mounts_on_startup(&mut self) {
        let repos: Vec<PathBuf> = self.known_repos.iter().cloned().collect();
        for repo_git_dir in repos {
            let ws_root = repo_git_dir.join("ozone").join("ws");
            let Ok(entries) = std::fs::read_dir(&ws_root) else {
                continue;
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
                if !record.mounted || !record.backend.requires_daemon() {
                    continue;
                }

                let spec = load_or_reconstruct_spec(&repo_git_dir, &ws_dir, &record);
                let stale_enotconn = std::fs::symlink_metadata(&spec.mount_point)
                    .err()
                    .is_some_and(|err| oz_core::is_enotconn(&err));
                let in_mountinfo = {
                    #[cfg(unix)]
                    {
                        crate::overlay::is_fuse_mount_in_proc_mountinfo(&spec.mount_point)
                    }
                    #[cfg(not(unix))]
                    {
                        false
                    }
                };
                if stale_enotconn || in_mountinfo {
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
    }

    fn unmount_all_clean(&mut self) {
        let keys: Vec<(PathBuf, String)> = self.mounts.keys().cloned().collect();
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
            VfsRequest::Unmount {
                workspace,
                force,
                git_dir,
            } => match self.unmount_workspace(git_dir.as_deref(), &workspace, force) {
                Ok(()) => (VfsResponse::Unmounted { workspace }, false),
                Err(err) => (
                    VfsResponse::Error {
                        code: err.error_code().to_string(),
                        message: err.to_string(),
                    },
                    false,
                ),
            },
            VfsRequest::DirtyPaths { workspace, git_dir } => {
                match self.dirty_paths(git_dir.as_deref(), &workspace) {
                    Ok(paths) => (VfsResponse::DirtySet { workspace, paths }, false),
                    Err(err) => (
                        VfsResponse::Error {
                            code: err.error_code().to_string(),
                            message: err.to_string(),
                        },
                        false,
                    ),
                }
            }
            VfsRequest::ListMounts => (
                VfsResponse::MountList {
                    mounts: self.list_mounts(),
                },
                false,
            ),
            VfsRequest::Shutdown => {
                if self.global_mode {
                    (
                        VfsResponse::Error {
                            code: "forbidden".to_string(),
                            message: "global oz-vfsd is managed by systemd; stop via `systemctl --user stop oz-vfsd.service`".to_string(),
                        },
                        false,
                    )
                } else {
                    (VfsResponse::ShuttingDown, true)
                }
            }
        }
    }
}

impl Drop for DaemonManager {
    fn drop(&mut self) {
        // Ensure active FUSE3/ProjFS mounts are always unmounted even if the daemon
        // future is cancelled or returns early before `cleanup_daemon_runtime` runs.
        self.unmount_all_clean();
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

fn guardrail_err(kind: &str, message: String) -> OzError {
    OzError::GuardrailViolation {
        kind: kind.to_string(),
        message,
        recovery: Vec::new(),
    }
}

/// Validates a [`MountSpec`] received by the hardened user-wide global `oz-vfsd` daemon:
/// - `workspace` must be a valid Ozone workspace name (no path traversal).
/// - `git_dir` must be an existing UID-owned Git directory (`HEAD` + `objects/`) free of symlinks.
/// - `repo_root` must be an existing UID-owned directory free of symlinks.
/// - `upper_dir` and `meta_dir` must be symlink-free, UID-owned along existing ancestors, and
///   canonically reside under `<git_dir>/ozone/ws/<workspace>/`.
/// - `baseline_dir` (when `Some`) must be absolute, free of `..`, free of symlinks along existing
///   paths/leaves, and UID-owned along existing path/ancestors.
/// - `mount_point` must be absolute, free of `..`, free of symlinks along ancestors and leaf,
///   reside inside an `*.oz/<workspace>` (or `<git_dir>/ozone/`) directory, and be empty
///   (unless `already_mounted_here` is `true` or stale `ENOTCONN`).
pub fn validate_global_mount_spec(spec: &MountSpec, already_mounted_here: bool) -> Result<()> {
    validate_workspace_name(&spec.workspace)?;

    for (label, p) in [
        ("git_dir", &spec.git_dir),
        ("repo_root", &spec.repo_root),
        ("mount_point", &spec.mount_point),
        ("upper_dir", &spec.upper_dir),
        ("meta_dir", &spec.meta_dir),
    ] {
        validate_abs_no_parent_dir(p, label)?;
    }

    reject_symlinks_along_existing_path(&spec.git_dir, "git_dir")?;
    verify_owned_dir(&spec.git_dir, "git_dir")?;
    let canon_git = std::fs::canonicalize(&spec.git_dir).map_err(|err| {
        guardrail_err(
            "global_vfsd_validation",
            format!(
                "git_dir '{}' could not be canonicalized: {err}",
                spec.git_dir.display()
            ),
        )
    })?;
    verify_owned_dir(&canon_git, "git_dir")?;
    if !canon_git.join("HEAD").is_file() || !canon_git.join("objects").is_dir() {
        return Err(guardrail_err(
            "global_vfsd_validation",
            format!(
                "git_dir '{}' is not a valid Git directory (missing HEAD or objects/)",
                spec.git_dir.display()
            ),
        ));
    }

    reject_symlinks_along_existing_path(&spec.repo_root, "repo_root")?;
    verify_owned_dir(&spec.repo_root, "repo_root")?;
    let canon_repo = std::fs::canonicalize(&spec.repo_root).map_err(|err| {
        guardrail_err(
            "global_vfsd_validation",
            format!(
                "repo_root '{}' could not be canonicalized: {err}",
                spec.repo_root.display()
            ),
        )
    })?;
    verify_owned_dir(&canon_repo, "repo_root")?;

    let expected_ws_meta_root = canon_git.join("ozone").join("ws").join(&spec.workspace);
    for (label, path) in [("upper_dir", &spec.upper_dir), ("meta_dir", &spec.meta_dir)] {
        reject_symlinks_along_existing_path(path, label)?;
        let (canon_existing, missing_tail) = resolve_canonical_with_missing_tail(path, label)?;
        let mut resolved = canon_existing.clone();
        for seg in &missing_tail {
            resolved.push(seg);
        }
        if !resolved.starts_with(&expected_ws_meta_root) {
            return Err(guardrail_err(
                "global_vfsd_validation",
                format!(
                    "{label} '{}' (resolved '{}') must reside under '<git_dir>/ozone/ws/{}/'",
                    path.display(),
                    resolved.display(),
                    spec.workspace
                ),
            ));
        }
        verify_owned_dir_chain(&canon_git, &canon_existing, label)?;
    }

    if let Some(ref baseline) = spec.baseline_dir {
        validate_abs_no_parent_dir(baseline, "baseline_dir")?;
        reject_symlinks_along_existing_path(baseline, "baseline_dir")?;
        let _ = resolve_canonical_with_missing_tail(baseline, "baseline_dir")?;
    }

    reject_symlinks_along_existing_path(&spec.mount_point, "mount_point")?;
    let mp_parent = spec.mount_point.parent().ok_or_else(|| {
        guardrail_err(
            "global_vfsd_validation",
            format!(
                "mount_point '{}' must have a parent directory",
                spec.mount_point.display()
            ),
        )
    })?;
    let (canon_mp_parent_existing, mp_parent_missing) =
        resolve_canonical_with_missing_tail(mp_parent, "mount_point parent")?;
    let mut resolved_mp = canon_mp_parent_existing;
    for seg in &mp_parent_missing {
        resolved_mp.push(seg);
    }
    if let Some(file_name) = spec.mount_point.file_name() {
        resolved_mp.push(file_name);
    }

    let mp_name = spec
        .mount_point
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let parent_name = mp_parent.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let inside_oz_container = mp_name == spec.workspace && parent_name.ends_with(".oz");
    let inside_git_ozone = resolved_mp.starts_with(canon_git.join("ozone"));
    if !inside_oz_container && !inside_git_ozone {
        return Err(guardrail_err(
            "global_vfsd_validation",
            format!(
                "refusing mount_point '{}': global oz-vfsd only mounts into '*.oz/<workspace>' directories",
                spec.mount_point.display()
            ),
        ));
    }

    match std::fs::symlink_metadata(&spec.mount_point) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(guardrail_err(
                    "global_vfsd_validation",
                    format!(
                        "mount_point '{}' must be a regular directory (not a symlink or file)",
                        spec.mount_point.display()
                    ),
                ));
            }
            verify_owned_dir(mp_parent, "mount_point parent")?;
            verify_uid_owner(&meta, "mount_point", &spec.mount_point)?;
            if !already_mounted_here
                && std::fs::read_dir(&spec.mount_point)
                    .map(|mut it| it.next().is_some())
                    .unwrap_or(true)
            {
                return Err(guardrail_err(
                    "global_vfsd_validation",
                    format!(
                        "refusing to mount over non-empty directory '{}'",
                        spec.mount_point.display()
                    ),
                ));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound || oz_core::is_enotconn(&err) => {
            if std::fs::symlink_metadata(mp_parent).is_ok() {
                verify_owned_dir(mp_parent, "mount_point parent")?;
            }
        }
        Err(err) => return Err(OzError::Io(err)),
    }

    Ok(())
}

fn validate_abs_no_parent_dir(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute() {
        return Err(guardrail_err(
            "global_vfsd_validation",
            format!("{label} '{}' must be an absolute path", path.display()),
        ));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(guardrail_err(
            "global_vfsd_validation",
            format!(
                "{label} '{}' must not contain '..' segments",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn reject_symlinks_along_existing_path(path: &Path, label: &str) -> Result<()> {
    let mut cur = PathBuf::new();
    for comp in path.components() {
        cur.push(comp.as_os_str());
        if matches!(comp, Component::RootDir | Component::Prefix(_)) {
            continue;
        }
        // On macOS, `/var` and `/tmp` are OS-managed top-level symlinks to `/private/var`
        // and `/private/tmp` (used by `std::env::temp_dir()`).
        #[cfg(target_os = "macos")]
        if matches!(cur.to_str(), Some("/var" | "/tmp")) {
            continue;
        }
        match std::fs::symlink_metadata(&cur) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(guardrail_err(
                        "global_vfsd_validation",
                        format!(
                            "{label} '{}' contains symlink component '{}'",
                            path.display(),
                            cur.display()
                        ),
                    ));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
            Err(err) if oz_core::is_enotconn(&err) && cur == path => break,
            Err(err) => return Err(OzError::Io(err)),
        }
    }
    Ok(())
}

/// Peels non-existent trailing path segments using `symlink_metadata`, verifies that the deepest
/// existing ancestor (or leaf) is a real UID-owned directory (not a symlink or file), canonicalizes
/// it, re-verifies ownership on the canonical directory, and returns `(canon_existing, missing_tail)`.
fn resolve_canonical_with_missing_tail(
    path: &Path,
    label: &str,
) -> Result<(PathBuf, Vec<OsString>)> {
    let mut cursor = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(guardrail_err(
                        "global_vfsd_validation",
                        format!(
                            "{label} '{}' contains symlink component '{}'",
                            path.display(),
                            cursor.display()
                        ),
                    ));
                }
                if !meta.is_dir() {
                    return Err(guardrail_err(
                        "global_vfsd_validation",
                        format!(
                            "{label} '{}' has non-directory path component '{}'",
                            path.display(),
                            cursor.display()
                        ),
                    ));
                }
                verify_uid_owner(&meta, label, cursor)?;
                let canon = std::fs::canonicalize(cursor).map_err(|err| {
                    guardrail_err(
                        "global_vfsd_validation",
                        format!(
                            "{label} '{}' could not be canonicalized: {err}",
                            cursor.display()
                        ),
                    )
                })?;
                verify_owned_dir(&canon, label)?;
                missing.reverse();
                return Ok((canon, missing));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = cursor.file_name() else {
                    return Err(guardrail_err(
                        "global_vfsd_validation",
                        format!("{label} '{}' has no existing ancestor", path.display()),
                    ));
                };
                missing.push(name.to_os_string());
                let Some(parent) = cursor.parent() else {
                    return Err(guardrail_err(
                        "global_vfsd_validation",
                        format!("{label} '{}' has no existing parent", path.display()),
                    ));
                };
                cursor = parent;
            }
            Err(err) => return Err(OzError::Io(err)),
        }
    }
}

fn verify_owned_dir_chain(ancestor: &Path, descendant_existing: &Path, label: &str) -> Result<()> {
    let rel = descendant_existing.strip_prefix(ancestor).map_err(|_| {
        guardrail_err(
            "global_vfsd_validation",
            format!(
                "{label} '{}' escapes '{}'",
                descendant_existing.display(),
                ancestor.display()
            ),
        )
    })?;
    let mut cur = ancestor.to_path_buf();
    verify_owned_dir(&cur, label)?;
    for comp in rel.components() {
        cur.push(comp.as_os_str());
        verify_owned_dir(&cur, label)?;
    }
    Ok(())
}

fn verify_owned_dir(path: &Path, label: &str) -> Result<()> {
    let meta = std::fs::symlink_metadata(path).map_err(|err| {
        guardrail_err(
            "global_vfsd_validation",
            format!("{label} '{}' is inaccessible: {err}", path.display()),
        )
    })?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(guardrail_err(
            "global_vfsd_validation",
            format!("{label} '{}' must be a real directory", path.display()),
        ));
    }
    verify_uid_owner(&meta, label, path)
}

fn verify_uid_owner(meta: &std::fs::Metadata, label: &str, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: `libc::geteuid()` is always safe to call and has no preconditions.
        let my_uid = unsafe { libc::geteuid() };
        if meta.uid() != my_uid {
            return Err(guardrail_err(
                "global_vfsd_validation",
                format!(
                    "{label} '{}' is owned by uid {}, expected current uid {my_uid}",
                    path.display(),
                    meta.uid()
                ),
            ));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (meta, label, path);
    }
    Ok(())
}

/// Ensures `parent` exists as a non-symlink directory owned by the current user with strict
/// owner-only permissions (`0o700` on Unix), propagating all permission errors.
fn ensure_secure_socket_parent(parent: &Path) -> Result<()> {
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        reject_symlinks_along_existing_path(parent, "socket parent directory")?;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(parent)?;
        verify_owned_dir(parent, "socket parent directory")?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        verify_owned_dir(parent, "socket parent directory")?;
        let meta = std::fs::symlink_metadata(parent)?;
        if (meta.permissions().mode() & 0o077) != 0 {
            return Err(guardrail_err(
                "ipc_security",
                format!(
                    "socket parent directory '{}' has overly permissive mode {:o} (expected 0700)",
                    parent.display(),
                    meta.permissions().mode() & 0o777
                ),
            ));
        }
    }
    #[cfg(not(unix))]
    {
        reject_symlinks_along_existing_path(parent, "socket parent directory")?;
        std::fs::create_dir_all(parent)?;
        verify_owned_dir(parent, "socket parent directory")?;
    }
    Ok(())
}

fn acquire_lock_file(lock_path: &Path) -> Result<File> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    match Fs4FileExt::try_lock(&file) {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(OzError::Other(format!(
            "another oz-vfsd instance already holds {}",
            lock_path.display()
        ))),
        Err(TryLockError::Error(err)) => Err(OzError::Io(err)),
    }
}

fn acquire_daemon_lock(git_dir: &Path) -> Result<File> {
    let ozone_dir = git_dir.join("ozone");
    acquire_lock_file(&ozone_dir.join("vfsd.lock"))
}

/// Acquires the daemon manager mutex, recovering gracefully if a prior holder panicked.
fn lock_manager(manager: &Mutex<DaemonManager>) -> MutexGuard<'_, DaemonManager> {
    manager.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Constant-time equality comparison for IPC authentication tokens.
///
/// Prevents timing side-channel leaks by accumulating byte XORs across the full slice
/// without short-circuiting.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= std::hint::black_box(x ^ y);
    }
    std::hint::black_box(diff) == 0
}

#[cfg(unix)]
fn fill_csprng_bytes(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    let mut rng = File::open("/dev/urandom")?;
    rng.read_exact(buf)
}

#[cfg(windows)]
fn fill_csprng_bytes(buf: &mut [u8]) -> std::io::Result<()> {
    use std::ffi::c_void;

    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(
            h_algorithm: *mut c_void,
            pb_buffer: *mut u8,
            cb_buffer: u32,
            dw_flags: u32,
        ) -> i32;
    }

    let len = u32::try_from(buf.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "CSPRNG buffer length exceeds u32::MAX",
        )
    })?;
    // SAFETY: `buf.as_mut_ptr()` is valid for writes of `len` bytes. Passing a null algorithm
    // handle with `BCRYPT_USE_SYSTEM_PREFERRED_RNG` is documented by Win32 to fill `buf` from
    // the OS system-preferred CSPRNG.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(std::io::Error::other(format!(
            "BCryptGenRandom failed with NTSTATUS 0x{:08x}",
            status as u32
        )));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn fill_csprng_bytes(_buf: &mut [u8]) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "OS CSPRNG is unsupported on this target",
    ))
}

/// Generates a 256-bit cryptographically random hex authentication token (64 lowercase hex chars).
fn generate_ipc_auth_token() -> Result<String> {
    use std::fmt::Write as _;

    let mut bytes = [0u8; 32];
    fill_csprng_bytes(&mut bytes)?;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(hex, "{b:02x}");
    }
    Ok(hex)
}

#[cfg(windows)]
mod win_acl {
    use std::ffi::c_void;
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;

    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_USER_CLASS: u32 = 1;
    const SDDL_REVISION_1: u32 = 1;
    const SE_FILE_OBJECT: u32 = 1;
    const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
    const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;
    const MAX_SID_UTF16_LEN: usize = 256;

    #[repr(C)]
    struct SidAndAttributes {
        sid: *mut c_void,
        attributes: u32,
    }

    #[repr(C)]
    struct TokenUser {
        user: SidAndAttributes,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn CloseHandle(h_object: *mut c_void) -> i32;
        fn LocalFree(h_mem: *mut c_void) -> *mut c_void;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(
            process_handle: *mut c_void,
            desired_access: u32,
            token_handle: *mut *mut c_void,
        ) -> i32;
        fn GetTokenInformation(
            token_handle: *mut c_void,
            token_information_class: u32,
            token_information: *mut c_void,
            token_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *mut c_void, string_sid: *mut *mut u16) -> i32;
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            string_security_descriptor: *const u16,
            string_sd_revision: u32,
            security_descriptor: *mut *mut c_void,
            security_descriptor_size: *mut u32,
        ) -> i32;
        fn GetSecurityDescriptorDacl(
            p_security_descriptor: *mut c_void,
            lpb_dacl_present: *mut i32,
            p_dacl: *mut *mut c_void,
            lpb_dacl_defaulted: *mut i32,
        ) -> i32;
        fn SetSecurityInfo(
            handle: *mut c_void,
            object_type: u32,
            security_info: u32,
            psid_owner: *mut c_void,
            psid_group: *mut c_void,
            p_dacl: *mut c_void,
            p_sacl: *mut c_void,
        ) -> u32;
    }

    struct HandleGuard(*mut c_void);

    impl Drop for HandleGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is an owned Win32 kernel handle opened via `OpenProcessToken`
                // and is closed exactly once on drop.
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    struct LocalMemGuard(*mut c_void);

    impl Drop for LocalMemGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` was allocated by Win32 (`ConvertSidToStringSidW` or
                // `ConvertStringSecurityDescriptorToSecurityDescriptorW`) and is freed once via `LocalFree`.
                unsafe {
                    let _ = LocalFree(self.0);
                }
            }
        }
    }

    /// Replaces the DACL on `file` with a protected DACL granting full access solely to the
    /// current process user's SID (disabling inherited ACEs from parent directories).
    pub(super) fn restrict_file_to_current_user(file: &File) -> std::io::Result<()> {
        let mut token_handle: *mut c_void = std::ptr::null_mut();
        // SAFETY: `GetCurrentProcess()` returns a valid pseudo-handle; `&mut token_handle` is a
        // valid writable out-pointer for `OpenProcessToken`.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_handle) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let _token_guard = HandleGuard(token_handle);

        let mut needed: u32 = 0;
        // SAFETY: Querying required buffer length with null buffer and zero length is the
        // documented Win32 pattern for `GetTokenInformation`.
        let _ = unsafe {
            GetTokenInformation(
                token_handle,
                TOKEN_USER_CLASS,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        if (needed as usize) < std::mem::size_of::<TokenUser>() {
            return Err(std::io::Error::last_os_error());
        }

        // Allocate 8-byte aligned storage so casting to `*const TokenUser` is strictly aligned.
        let word_count = (needed as usize).div_ceil(std::mem::size_of::<u64>());
        let mut buf = vec![0u64; word_count];
        let buf_bytes = u32::try_from(buf.len() * std::mem::size_of::<u64>())
            .map_err(|_| std::io::Error::other("token buffer size overflow"))?;
        // SAFETY: `buf.as_mut_ptr()` points to `buf_bytes` writable bytes and `token_handle` is valid.
        if unsafe {
            GetTokenInformation(
                token_handle,
                TOKEN_USER_CLASS,
                buf.as_mut_ptr().cast::<c_void>(),
                buf_bytes,
                &mut needed,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: `buf` is 8-byte aligned, has length >= `size_of::<TokenUser>()`, and was
        // initialized by a successful `GetTokenInformation(TokenUser)` call.
        let user_sid = unsafe { (*buf.as_ptr().cast::<TokenUser>()).user.sid };
        if user_sid.is_null() {
            return Err(std::io::Error::other("Windows TokenUser returned null SID"));
        }

        let mut sid_w_ptr: *mut u16 = std::ptr::null_mut();
        // SAFETY: `user_sid` points inside `buf` (still alive) and `&mut sid_w_ptr` is valid.
        if unsafe { ConvertSidToStringSidW(user_sid, &mut sid_w_ptr) } == 0 || sid_w_ptr.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let _sid_guard = LocalMemGuard(sid_w_ptr.cast::<c_void>());

        let mut sid_len = 0usize;
        while sid_len < MAX_SID_UTF16_LEN {
            // SAFETY: `sid_w_ptr` is a NUL-terminated UTF-16 string allocated by
            // `ConvertSidToStringSidW`; we stop immediately upon reading the NUL terminator.
            if unsafe { *sid_w_ptr.add(sid_len) } == 0 {
                break;
            }
            sid_len += 1;
        }
        // SAFETY: `sid_w_ptr` is valid for `sid_len` UTF-16 code units before the NUL terminator.
        let sid_str =
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid_w_ptr, sid_len) });
        if sid_str.is_empty() {
            return Err(std::io::Error::other("empty Windows user SID string"));
        }

        let sddl_w: Vec<u16> = format!("D:P(A;;FA;;;{sid_str})")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut sd_ptr: *mut c_void = std::ptr::null_mut();
        // SAFETY: `sddl_w` is NUL-terminated UTF-16 and `&mut sd_ptr` is a valid out-pointer.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_w.as_ptr(),
                SDDL_REVISION_1,
                &mut sd_ptr,
                std::ptr::null_mut(),
            )
        } == 0
            || sd_ptr.is_null()
        {
            return Err(std::io::Error::last_os_error());
        }
        let _sd_guard = LocalMemGuard(sd_ptr);

        let mut dacl_present: i32 = 0;
        let mut dacl_ptr: *mut c_void = std::ptr::null_mut();
        let mut dacl_defaulted: i32 = 0;
        // SAFETY: `sd_ptr` is a valid self-relative security descriptor kept alive by `_sd_guard`.
        if unsafe {
            GetSecurityDescriptorDacl(
                sd_ptr,
                &mut dacl_present,
                &mut dacl_ptr,
                &mut dacl_defaulted,
            )
        } == 0
            || dacl_present == 0
            || dacl_ptr.is_null()
        {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: `file.as_raw_handle()` is a valid open file handle and `dacl_ptr` points into
        // `sd_ptr` which remains alive for the duration of this call.
        let rc = unsafe {
            SetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl_ptr,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc as i32));
        }
        Ok(())
    }
}

async fn prepare_daemon_runtime(
    git_dir: Option<&Path>,
    global_mode: bool,
    socket_path: &Path,
) -> Result<(File, Arc<Mutex<DaemonManager>>)> {
    let lock_file = if let Some(gd) = git_dir {
        let ozone_dir = gd.join("ozone");
        std::fs::create_dir_all(&ozone_dir)?;
        std::fs::create_dir_all(ozone_dir.join("ws"))?;
        if let Some(parent) = socket_path.parent() {
            ensure_secure_socket_parent(parent)?;
        }
        acquire_daemon_lock(gd)?
    } else {
        if let Some(parent) = socket_path.parent() {
            ensure_secure_socket_parent(parent)?;
        }
        acquire_lock_file(&socket_path.with_file_name("oz-vfsd.lock"))?
    };

    if std::fs::symlink_metadata(socket_path).is_ok() {
        let _ = std::fs::remove_file(socket_path);
    }

    let manager = Arc::new(Mutex::new(DaemonManager::new(
        git_dir.map(Path::to_path_buf),
        global_mode,
        socket_path.to_path_buf(),
    )));

    // Restore previously active VFS mounts before opening the IPC endpoint so clients
    // connecting after startup immediately observe live mounts.
    {
        let mgr = Arc::clone(&manager);
        tokio::task::spawn_blocking(move || {
            lock_manager(&mgr).restore_mounts_on_startup();
        })
        .await
        .map_err(|e| OzError::Other(format!("startup restore panicked: {e}")))?;
    }

    Ok((lock_file, manager))
}

async fn cleanup_daemon_runtime(socket_path: &Path, manager: Arc<Mutex<DaemonManager>>) {
    let _ = tokio::task::spawn_blocking(move || {
        lock_manager(&manager).unmount_all_clean();
    })
    .await;
    let _ = std::fs::remove_file(socket_path);
}

fn handle_request_concurrent(
    manager: &Mutex<DaemonManager>,
    req: VfsRequest,
) -> (VfsResponse, bool) {
    if let VfsRequest::Mount { spec } = req {
        let mut spec = *spec;
        let (key, global_mode, old_mount) = {
            let mut guard = lock_manager(manager);
            let global_mode = guard.global_mode;
            if global_mode {
                spec.allow_other = false;
            }
            let key = (canonical_git_key(&spec.git_dir), spec.workspace.clone());
            if let Some(existing) = guard.mounts.get(&key)
                && existing.is_healthy()
                && existing.spec == spec
            {
                return (
                    VfsResponse::Mounted {
                        mount: existing.info(),
                    },
                    false,
                );
            }
            let already_mounted_here = guard.mounts.contains_key(&key);
            if global_mode && let Err(err) = validate_global_mount_spec(&spec, already_mounted_here)
            {
                return (
                    VfsResponse::Error {
                        code: err.error_code().to_string(),
                        message: err.to_string(),
                    },
                    false,
                );
            }
            let old = guard.mounts.remove(&key);
            (key, global_mode, old)
        };
        let _ = global_mode;

        if let Some(old) = old_mount {
            let _ = old.mounted.unmount(true);
        }

        // Perform the blocking FUSE3/ProjFS mount syscall OUTSIDE the global daemon mutex so
        // concurrent `oz ws spawn` calls mount in parallel instead of serializing behind one lock.
        let (backend, mounted) = match mount_platform_vfs(&spec) {
            Ok(pair) => pair,
            Err(err) => {
                return (
                    VfsResponse::Error {
                        code: err.error_code().to_string(),
                        message: err.to_string(),
                    },
                    false,
                );
            }
        };

        let active = ActiveMount {
            spec: spec.clone(),
            backend,
            mounted,
        };
        let info = active.info();

        let displaced = {
            let mut guard = lock_manager(manager);
            let prev = guard.mounts.insert(key, active);
            guard.persist_mount_state(&spec, backend, true);
            prev
        };
        if let Some(old) = displaced {
            let _ = old.mounted.unmount(true);
        }

        return (VfsResponse::Mounted { mount: info }, false);
    }

    lock_manager(manager).handle_request(req)
}

async fn wait_for_shutdown(shutdown_rx: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = shutdown_rx.wait_for(|&v| v).await;
}

#[cfg(unix)]
async fn wait_for_termination_signal(
    sigterm: &mut tokio::signal::unix::Signal,
    sigint: &mut tokio::signal::unix::Signal,
) {
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
}

async fn serve_ipc_stream<S>(
    stream: S,
    manager: Arc<Mutex<DaemonManager>>,
    shutdown_tx: Arc<tokio::sync::watch::Sender<bool>>,
    required_auth_token: Option<Arc<String>>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if buf.capacity() > 64 * 1024 {
            buf.shrink_to(64 * 1024);
        }
        let read_res = (&mut reader)
            .take((MAX_IPC_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut buf)
            .await;
        match read_res {
            Ok(0) => break,
            Ok(_) => {
                if buf.len() > MAX_IPC_LINE_BYTES {
                    let resp = VfsResponse::Error {
                        code: "frame_too_large".to_string(),
                        message: format!(
                            "IPC request frame exceeds maximum size of {MAX_IPC_LINE_BYTES} bytes"
                        ),
                    };
                    if let Ok(encoded) = encode_line(&resp) {
                        let _ = write_half.write_all(encoded.as_bytes()).await;
                        let _ = write_half.flush().await;
                    }
                    break;
                }
                let Ok(raw_line) = std::str::from_utf8(&buf) else {
                    let resp = VfsResponse::Error {
                        code: "invalid_request".to_string(),
                        message: "IPC request frame is not valid UTF-8".to_string(),
                    };
                    if let Ok(encoded) = encode_line(&resp) {
                        let _ = write_half.write_all(encoded.as_bytes()).await;
                        let _ = write_half.flush().await;
                    }
                    if required_auth_token.is_some() {
                        break;
                    }
                    continue;
                };
                let trimmed = raw_line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(ref expected_token) = required_auth_token {
                    #[derive(serde::Deserialize)]
                    struct AuthHeader<'a> {
                        #[serde(default)]
                        auth_token: Option<std::borrow::Cow<'a, str>>,
                    }
                    let authed = serde_json::from_str::<AuthHeader<'_>>(trimmed)
                        .ok()
                        .and_then(|hdr| hdr.auth_token)
                        .is_some_and(|tok| {
                            constant_time_eq(tok.as_bytes(), expected_token.as_bytes())
                        });
                    if !authed {
                        let resp = VfsResponse::Error {
                            code: "unauthorized".to_string(),
                            message: "missing or invalid IPC authentication token".to_string(),
                        };
                        if let Ok(encoded) = encode_line(&resp) {
                            let _ = write_half.write_all(encoded.as_bytes()).await;
                            let _ = write_half.flush().await;
                        }
                        break;
                    }
                }
                let req: VfsRequest = match serde_json::from_str::<AuthenticatedRequest>(trimmed) {
                    Ok(frame) => frame.request,
                    Err(err) => {
                        let resp = VfsResponse::Error {
                            code: "invalid_request".to_string(),
                            message: err.to_string(),
                        };
                        if let Ok(encoded) = encode_line(&resp) {
                            let _ = write_half.write_all(encoded.as_bytes()).await;
                            let _ = write_half.flush().await;
                        }
                        continue;
                    }
                };
                let mgr_call = Arc::clone(&manager);
                let call_res =
                    tokio::task::spawn_blocking(move || handle_request_concurrent(&mgr_call, req))
                        .await;

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
                    let _ = shutdown_tx.send(true);
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn write_tcp_endpoint_file(socket_path: &Path, endpoint: &str) -> Result<()> {
    use std::io::Write;

    let parent = socket_path.parent().unwrap_or_else(|| Path::new("."));
    ensure_secure_socket_parent(parent)?;

    let file_name = socket_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("vfsd.sock");
    let seq = ENDPOINT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = socket_path.with_file_name(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));

    let write_res: std::io::Result<()> = (|| {
        let mut opts = OpenOptions::new();
        opts.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        #[cfg(windows)]
        {
            win_acl::restrict_file_to_current_user(&file)?;
        }
        file.write_all(endpoint.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, socket_path)?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = std::fs::remove_file(&tmp);
        return Err(OzError::Io(err));
    }
    Ok(())
}

/// Runs the authenticated loopback TCP IPC daemon (`tcp://127.0.0.1:<port>\ntoken=<hex>\n`
/// recorded at `socket_path` with owner-only permissions) until `Shutdown` IPC or Ctrl-C on Windows.
pub async fn run_tcp_daemon(git_dir: &Path, socket_path: &Path) -> Result<()> {
    use tokio::net::TcpListener;

    let (_lock_file, manager) = prepare_daemon_runtime(Some(git_dir), false, socket_path).await?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let auth_token = Arc::new(generate_ipc_auth_token()?);
    let endpoint = format_tcp_endpoint(port, &auth_token);
    write_tcp_endpoint_file(socket_path, &endpoint)?;

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);
    let conn_semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_IPC_CONNECTIONS));

    let ctrl_c_fut = async {
        #[cfg(windows)]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        #[cfg(not(windows))]
        std::future::pending::<()>().await;
    };
    tokio::pin!(ctrl_c_fut);

    loop {
        let permit = tokio::select! {
            () = wait_for_shutdown(&mut shutdown_rx) => break,
            () = &mut ctrl_c_fut => break,
            permit = Arc::clone(&conn_semaphore).acquire_owned() => {
                match permit {
                    Ok(p) => p,
                    Err(_) => break,
                }
            }
        };

        let stream = tokio::select! {
            () = wait_for_shutdown(&mut shutdown_rx) => break,
            () = &mut ctrl_c_fut => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    break;
                };
                stream
            }
        };
        let mgr = Arc::clone(&manager);
        let tx = Arc::clone(&shutdown_tx);
        let tok = Arc::clone(&auth_token);
        tokio::spawn(async move {
            let _permit = permit;
            serve_ipc_stream(stream, mgr, tx, Some(tok)).await;
        });
    }

    drop(listener);
    cleanup_daemon_runtime(socket_path, manager).await;
    Ok(())
}

#[cfg(unix)]
async fn run_unix_daemon_inner(
    git_dir: Option<&Path>,
    global_mode: bool,
    socket_path: &Path,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;
    use tokio::signal::unix::{SignalKind, signal};

    let (_lock_file, manager) = prepare_daemon_runtime(git_dir, global_mode, socket_path).await?;
    // Re-verify the socket parent directory is owner-verified and 0o700 right before binding
    // so there is no window where another UID can traverse into the directory before 0o600 chmod.
    if let Some(parent) = socket_path.parent() {
        ensure_secure_socket_parent(parent)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    // Enforce strict owner-only permissions on the IPC socket and propagate any failure.
    if let Err(err) = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
    {
        drop(listener);
        let _ = std::fs::remove_file(socket_path);
        return Err(OzError::Io(err));
    }
    // SAFETY: `libc::geteuid()` is always safe to call and has no preconditions.
    let expected_uid = unsafe { libc::geteuid() };

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);
    let conn_semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_IPC_CONNECTIONS));
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    loop {
        let permit = tokio::select! {
            () = wait_for_shutdown(&mut shutdown_rx) => break,
            () = wait_for_termination_signal(&mut sigterm, &mut sigint) => break,
            permit = Arc::clone(&conn_semaphore).acquire_owned() => {
                match permit {
                    Ok(p) => p,
                    Err(_) => break,
                }
            }
        };

        let stream = tokio::select! {
            () = wait_for_shutdown(&mut shutdown_rx) => break,
            () = wait_for_termination_signal(&mut sigterm, &mut sigint) => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    break;
                };
                stream
            }
        };

        // Enforce fail-closed SO_PEERCRED / peer UID verification on every connection.
        match stream.peer_cred() {
            Ok(cred) if cred.uid() == expected_uid => {}
            Ok(cred) => {
                eprintln!(
                    "oz-vfsd: rejecting IPC connection from uid {} (expected {expected_uid})",
                    cred.uid()
                );
                continue;
            }
            Err(err) => {
                eprintln!(
                    "oz-vfsd: rejecting IPC connection: failed to query peer credentials: {err}"
                );
                continue;
            }
        }

        let mgr = Arc::clone(&manager);
        let tx = Arc::clone(&shutdown_tx);
        tokio::spawn(async move {
            let _permit = permit;
            serve_ipc_stream(stream, mgr, tx, None).await;
        });
    }

    drop(listener);
    cleanup_daemon_runtime(socket_path, manager).await;
    Ok(())
}

/// Runs the `oz-vfsd` Unix socket daemon for a single repository until `Shutdown` IPC or `SIGTERM`/`SIGINT`.
#[cfg(unix)]
pub async fn run_daemon(git_dir: &Path, socket_path: &Path) -> Result<()> {
    run_unix_daemon_inner(Some(git_dir), false, socket_path).await
}

/// Runs the hardened user-wide `oz-vfsd` Unix socket daemon serving all repositories owned by the current user.
#[cfg(unix)]
pub async fn run_global_daemon(socket_path: &Path) -> Result<()> {
    run_unix_daemon_inner(None, true, socket_path).await
}

/// Runs the `oz-vfsd` Windows loopback TCP + ProjFS daemon until `Shutdown` IPC or Ctrl-C.
#[cfg(windows)]
pub async fn run_daemon(git_dir: &Path, socket_path: &Path) -> Result<()> {
    run_tcp_daemon(git_dir, socket_path).await
}

/// Unsupported platform stub for [`run_global_daemon`] on Windows.
#[cfg(windows)]
pub async fn run_global_daemon(_socket_path: &Path) -> Result<()> {
    Err(OzError::Other(
        "global oz-vfsd mode is only supported on Unix".to_string(),
    ))
}

/// Unsupported platform stub for [`run_daemon`].
#[cfg(not(any(unix, windows)))]
pub async fn run_daemon(_git_dir: &Path, _socket_path: &Path) -> Result<()> {
    Err(OzError::Other(
        "oz-vfsd daemon is only supported on Unix and Windows".to_string(),
    ))
}

/// Unsupported platform stub for [`run_global_daemon`].
#[cfg(not(any(unix, windows)))]
pub async fn run_global_daemon(_socket_path: &Path) -> Result<()> {
    Err(OzError::Other(
        "global oz-vfsd mode is only supported on Unix".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_valid_spec(root: &Path) -> MountSpec {
        let repo_root = root.join("demo");
        let git_dir = repo_root.join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let ws_dir = git_dir.join("ozone/ws/feat-1");
        let upper_dir = ws_dir.join("upper");
        let meta_dir = ws_dir.join("meta");
        std::fs::create_dir_all(&upper_dir).unwrap();
        std::fs::create_dir_all(&meta_dir).unwrap();
        let mount_point = root.join("demo.oz/feat-1");
        std::fs::create_dir_all(&mount_point).unwrap();

        MountSpec {
            workspace: "feat-1".to_string(),
            branch: "feat-1".to_string(),
            repo_root,
            git_dir,
            mount_point,
            upper_dir,
            meta_dir,
            base_tree: None,
            base_commit: None,
            baseline_dir: None,
            allow_other: false,
            writeback_cache: true,
        }
    }

    #[test]
    fn global_mount_validation_accepts_valid_spec_and_blocks_shadowing_or_escapes() {
        let dir = TempDir::new().unwrap();
        let spec = make_valid_spec(dir.path());
        validate_global_mount_spec(&spec, false).expect("valid global mount spec");

        // Reject arbitrary non-*.oz mount points (e.g. ~/.ssh or ~/.local/bin)
        let mut bad_mp = spec.clone();
        bad_mp.mount_point = dir.path().join(".ssh/feat-1");
        std::fs::create_dir_all(&bad_mp.mount_point).unwrap();
        assert!(validate_global_mount_spec(&bad_mp, false).is_err());

        // Reject non-empty mount point directory
        let non_empty = spec.clone();
        std::fs::write(non_empty.mount_point.join("existing.txt"), "data").unwrap();
        assert!(validate_global_mount_spec(&non_empty, false).is_err());
        // Already-mounted workspace is permitted to refresh
        validate_global_mount_spec(&non_empty, true).expect("already mounted allowed");
        std::fs::remove_file(non_empty.mount_point.join("existing.txt")).unwrap();

        // Reject upper_dir outside <git_dir>/ozone/ws/<workspace>/
        let mut bad_upper = spec.clone();
        bad_upper.upper_dir = dir.path().join("elsewhere/upper");
        assert!(validate_global_mount_spec(&bad_upper, false).is_err());

        // Reject fake git_dir missing HEAD / objects
        let mut bad_git = spec.clone();
        bad_git.git_dir = dir.path().join("not-a-git-dir");
        std::fs::create_dir_all(&bad_git.git_dir).unwrap();
        assert!(validate_global_mount_spec(&bad_git, false).is_err());

        // Reject '..' traversal
        let mut dotdot = spec;
        dotdot.mount_point = dir.path().join("demo.oz/../escape");
        assert!(validate_global_mount_spec(&dotdot, false).is_err());
    }

    #[test]
    fn constant_time_eq_and_csprng_token_behave_correctly() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abcdef0123456789", b"abcdef0123456789"));
        assert!(!constant_time_eq(b"abcdef0123456789", b"bbcdef0123456789"));
        assert!(!constant_time_eq(b"abcdef0123456789", b"abcdef0123456788"));
        assert!(!constant_time_eq(b"abcdef", b"abcde"));

        let t1 = generate_ipc_auth_token().expect("token 1");
        let t2 = generate_ipc_auth_token().expect("token 2");
        assert_eq!(t1.len(), 64);
        assert_eq!(t2.len(), 64);
        assert!(t1.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_ne!(t1, t2);
    }

    #[cfg(unix)]
    #[test]
    fn global_mount_validation_blocks_symlink_bypasses_and_invalid_baseline_dir() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let spec = make_valid_spec(dir.path());
        validate_global_mount_spec(&spec, false).expect("baseline valid spec");

        let outside = dir.path().join("outside-target");
        std::fs::create_dir_all(&outside).unwrap();

        // 1. Symlink at upper_dir leaf pointing outside <git_dir>/ozone/ws/<ws>/
        {
            let dir2 = TempDir::new().unwrap();
            let s = make_valid_spec(dir2.path());
            std::fs::remove_dir(&s.upper_dir).unwrap();
            symlink(&outside, &s.upper_dir).unwrap();
            assert!(
                validate_global_mount_spec(&s, false).is_err(),
                "upper_dir symlink pointing outside must be rejected"
            );
        }

        // 2. Dangling symlink at upper_dir leaf
        {
            let dir2 = TempDir::new().unwrap();
            let s = make_valid_spec(dir2.path());
            std::fs::remove_dir(&s.upper_dir).unwrap();
            symlink(dir2.path().join("nonexistent"), &s.upper_dir).unwrap();
            assert!(
                validate_global_mount_spec(&s, false).is_err(),
                "dangling upper_dir symlink must be rejected"
            );
        }

        // 3. Symlink at meta_dir leaf pointing inside the same workspace dir (still a symlink!)
        {
            let dir2 = TempDir::new().unwrap();
            let s = make_valid_spec(dir2.path());
            let alt = s.meta_dir.parent().unwrap().join("real_meta");
            std::fs::create_dir_all(&alt).unwrap();
            std::fs::remove_dir(&s.meta_dir).unwrap();
            symlink(&alt, &s.meta_dir).unwrap();
            assert!(
                validate_global_mount_spec(&s, false).is_err(),
                "meta_dir symlink must be rejected even when pointing inside workspace dir"
            );
        }

        // 4. Symlink at intermediate ancestor (`<git_dir>/ozone/ws` -> outside) with non-existent leaf
        {
            let dir2 = TempDir::new().unwrap();
            let s = make_valid_spec(dir2.path());
            let ws_parent = s.git_dir.join("ozone/ws");
            std::fs::remove_dir_all(&ws_parent).unwrap();
            let fake_ws = dir2.path().join("fake_ws");
            std::fs::create_dir_all(&fake_ws).unwrap();
            symlink(&fake_ws, &ws_parent).unwrap();
            assert!(
                validate_global_mount_spec(&s, false).is_err(),
                "symlinked ancestor of upper_dir/meta_dir must be rejected"
            );
        }

        // 5. Symlink at mount_point parent (`demo.oz` -> outside)
        {
            let dir2 = TempDir::new().unwrap();
            let s = make_valid_spec(dir2.path());
            let oz_container = s.mount_point.parent().unwrap();
            std::fs::remove_dir_all(oz_container).unwrap();
            let victim = dir2.path().join("victim");
            std::fs::create_dir_all(victim.join("feat-1")).unwrap();
            symlink(&victim, oz_container).unwrap();
            assert!(
                validate_global_mount_spec(&s, false).is_err(),
                "symlinked *.oz parent of mount_point must be rejected"
            );
        }

        // 6. Valid baseline_dir (owned real directory) is accepted
        let valid_base = dir.path().join("owned-baseline");
        std::fs::create_dir_all(&valid_base).unwrap();
        let mut with_valid_base = spec.clone();
        with_valid_base.baseline_dir = Some(valid_base.clone());
        validate_global_mount_spec(&with_valid_base, false)
            .expect("owned real baseline_dir must be accepted");

        // 7. Relative baseline_dir rejected
        let mut rel_base = spec.clone();
        rel_base.baseline_dir = Some(PathBuf::from("relative/baseline"));
        assert!(validate_global_mount_spec(&rel_base, false).is_err());

        // 8. baseline_dir with '..' rejected
        let mut dotdot_base = spec.clone();
        dotdot_base.baseline_dir = Some(valid_base.join("../outside-target"));
        assert!(validate_global_mount_spec(&dotdot_base, false).is_err());

        // 9. Symlink baseline_dir leaf rejected
        let base_link = dir.path().join("baseline-link");
        symlink(&valid_base, &base_link).unwrap();
        let mut sym_base = spec.clone();
        sym_base.baseline_dir = Some(base_link);
        assert!(validate_global_mount_spec(&sym_base, false).is_err());

        // 10. Symlink ancestor of baseline_dir rejected
        let mut sym_ancestor_base = spec.clone();
        sym_ancestor_base.baseline_dir = Some(dir.path().join("baseline-link/sub"));
        assert!(validate_global_mount_spec(&sym_ancestor_base, false).is_err());

        // 11. Dangling symlink baseline_dir rejected
        let dangling_base = dir.path().join("dangling-baseline");
        symlink(dir.path().join("no-such-dir"), &dangling_base).unwrap();
        let mut dangling_spec = spec.clone();
        dangling_spec.baseline_dir = Some(dangling_base);
        assert!(validate_global_mount_spec(&dangling_spec, false).is_err());

        // 12. Regular file as baseline_dir rejected
        let file_base = dir.path().join("not-a-dir.txt");
        std::fs::write(&file_base, b"hello").unwrap();
        let mut file_spec = spec.clone();
        file_spec.baseline_dir = Some(file_base);
        assert!(validate_global_mount_spec(&file_spec, false).is_err());

        // 13. Non-UID-owned baseline_dir (e.g. root-owned `/etc` or `/etc/nonexistent`) rejected
        // SAFETY: `libc::geteuid()` is always safe to call and has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            let mut root_base = spec.clone();
            root_base.baseline_dir = Some(PathBuf::from("/etc"));
            assert!(validate_global_mount_spec(&root_base, false).is_err());

            let mut root_ancestor_base = spec;
            root_ancestor_base.baseline_dir = Some(PathBuf::from("/etc/ozone-nonexistent"));
            assert!(validate_global_mount_spec(&root_ancestor_base, false).is_err());
        }
    }

    #[tokio::test]
    async fn tcp_daemon_requires_valid_auth_token_and_rejects_unauthenticated_requests() {
        use crate::client::send_tcp_request;
        use crate::protocol::parse_tcp_endpoint;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = TempDir::new().unwrap();
        let git_dir = dir.path().join("tcp-auth/.git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let sock_path = git_dir.join("ozone/vfsd.sock");

        let git_srv = git_dir.clone();
        let sock_srv = sock_path.clone();
        let server_task = tokio::spawn(async move { run_tcp_daemon(&git_srv, &sock_srv).await });

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !sock_path.is_file() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for endpoint file"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let raw_endpoint = std::fs::read_to_string(&sock_path).expect("read endpoint");
        let parsed = parse_tcp_endpoint(&raw_endpoint).expect("parse endpoint");
        assert_eq!(parsed.auth_token.len(), 64);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = std::fs::symlink_metadata(&sock_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600, "endpoint file must be mode 0600");
            let dir_mode = std::fs::symlink_metadata(sock_path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700, "endpoint parent dir must be mode 0700");
        }

        // Helper sending one raw JSON line over TCP and returning the parsed VfsResponse.
        async fn send_raw_line(addr: std::net::SocketAddr, line: &str) -> VfsResponse {
            let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
            stream.write_all(line.as_bytes()).await.expect("write");
            stream.flush().await.expect("flush");
            let mut reader = BufReader::new(stream);
            let mut resp_line = String::new();
            let n = reader.read_line(&mut resp_line).await.expect("read");
            assert!(n > 0, "expected structured error response");
            serde_json::from_str(resp_line.trim()).expect("parse VfsResponse")
        }

        // 1. Unauthenticated Ping, ListMounts, and Shutdown must be rejected as unauthorized.
        for bare_req in [
            VfsRequest::Ping,
            VfsRequest::ListMounts,
            VfsRequest::Shutdown,
        ] {
            let raw = encode_line(&bare_req).unwrap();
            match send_raw_line(parsed.addr, &raw).await {
                VfsResponse::Error { code, .. } => assert_eq!(code, "unauthorized"),
                other => panic!("expected unauthorized for bare {bare_req:?}, got {other:?}"),
            }
        }

        // 2. Wrong token (1 hex char flipped), empty token, and truncated token must be rejected.
        let mut flipped = parsed.auth_token.clone();
        let last = flipped.pop().unwrap();
        flipped.push(if last == '0' { '1' } else { '0' });
        for bad_tok in [flipped, String::new(), parsed.auth_token[..16].to_string()] {
            let bad_line = encode_line(&AuthenticatedRequest::with_token(
                VfsRequest::Shutdown,
                bad_tok,
            ))
            .unwrap();
            match send_raw_line(parsed.addr, &bad_line).await {
                VfsResponse::Error { code, .. } => assert_eq!(code, "unauthorized"),
                other => panic!("expected unauthorized for bad token, got {other:?}"),
            }
        }

        // 3. Authenticated client requests still succeed (proving unauthorized Shutdown did not stop daemon).
        let sock_for_client = sock_path.clone();
        let ping_resp = tokio::task::spawn_blocking(move || {
            send_tcp_request(&sock_for_client, &VfsRequest::Ping)
        })
        .await
        .unwrap()
        .expect("authenticated ping");
        assert!(matches!(ping_resp, VfsResponse::Pong { .. }));

        // 4. Authenticated Shutdown cleanly stops the daemon.
        let sock_for_shutdown = sock_path.clone();
        let shutdown_resp = tokio::task::spawn_blocking(move || {
            send_tcp_request(&sock_for_shutdown, &VfsRequest::Shutdown)
        })
        .await
        .unwrap()
        .expect("authenticated shutdown");
        assert!(matches!(shutdown_resp, VfsResponse::ShuttingDown));

        server_task
            .await
            .expect("join tcp daemon")
            .expect("tcp daemon clean exit");
    }

    #[tokio::test]
    async fn serve_ipc_stream_rejects_oversized_frames_with_structured_error() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = TempDir::new().unwrap();
        let sock_path = dir.path().join("vfsd.sock");
        let manager = Arc::new(Mutex::new(DaemonManager::new(None, false, sock_path)));
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let shutdown_tx = Arc::new(shutdown_tx);

        // 1. Oversized UTF-8 frame (> MAX_IPC_LINE_BYTES) returns structured `frame_too_large` error.
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let srv_task = tokio::spawn(serve_ipc_stream(
            server,
            Arc::clone(&manager),
            Arc::clone(&shutdown_tx),
            None,
        ));
        let mut oversized = vec![b'a'; MAX_IPC_LINE_BYTES + 128];
        oversized.push(b'\n');
        let writer = tokio::spawn(async move {
            let _ = client.write_all(&oversized).await;
            let _ = client.flush().await;
            let mut reader = BufReader::new(client);
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.expect("read error line");
            assert!(n > 0);
            let resp: VfsResponse = serde_json::from_str(line.trim()).expect("parse error json");
            assert!(
                matches!(resp, VfsResponse::Error { ref code, .. } if code == "frame_too_large")
            );
            // Connection must close after oversized frame error.
            let mut extra = String::new();
            assert_eq!(reader.read_line(&mut extra).await.unwrap(), 0);
        });
        writer.await.expect("writer task");
        srv_task.await.expect("server task");

        // 2. Oversized non-UTF-8 binary payload also returns structured `frame_too_large` error.
        let (mut client2, server2) = tokio::io::duplex(64 * 1024);
        let srv_task2 = tokio::spawn(serve_ipc_stream(
            server2,
            Arc::clone(&manager),
            Arc::clone(&shutdown_tx),
            Some(Arc::new("secret".to_string())),
        ));
        let mut oversized_bin = vec![0xFFu8; MAX_IPC_LINE_BYTES + 64];
        oversized_bin.push(b'\n');
        let writer2 = tokio::spawn(async move {
            let _ = client2.write_all(&oversized_bin).await;
            let _ = client2.flush().await;
            let mut reader = BufReader::new(client2);
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.expect("read error line");
            assert!(n > 0);
            let resp: VfsResponse = serde_json::from_str(line.trim()).expect("parse error json");
            assert!(
                matches!(resp, VfsResponse::Error { ref code, .. } if code == "frame_too_large")
            );
        });
        writer2.await.expect("writer2 task");
        srv_task2.await.expect("server2 task");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unix_daemon_enforces_parent_ownership_permissions_and_concurrent_signals() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        // 1. Symlinked socket parent directory must be rejected before binding.
        let dir = TempDir::new().unwrap();
        let real_parent = dir.path().join("real");
        std::fs::create_dir_all(&real_parent).unwrap();
        let sym_parent = dir.path().join("sym");
        symlink(&real_parent, &sym_parent).unwrap();
        assert!(ensure_secure_socket_parent(&sym_parent).is_err());

        // 2. Root-owned parent directory (e.g. `/tmp`) must be rejected for non-root UIDs.
        // SAFETY: `libc::geteuid()` is always safe to call and has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            assert!(ensure_secure_socket_parent(Path::new("/tmp")).is_err());
        }

        // 3. Two concurrent Unix daemons in the same process must run and shut down cleanly
        // without racing on any process-global signal FD, and enforce 0700 parent / 0600 socket.
        let g1 = dir.path().join("r1/.git");
        let g2 = dir.path().join("r2/.git");
        std::fs::create_dir_all(&g1).unwrap();
        std::fs::create_dir_all(&g2).unwrap();
        let s1 = g1.join("ozone/vfsd.sock");
        let s2 = g2.join("ozone/vfsd.sock");

        let (g1c, s1c) = (g1.clone(), s1.clone());
        let (g2c, s2c) = (g2.clone(), s2.clone());
        let t1 = tokio::spawn(async move { run_daemon(&g1c, &s1c).await });
        let t2 = tokio::spawn(async move { run_daemon(&g2c, &s2c).await });

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        for sock in [&s1, &s2] {
            loop {
                let sock_ping = sock.clone();
                let ok =
                    tokio::task::spawn_blocking(move || crate::client::ping_daemon(&sock_ping))
                        .await
                        .unwrap()
                        .is_ok();
                if ok {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "daemon did not bind {}",
                    sock.display()
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let sock_mode = std::fs::symlink_metadata(sock)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            let parent_mode = std::fs::symlink_metadata(sock.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(sock_mode, 0o600);
            assert_eq!(parent_mode, 0o700);
        }

        // Burst more concurrent connections than MAX_CONCURRENT_IPC_CONNECTIONS to verify
        // Semaphore-bounded serving handles bursts smoothly without errors.
        let mut burst = Vec::new();
        for _ in 0..(MAX_CONCURRENT_IPC_CONNECTIONS + 16) {
            let s1_clone = s1.clone();
            burst.push(tokio::task::spawn_blocking(move || {
                crate::client::ping_daemon(&s1_clone)
            }));
        }
        for h in burst {
            h.await.unwrap().expect("burst ping succeeded");
        }

        let (s1_sd, s2_sd) = (s1.clone(), s2.clone());
        tokio::task::spawn_blocking(move || {
            let _ = crate::client::send_request(&s1_sd, &VfsRequest::Shutdown);
            let _ = crate::client::send_request(&s2_sd, &VfsRequest::Shutdown);
        })
        .await
        .unwrap();

        t1.await.unwrap().expect("daemon 1 clean exit");
        t2.await.unwrap().expect("daemon 2 clean exit");
    }

    #[test]
    fn lock_manager_recovers_gracefully_after_mutex_poisoning() {
        let dir = TempDir::new().unwrap();
        let manager = Arc::new(Mutex::new(DaemonManager::new(
            None,
            false,
            dir.path().join("vfsd.sock"),
        )));

        let poisoned = Arc::clone(&manager);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("simulated panic while holding daemon lock");
        }));
        assert!(manager.is_poisoned(), "mutex should be poisoned");

        let (resp, should_shutdown) = handle_request_concurrent(&manager, VfsRequest::Ping);
        assert!(!should_shutdown);
        assert!(matches!(resp, VfsResponse::Pong { .. }));
    }
}
