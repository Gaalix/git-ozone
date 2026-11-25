// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Local IPC server (`oz-vfsd`) managing CoW FUSE3 (Linux) and ProjFS (Windows) mounts.

#[cfg(not(target_os = "windows"))]
use crate::overlay::{MountedOverlay, mount_overlay};
use crate::overlay::{OverlayMountConfig, lazy_unmount_fuse, scan_dirty_paths};
#[cfg(target_os = "windows")]
use crate::projfs::{MountedProjFs, mount_projfs};
use crate::protocol::{MountInfo, MountSpec, VfsRequest, VfsResponse, encode_line};
use chrono::Utc;
use fs4::{FileExt as Fs4FileExt, TryLockError};
use oz_core::{
    BackendKind, MountRecord, OzError, OzoneConfig, OzoneLayout, Result, WorkspaceState,
    normalize_lexical, validate_workspace_name,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};
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

/// Validates a [`MountSpec`] received by the hardened user-wide global `oz-vfsd` daemon:
/// - `workspace` must be a valid Ozone workspace name (no path traversal).
/// - `git_dir` must be an existing UID-owned Git directory (`HEAD` + `objects/`).
/// - `repo_root` must be an existing UID-owned directory.
/// - `upper_dir` and `meta_dir` must live under `<git_dir>/ozone/ws/<workspace>/`.
/// - `mount_point` must be absolute, free of `..`, reside inside an `*.oz/<workspace>` (or `<git_dir>/ozone/`)
///   directory, never be a symlink, and be empty (unless `already_mounted_here` is `true` or stale `ENOTCONN`).
pub fn validate_global_mount_spec(spec: &MountSpec, already_mounted_here: bool) -> Result<()> {
    validate_workspace_name(&spec.workspace)?;

    for (label, p) in [
        ("git_dir", &spec.git_dir),
        ("repo_root", &spec.repo_root),
        ("mount_point", &spec.mount_point),
        ("upper_dir", &spec.upper_dir),
        ("meta_dir", &spec.meta_dir),
    ] {
        if !p.is_absolute() {
            return Err(OzError::GuardrailViolation {
                kind: "global_vfsd_validation".to_string(),
                message: format!("{label} '{}' must be an absolute path", p.display()),
                recovery: Vec::new(),
            });
        }
        if p.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(OzError::GuardrailViolation {
                kind: "global_vfsd_validation".to_string(),
                message: format!("{label} '{}' must not contain '..' segments", p.display()),
                recovery: Vec::new(),
            });
        }
    }

    verify_owned_dir(&spec.git_dir, "git_dir")?;
    if !spec.git_dir.join("HEAD").is_file() || !spec.git_dir.join("objects").is_dir() {
        return Err(OzError::GuardrailViolation {
            kind: "global_vfsd_validation".to_string(),
            message: format!(
                "git_dir '{}' is not a valid Git directory (missing HEAD or objects/)",
                spec.git_dir.display()
            ),
            recovery: Vec::new(),
        });
    }
    verify_owned_dir(&spec.repo_root, "repo_root")?;

    let canon_git = canonical_git_key(&spec.git_dir);
    let expected_ws_meta_root = canon_git.join("ozone").join("ws").join(&spec.workspace);
    let upper_norm = normalize_lexical(&spec.upper_dir);
    let meta_norm = normalize_lexical(&spec.meta_dir);
    let upper_canon_ok = upper_norm.starts_with(&expected_ws_meta_root)
        || normalize_lexical(&spec.upper_dir)
            .starts_with(normalize_lexical(&spec.git_dir).join("ozone").join("ws").join(&spec.workspace));
    let meta_canon_ok = meta_norm.starts_with(&expected_ws_meta_root)
        || normalize_lexical(&spec.meta_dir)
            .starts_with(normalize_lexical(&spec.git_dir).join("ozone").join("ws").join(&spec.workspace));
    if !upper_canon_ok || !meta_canon_ok {
        return Err(OzError::GuardrailViolation {
            kind: "global_vfsd_validation".to_string(),
            message: format!(
                "upper_dir/meta_dir must reside under '<git_dir>/ozone/ws/{}/'",
                spec.workspace
            ),
            recovery: Vec::new(),
        });
    }

    let mp_name = spec
        .mount_point
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let parent_name = spec
        .mount_point
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let inside_oz_container = mp_name == spec.workspace && parent_name.ends_with(".oz");
    let inside_git_ozone = normalize_lexical(&spec.mount_point)
        .starts_with(normalize_lexical(&spec.git_dir).join("ozone"));
    if !inside_oz_container && !inside_git_ozone {
        return Err(OzError::GuardrailViolation {
            kind: "global_vfsd_validation".to_string(),
            message: format!(
                "refusing mount_point '{}': global oz-vfsd only mounts into '*.oz/<workspace>' directories",
                spec.mount_point.display()
            ),
            recovery: Vec::new(),
        });
    }

    match std::fs::symlink_metadata(&spec.mount_point) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(OzError::GuardrailViolation {
                    kind: "global_vfsd_validation".to_string(),
                    message: format!(
                        "mount_point '{}' must be a regular directory (not a symlink or file)",
                        spec.mount_point.display()
                    ),
                    recovery: Vec::new(),
                });
            }
            verify_uid_owner(&meta, "mount_point", &spec.mount_point)?;
            if !already_mounted_here
                && std::fs::read_dir(&spec.mount_point)
                    .map(|mut it| it.next().is_some())
                    .unwrap_or(true)
            {
                return Err(OzError::GuardrailViolation {
                    kind: "global_vfsd_validation".to_string(),
                    message: format!(
                        "refusing to mount over non-empty directory '{}'",
                        spec.mount_point.display()
                    ),
                    recovery: Vec::new(),
                });
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound || oz_core::is_enotconn(&err) => {
            if let Some(parent) = spec.mount_point.parent()
                && parent.exists()
            {
                verify_owned_dir(parent, "mount_point parent")?;
            }
        }
        Err(err) => return Err(OzError::Io(err)),
    }

    Ok(())
}

fn verify_owned_dir(path: &Path, label: &str) -> Result<()> {
    let meta = std::fs::symlink_metadata(path).map_err(|err| OzError::GuardrailViolation {
        kind: "global_vfsd_validation".to_string(),
        message: format!("{label} '{}' is inaccessible: {err}", path.display()),
        recovery: Vec::new(),
    })?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(OzError::GuardrailViolation {
            kind: "global_vfsd_validation".to_string(),
            message: format!("{label} '{}' must be a real directory", path.display()),
            recovery: Vec::new(),
        });
    }
    verify_uid_owner(&meta, label, path)
}

fn verify_uid_owner(meta: &std::fs::Metadata, label: &str, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let my_uid = unsafe { libc::geteuid() };
        if meta.uid() != my_uid {
            return Err(OzError::GuardrailViolation {
                kind: "global_vfsd_validation".to_string(),
                message: format!(
                    "{label} '{}' is owned by uid {}, expected current uid {my_uid}",
                    path.display(),
                    meta.uid()
                ),
                recovery: Vec::new(),
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (meta, label, path);
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
                libc::sigaction(
                    libc::SIGTERM,
                    &raw const self.prev_term,
                    std::ptr::null_mut(),
                );
                libc::sigaction(libc::SIGINT, &raw const self.prev_int, std::ptr::null_mut());
            }
        }
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
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            if parent != ozone_dir {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        acquire_daemon_lock(gd)?
    } else {
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
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
            mgr.lock()
                .expect("daemon lock poisoned")
                .restore_mounts_on_startup();
        })
        .await
        .map_err(|e| OzError::Other(format!("startup restore panicked: {e}")))?;
    }

    Ok((lock_file, manager))
}

async fn cleanup_daemon_runtime(socket_path: &Path, manager: Arc<Mutex<DaemonManager>>) {
    let _ = tokio::task::spawn_blocking(move || {
        manager
            .lock()
            .expect("daemon lock poisoned")
            .unmount_all_clean();
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
            let mut guard = manager.lock().expect("daemon lock poisoned");
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
            if global_mode
                && let Err(err) = validate_global_mount_spec(&spec, already_mounted_here)
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
            let mut guard = manager.lock().expect("daemon lock poisoned");
            let prev = guard.mounts.insert(key, active);
            guard.persist_mount_state(&spec, backend, true);
            prev
        };
        if let Some(old) = displaced {
            let _ = old.mounted.unmount(true);
        }

        return (VfsResponse::Mounted { mount: info }, false);
    }

    manager
        .lock()
        .expect("daemon lock poisoned")
        .handle_request(req)
}

async fn wait_for_shutdown(shutdown_rx: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = shutdown_rx.wait_for(|&v| v).await;
}

async fn serve_ipc_stream<S>(
    stream: S,
    manager: Arc<Mutex<DaemonManager>>,
    shutdown_tx: Arc<tokio::sync::watch::Sender<bool>>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read_half, mut write_half) = tokio::io::split(stream);
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

fn write_tcp_endpoint_file(socket_path: &Path, endpoint: &str) -> std::io::Result<()> {
    let parent = socket_path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = socket_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("vfsd.sock");
    let tmp = socket_path.with_file_name(format!(".{file_name}.tmp.{}", std::process::id()));
    std::fs::write(&tmp, endpoint.as_bytes())?;
    std::fs::rename(&tmp, socket_path)?;
    Ok(())
}

#[cfg(windows)]
mod win_signal {
    use std::sync::atomic::{AtomicBool, Ordering};

    static CTRL_RECEIVED: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler_routine: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }

    unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> i32 {
        CTRL_RECEIVED.store(true, Ordering::SeqCst);
        1
    }

    pub(super) struct WinCtrlWatcher;

    impl WinCtrlWatcher {
        pub(super) fn install() -> Self {
            CTRL_RECEIVED.store(false, Ordering::SeqCst);
            unsafe {
                let _ = SetConsoleCtrlHandler(Some(ctrl_handler), 1);
            }
            Self
        }

        pub(super) async fn wait(&self) {
            while !CTRL_RECEIVED.load(Ordering::Acquire) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }

    impl Drop for WinCtrlWatcher {
        fn drop(&mut self) {
            unsafe {
                let _ = SetConsoleCtrlHandler(Some(ctrl_handler), 0);
            }
        }
    }
}

/// Runs the un-gated loopback TCP IPC daemon (`tcp://127.0.0.1:<port>\n` recorded at
/// `socket_path`) until `Shutdown` IPC (or console Ctrl-C on Windows).
pub async fn run_tcp_daemon(git_dir: &Path, socket_path: &Path) -> Result<()> {
    use tokio::net::TcpListener;

    let (_lock_file, manager) = prepare_daemon_runtime(Some(git_dir), false, socket_path).await?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    write_tcp_endpoint_file(socket_path, &format!("tcp://127.0.0.1:{port}\n"))?;

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);
    #[cfg(windows)]
    let ctrl_watcher = win_signal::WinCtrlWatcher::install();
    let ctrl_c_fut = async {
        #[cfg(windows)]
        ctrl_watcher.wait().await;
        #[cfg(not(windows))]
        std::future::pending::<()>().await;
    };
    tokio::pin!(ctrl_c_fut);

    loop {
        tokio::select! {
            () = wait_for_shutdown(&mut shutdown_rx) => {
                break;
            }
            () = &mut ctrl_c_fut => {
                break;
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    break;
                };
                let mgr = Arc::clone(&manager);
                let tx = Arc::clone(&shutdown_tx);
                tokio::spawn(serve_ipc_stream(stream, mgr, tx));
            }
        }
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

    let (_lock_file, manager) = prepare_daemon_runtime(git_dir, global_mode, socket_path).await?;
    let listener = UnixListener::bind(socket_path)?;
    // Enforce strict owner-only permissions on the IPC socket.
    let _ = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600));
    let expected_uid = unsafe { libc::geteuid() };

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);
    let signal_watcher = unix_signal::SignalWatcher::install()?;

    loop {
        tokio::select! {
            () = wait_for_shutdown(&mut shutdown_rx) => {
                break;
            }
            () = signal_watcher.wait_signal() => {
                break;
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else {
                    break;
                };
                // Enforce SO_PEERCRED / peer UID verification on every connection.
                if let Ok(cred) = stream.peer_cred()
                    && cred.uid() != expected_uid
                {
                    eprintln!(
                        "oz-vfsd: rejecting IPC connection from uid {} (expected {expected_uid})",
                        cred.uid()
                    );
                    continue;
                }
                let mgr = Arc::clone(&manager);
                let tx = Arc::clone(&shutdown_tx);
                tokio::spawn(serve_ipc_stream(stream, mgr, tx));
            }
        }
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
}
