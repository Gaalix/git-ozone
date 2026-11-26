// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Concrete [`WorkspaceBackend`] implementations across APFS, Linux `FICLONE`, Windows `ReFS`,
//! Linux FUSE3 / Windows `ProjFS` (`oz-vfsd`), and universal CAS hardlink fallback.

use crate::detect::detect_filesystem;
use crate::materialize::{
    apply_tree_delta_to_dir, clone_dir_with_reflink, compute_tree_delta, materialize_cas_workspace,
    materialize_tree_from_odb, resolve_baseline_tree_id, resolve_git_common_dir,
};
use crate::{BackendHealth, SpawnRequest, SpawnedWorkspace, WorkspaceBackend};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use chrono::Utc;
use oz_core::{BackendKind, OzError, Result};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use oz_core::{MountHealth, MountRecord, OzoneLayout, OzoneRepo, probe_mount_health_os};
#[cfg(target_os = "linux")]
use oz_vfsd::lazy_unmount_fuse;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use oz_vfsd::{MountSpec, mount_via_daemon_with_auto_start, ping_daemon, unmount_via_daemon};
use std::path::{Path, PathBuf};

/// Returns the concrete [`WorkspaceBackend`] implementation for `kind` on the current OS.
pub fn backend_for_kind(kind: BackendKind) -> Result<Box<dyn WorkspaceBackend>> {
    match kind {
        #[cfg(target_os = "macos")]
        BackendKind::ApfsClonefile => Ok(Box::new(ClonefileBackend)),
        #[cfg(not(target_os = "macos"))]
        BackendKind::ApfsClonefile => Err(OzError::InvalidUsage {
            message: "apfs_clonefile backend is only available on macOS".to_string(),
            recovery: Vec::new(),
        }),

        #[cfg(target_os = "linux")]
        BackendKind::LinuxFiclone => Ok(Box::new(FicloneBackend)),
        #[cfg(not(target_os = "linux"))]
        BackendKind::LinuxFiclone => Err(OzError::InvalidUsage {
            message: "linux_ficlone backend is only available on Linux".to_string(),
            recovery: Vec::new(),
        }),

        #[cfg(target_os = "linux")]
        BackendKind::LinuxFuse3 => Ok(Box::new(Fuse3VfsBackend::default())),
        #[cfg(not(target_os = "linux"))]
        BackendKind::LinuxFuse3 => Err(OzError::InvalidUsage {
            message: "linux_fuse3 backend is only available on Linux".to_string(),
            recovery: Vec::new(),
        }),

        #[cfg(target_os = "windows")]
        BackendKind::WindowsRefsClone => Ok(Box::new(RefsCloneBackend)),
        #[cfg(not(target_os = "windows"))]
        BackendKind::WindowsRefsClone => Err(OzError::InvalidUsage {
            message: "windows_refs_clone backend is only available on Windows".to_string(),
            recovery: Vec::new(),
        }),

        #[cfg(target_os = "windows")]
        BackendKind::WindowsProjFs => Ok(Box::new(ProjFsVfsBackend::default())),
        #[cfg(not(target_os = "windows"))]
        BackendKind::WindowsProjFs => Err(OzError::InvalidUsage {
            message: "windows_proj_fs backend is only available on Windows".to_string(),
            recovery: Vec::new(),
        }),

        BackendKind::CasHardlink => Ok(Box::new(CasHardlinkBackend)),
    }
}

/// macOS APFS native CoW backend using `clonefile(2)` / `reflink-copy`.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, Default)]
pub struct ClonefileBackend;

#[cfg(target_os = "macos")]
impl WorkspaceBackend for ClonefileBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::ApfsClonefile
    }

    fn is_available(&self, repo_root: &Path) -> Result<bool> {
        Ok(detect_filesystem(repo_root)?.reflink_supported)
    }

    fn spawn(&self, req: &SpawnRequest) -> Result<SpawnedWorkspace> {
        if let Some(ref base) = req.baseline_dir
            && base.is_dir()
            && !req.target_path.exists()
            && try_macos_clonefile_dir(base, &req.target_path).is_ok()
        {
            reconcile_reflink_delta(req, base)?;
            return Ok(spawned_non_vfs(BackendKind::ApfsClonefile, req));
        }
        spawn_reflink_workspace(BackendKind::ApfsClonefile, req)
    }

    fn destroy(&self, workspace_path: &Path) -> Result<()> {
        remove_dir_if_exists(workspace_path)
    }

    fn health_check(&self, workspace_path: &Path) -> Result<BackendHealth> {
        Ok(simple_dir_health(BackendKind::ApfsClonefile, workspace_path))
    }
}

#[cfg(target_os = "macos")]
fn try_macos_clonefile_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let c_src = CString::new(src.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let c_dst = CString::new(dst.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `c_src` and `c_dst` are valid NUL-terminated C strings passed to macOS `clonefile(2)`.
    let rc = unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Linux Btrfs / XFS / bcachefs native kernel reflink backend via `ioctl(FICLONE)`.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, Default)]
pub struct FicloneBackend;

#[cfg(target_os = "linux")]
impl WorkspaceBackend for FicloneBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::LinuxFiclone
    }

    fn is_available(&self, repo_root: &Path) -> Result<bool> {
        Ok(detect_filesystem(repo_root)?.reflink_supported)
    }

    fn spawn(&self, req: &SpawnRequest) -> Result<SpawnedWorkspace> {
        spawn_reflink_workspace(BackendKind::LinuxFiclone, req)
    }

    fn destroy(&self, workspace_path: &Path) -> Result<()> {
        remove_dir_if_exists(workspace_path)
    }

    fn health_check(&self, workspace_path: &Path) -> Result<BackendHealth> {
        Ok(simple_dir_health(BackendKind::LinuxFiclone, workspace_path))
    }
}

/// Windows `ReFS` / Dev Drive native block-cloning backend via `FSCTL_DUPLICATE_EXTENTS_TO_FILE`.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, Default)]
pub struct RefsCloneBackend;

#[cfg(target_os = "windows")]
impl WorkspaceBackend for RefsCloneBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::WindowsRefsClone
    }

    fn is_available(&self, repo_root: &Path) -> Result<bool> {
        Ok(detect_filesystem(repo_root)?.reflink_supported)
    }

    fn spawn(&self, req: &SpawnRequest) -> Result<SpawnedWorkspace> {
        spawn_reflink_workspace(BackendKind::WindowsRefsClone, req)
    }

    fn destroy(&self, workspace_path: &Path) -> Result<()> {
        remove_dir_if_exists(workspace_path)
    }

    fn health_check(&self, workspace_path: &Path) -> Result<BackendHealth> {
        Ok(simple_dir_health(BackendKind::WindowsRefsClone, workspace_path))
    }
}

/// Linux `ext4` unprivileged FUSE3 `CoW` overlay backend powered by `oz-vfsd`.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct Fuse3VfsBackend {
    pub allow_other: bool,
    pub writeback_cache: bool,
    pub auto_start: bool,
}

#[cfg(target_os = "linux")]
impl Default for Fuse3VfsBackend {
    fn default() -> Self {
        Self {
            allow_other: false,
            writeback_cache: true,
            auto_start: true,
        }
    }
}

#[cfg(target_os = "linux")]
impl Fuse3VfsBackend {
    /// Creates a configured [`Fuse3VfsBackend`].
    #[must_use]
    pub const fn new(allow_other: bool, writeback_cache: bool, auto_start: bool) -> Self {
        Self {
            allow_other,
            writeback_cache,
            auto_start,
        }
    }

    /// Unmounts and cleans up a FUSE3 workspace when `git_common_dir` and `workspace` are known.
    pub(crate) fn destroy_with_context(
        &self,
        git_common_dir: &Path,
        workspace: &str,
        workspace_path: &Path,
    ) {
        let _ = self;
        destroy_daemon_vfs_workspace(
            git_common_dir,
            workspace,
            workspace_path,
            BackendKind::LinuxFuse3,
        );
    }
}

#[cfg(target_os = "linux")]
impl WorkspaceBackend for Fuse3VfsBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::LinuxFuse3
    }

    fn is_available(&self, repo_root: &Path) -> Result<bool> {
        Ok(detect_filesystem(repo_root)?
            .fuse
            .as_ref()
            .is_some_and(|f| f.ready))
    }

    fn spawn(&self, req: &SpawnRequest) -> Result<SpawnedWorkspace> {
        spawn_daemon_vfs_workspace(
            req,
            BackendKind::LinuxFuse3,
            self.allow_other,
            self.writeback_cache,
            self.auto_start,
        )
    }

    fn destroy(&self, workspace_path: &Path) -> Result<()> {
        if let Ok(repo) = OzoneRepo::discover(Some(workspace_path))
            && let Some(ws_name) = repo.workspace.as_deref()
        {
            self.destroy_with_context(&repo.git_common_dir, ws_name, workspace_path);
            return Ok(());
        }

        if probe_mount_health_os(workspace_path) == MountHealth::DisconnectedEnotconn
            || is_linux_mountpoint(workspace_path)
        {
            let _ = lazy_unmount_fuse(workspace_path);
        }
        remove_dir_if_exists(workspace_path)
    }

    fn health_check(&self, workspace_path: &Path) -> Result<BackendHealth> {
        match probe_mount_health_os(workspace_path) {
            MountHealth::Healthy => Ok(BackendHealth {
                healthy: true,
                backend: BackendKind::LinuxFuse3,
                mount_active: true,
                message: None,
            }),
            MountHealth::DisconnectedEnotconn => Ok(BackendHealth {
                healthy: false,
                backend: BackendKind::LinuxFuse3,
                mount_active: false,
                message: Some("FUSE mountpoint returned ENOTCONN".to_string()),
            }),
            MountHealth::NotMounted | MountHealth::NotApplicable => Ok(BackendHealth {
                healthy: false,
                backend: BackendKind::LinuxFuse3,
                mount_active: false,
                message: Some("FUSE workspace path is not mounted".to_string()),
            }),
        }
    }
}

/// Windows `NTFS` Projected File System (`ProjFS`) CoW overlay backend powered by `oz-vfsd`.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone)]
pub struct ProjFsVfsBackend {
    pub auto_start: bool,
}

#[cfg(target_os = "windows")]
impl Default for ProjFsVfsBackend {
    fn default() -> Self {
        Self { auto_start: true }
    }
}

#[cfg(target_os = "windows")]
impl ProjFsVfsBackend {
    /// Creates a configured [`ProjFsVfsBackend`].
    #[must_use]
    pub const fn new(auto_start: bool) -> Self {
        Self { auto_start }
    }

    /// Stops `ProjFS` virtualization and cleans up a workspace when `git_common_dir` and
    /// `workspace` are known.
    pub(crate) fn destroy_with_context(
        &self,
        git_common_dir: &Path,
        workspace: &str,
        workspace_path: &Path,
    ) {
        let _ = self;
        destroy_daemon_vfs_workspace(
            git_common_dir,
            workspace,
            workspace_path,
            BackendKind::WindowsProjFs,
        );
    }
}

#[cfg(target_os = "windows")]
impl WorkspaceBackend for ProjFsVfsBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::WindowsProjFs
    }

    fn is_available(&self, repo_root: &Path) -> Result<bool> {
        let probe = detect_filesystem(repo_root)?;
        Ok(matches!(probe.fs_kind, oz_core::FilesystemKind::Ntfs)
            && probe.projfs_ready == Some(true))
    }

    fn spawn(&self, req: &SpawnRequest) -> Result<SpawnedWorkspace> {
        spawn_daemon_vfs_workspace(
            req,
            BackendKind::WindowsProjFs,
            false,
            false,
            self.auto_start,
        )
    }

    fn destroy(&self, workspace_path: &Path) -> Result<()> {
        if let Ok(repo) = OzoneRepo::discover(Some(workspace_path))
            && let Some(ws_name) = repo.workspace.as_deref()
        {
            self.destroy_with_context(&repo.git_common_dir, ws_name, workspace_path);
            return Ok(());
        }
        remove_dir_if_exists(workspace_path)
    }

    fn health_check(&self, workspace_path: &Path) -> Result<BackendHealth> {
        match probe_mount_health_os(workspace_path) {
            MountHealth::Healthy => {
                if let Ok(repo) = OzoneRepo::discover(Some(workspace_path))
                    && repo
                        .mount_record
                        .as_ref()
                        .is_some_and(|mr| mr.mounted && mr.backend == BackendKind::WindowsProjFs)
                    && ping_daemon(&repo.layout.vfsd_socket_path()).is_err()
                {
                    return Ok(BackendHealth {
                        healthy: false,
                        backend: BackendKind::WindowsProjFs,
                        mount_active: false,
                        message: Some(
                            "oz-vfsd daemon is not listening for ProjFS workspace".to_string(),
                        ),
                    });
                }
                Ok(BackendHealth {
                    healthy: true,
                    backend: BackendKind::WindowsProjFs,
                    mount_active: true,
                    message: None,
                })
            }
            MountHealth::DisconnectedEnotconn
            | MountHealth::NotMounted
            | MountHealth::NotApplicable => Ok(BackendHealth {
                healthy: false,
                backend: BackendKind::WindowsProjFs,
                mount_active: false,
                message: Some("ProjFS workspace path is not mounted".to_string()),
            }),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn spawn_daemon_vfs_workspace(
    req: &SpawnRequest,
    backend_kind: BackendKind,
    allow_other: bool,
    writeback_cache: bool,
    auto_start: bool,
) -> Result<SpawnedWorkspace> {
    let git_common_dir = resolve_git_common_dir(&req.git_dir)?;
    let layout = OzoneLayout::from_git_common_dir(&git_common_dir);
    layout.ensure_initialized()?;

    let ws_dir = layout.ws_meta_dir(&req.workspace_name);
    let upper_dir = req
        .upper_dir
        .clone()
        .unwrap_or_else(|| layout.ws_upper_dir(&req.workspace_name));
    let meta_dir = req
        .meta_dir
        .clone()
        .unwrap_or_else(|| layout.ws_tombstone_dir(&req.workspace_name));
    let whiteouts_dir = meta_dir.join("whiteouts");
    let opaque_dir = meta_dir.join("opaque");
    let seed_whiteouts_dir = meta_dir.join("seed_whiteouts");
    let seed_dir = ws_dir.join("seed");
    let mount_json_path = layout.ws_mount_record_path(&req.workspace_name);
    let sock_path = oz_vfsd::resolve_active_daemon_socket(&git_common_dir);

    for dir in [
        &ws_dir,
        &upper_dir,
        &meta_dir,
        &whiteouts_dir,
        &opaque_dir,
        &seed_whiteouts_dir,
        &seed_dir,
        &req.target_path,
    ] {
        std::fs::create_dir_all(dir)?;
    }

    prepare_vfs_seed_layer(req, &git_common_dir, &seed_dir, &seed_whiteouts_dir)?;

    let now = Utc::now();
    let mut mount_record = MountRecord {
        schema_version: MountRecord::CURRENT_SCHEMA_VERSION,
        workspace: req.workspace_name.clone(),
        branch: req.branch.clone(),
        backend: backend_kind,
        repo_root: req.repo_root.clone(),
        git_dir: git_common_dir.clone(),
        mount_point: req.target_path.clone(),
        upper_dir: Some(upper_dir.clone()),
        meta_dir: Some(meta_dir.clone()),
        base_tree: req.base_tree.clone(),
        base_commit: req.base_commit.clone(),
        socket_path: Some(sock_path.clone()),
        daemon_pid: None,
        mounted: false,
        created_at: now,
        updated_at: now,
    };
    write_json_atomic(&mount_json_path, &mount_record)?;

    let wrap_mount_err = |err: OzError| match err {
        OzError::MountUnavailable { .. } => err,
        other => OzError::MountUnavailable {
            workspace: req.workspace_name.clone(),
            path: req.target_path.clone(),
            errno: None,
            message: other.to_string(),
        },
    };

    let spec = MountSpec {
        workspace: req.workspace_name.clone(),
        branch: req.branch.clone(),
        repo_root: req.repo_root.clone(),
        git_dir: git_common_dir.clone(),
        mount_point: req.target_path.clone(),
        upper_dir: upper_dir.clone(),
        meta_dir: meta_dir.clone(),
        base_tree: req.base_tree.clone(),
        base_commit: req.base_commit.clone(),
        baseline_dir: req.baseline_dir.clone(),
        allow_other,
        writeback_cache,
    };

    let _mount_info = mount_via_daemon_with_auto_start(&git_common_dir, &spec, auto_start)
        .map_err(wrap_mount_err)?;

    // Prefer the MountRecord persisted by live oz-vfsd (includes daemon_pid), or update
    // mounted=true if a test mock listener handled the IPC without writing mount.json.
    if let Ok(raw) = std::fs::read_to_string(&mount_json_path)
        && let Ok(persisted) = serde_json::from_str::<MountRecord>(&raw)
        && persisted.mounted
    {
        mount_record = persisted;
    } else {
        mount_record.daemon_pid = ping_daemon(&sock_path).ok().map(|p| p.pid);
        mount_record.mounted = true;
        mount_record.updated_at = Utc::now();
        write_json_atomic(&mount_json_path, &mount_record)?;
    }

    Ok(SpawnedWorkspace {
        workspace_name: req.workspace_name.clone(),
        branch: req.branch.clone(),
        backend: backend_kind,
        workspace_path: req.target_path.clone(),
        upper_dir: Some(upper_dir),
        meta_dir: Some(meta_dir),
        mount_record: Some(mount_record),
        untouched_disk_bytes: 0,
    })
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn destroy_daemon_vfs_workspace(
    git_common_dir: &Path,
    workspace: &str,
    workspace_path: &Path,
    backend_kind: BackendKind,
) {
    let layout = OzoneLayout::from_git_common_dir(git_common_dir);

    let _ = unmount_via_daemon(git_common_dir, workspace, true);

    #[cfg(target_os = "linux")]
    if backend_kind == BackendKind::LinuxFuse3
        && (probe_mount_health_os(workspace_path) == MountHealth::DisconnectedEnotconn
            || is_linux_mountpoint(workspace_path))
    {
        let _ = lazy_unmount_fuse(workspace_path);
    }

    if backend_kind == BackendKind::WindowsProjFs {
        let _ = remove_dir_if_exists(workspace_path);
    } else if workspace_path.exists() {
        let _ = std::fs::remove_dir(workspace_path);
    }

    let mount_json = layout.ws_mount_record_path(workspace);
    if mount_json.is_file()
        && let Ok(raw) = std::fs::read_to_string(&mount_json)
        && let Ok(mut rec) = serde_json::from_str::<MountRecord>(&raw)
    {
        rec.mounted = false;
        rec.daemon_pid = None;
        rec.updated_at = Utc::now();
        if let Ok(json) = serde_json::to_string_pretty(&rec) {
            let _ = std::fs::write(&mount_json, json);
        }
    }
}

/// Universal fallback backend using a read-only (`0444`/`0555`) CAS hardlink store under
/// `.git/ozone/cas/` for immutable dependencies and `rayon` parallel copy for editable sources.
#[derive(Debug, Clone, Copy, Default)]
pub struct CasHardlinkBackend;

impl WorkspaceBackend for CasHardlinkBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::CasHardlink
    }

    fn is_available(&self, _repo_root: &Path) -> Result<bool> {
        Ok(true)
    }

    fn spawn(&self, req: &SpawnRequest) -> Result<SpawnedWorkspace> {
        let git_common_dir = resolve_git_common_dir(&req.git_dir)?;
        let cas_dir = git_common_dir.join("ozone/cas");
        materialize_cas_workspace(req, &cas_dir, &git_common_dir)?;
        Ok(spawned_non_vfs(BackendKind::CasHardlink, req))
    }

    fn destroy(&self, workspace_path: &Path) -> Result<()> {
        remove_dir_if_exists(workspace_path)
    }

    fn health_check(&self, workspace_path: &Path) -> Result<BackendHealth> {
        Ok(simple_dir_health(BackendKind::CasHardlink, workspace_path))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn spawn_reflink_workspace(kind: BackendKind, req: &SpawnRequest) -> Result<SpawnedWorkspace> {
    if let Some(ref base) = req.baseline_dir
        && base.is_dir()
    {
        clone_dir_with_reflink(base, &req.target_path)?;
        reconcile_reflink_delta(req, base)?;
        return Ok(spawned_non_vfs(kind, req));
    }

    std::fs::create_dir_all(&req.target_path)?;
    if let Some(ref tree_hex) = req.base_tree
        && let Ok(tree_id) = gix::ObjectId::from_hex(tree_hex.as_bytes())
        && let Ok(repo) = gix::open(&req.repo_root)
    {
        materialize_tree_from_odb(&repo, tree_id, &req.target_path)?;
    }

    Ok(spawned_non_vfs(kind, req))
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn reconcile_reflink_delta(req: &SpawnRequest, baseline_dir: &Path) -> Result<()> {
    let Some(ref target_tree_hex) = req.base_tree else {
        return Ok(());
    };
    let Ok(target_tree_id) = gix::ObjectId::from_hex(target_tree_hex.as_bytes()) else {
        return Ok(());
    };
    let Ok(repo) = gix::open(&req.repo_root) else {
        return Ok(());
    };

    if let Some(base_tree_id) = resolve_baseline_tree_id(&repo, baseline_dir) {
        let delta = compute_tree_delta(&repo, base_tree_id, target_tree_id)?;
        apply_tree_delta_to_dir(&repo, &delta, &req.target_path)?;
    } else {
        materialize_tree_from_odb(&repo, target_tree_id, &req.target_path)?;
    }
    Ok(())
}

fn spawned_non_vfs(kind: BackendKind, req: &SpawnRequest) -> SpawnedWorkspace {
    SpawnedWorkspace {
        workspace_name: req.workspace_name.clone(),
        branch: req.branch.clone(),
        backend: kind,
        workspace_path: req.target_path.clone(),
        upper_dir: None,
        meta_dir: None,
        mount_record: None,
        untouched_disk_bytes: 0,
    }
}

fn simple_dir_health(kind: BackendKind, workspace_path: &Path) -> BackendHealth {
    let exists = workspace_path.is_dir();
    BackendHealth {
        healthy: exists,
        backend: kind,
        mount_active: exists,
        message: (!exists).then(|| "workspace path does not exist".to_string()),
    }
}

pub(crate) fn remove_dir_if_exists(path: &Path) -> Result<()> {
    if !path.exists() && std::fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    clear_readonly_tree_on_windows(path);
    std::fs::remove_dir_all(path)?;
    Ok(())
}

#[cfg(windows)]
fn clear_readonly_tree_on_windows(path: &Path) {
    for entry in walkdir::WalkDir::new(path).into_iter().flatten() {
        if let Ok(meta) = entry.metadata() {
            let mut perms = meta.permissions();
            if perms.readonly() {
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(entry.path(), perms);
            }
        }
    }
}

#[cfg(not(windows))]
fn clear_readonly_tree_on_windows(_path: &Path) {}

/// Populates `.git/ozone/ws/<ws>/seed/` (worktree `.git` pointer + branch delta over
/// `baseline_dir`, or full ODB tree when no baseline exists) and writes `seed_whiteouts/*.oz_wh`
/// shared by both Linux FUSE3 and Windows `ProjFS` VFS backends.
pub fn prepare_vfs_seed_layer(
    req: &SpawnRequest,
    git_common_dir: &Path,
    seed_dir: &Path,
    seed_whiteouts_dir: &Path,
) -> Result<()> {
    let wt_git_dir = git_common_dir.join("worktrees").join(&req.workspace_name);
    if wt_git_dir.is_dir() {
        std::fs::write(
            seed_dir.join(".git"),
            format!("gitdir: {}\n", wt_git_dir.display()),
        )?;
    }

    let Some(ref target_tree_hex) = req.base_tree else {
        return Ok(());
    };
    let Ok(target_tree_id) = gix::ObjectId::from_hex(target_tree_hex.as_bytes()) else {
        return Ok(());
    };
    let Ok(repo) = gix::open(&req.repo_root) else {
        return Ok(());
    };

    if let Some(ref baseline_dir) = req.baseline_dir
        && baseline_dir.is_dir()
        && let Some(base_tree_id) = resolve_baseline_tree_id(&repo, baseline_dir)
    {
        let delta = compute_tree_delta(&repo, base_tree_id, target_tree_id)?;
        if !delta.upserts.is_empty() {
            let upsert_only = crate::materialize::TreeDelta {
                upserts: delta.upserts,
                deletions: Vec::new(),
            };
            apply_tree_delta_to_dir(&repo, &upsert_only, seed_dir)?;
        }
        for deleted_rel in &delta.deletions {
            let wh_path = whiteout_marker_path(seed_whiteouts_dir, deleted_rel);
            if let Some(parent) = wh_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(wh_path, b"")?;
        }
        return Ok(());
    }

    materialize_tree_from_odb(&repo, target_tree_id, seed_dir)?;
    Ok(())
}

fn whiteout_marker_path(root: &Path, rel_path: &str) -> PathBuf {
    let rel = Path::new(rel_path);
    let mut os = root.join(rel).into_os_string();
    os.push(".oz_wh");
    PathBuf::from(os)
}

#[cfg(target_os = "linux")]
fn is_linux_mountpoint(path: &Path) -> bool {
    let Ok(canon) = std::fs::canonicalize(path) else {
        return false;
    };
    let target = canon.to_string_lossy();
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return false;
    };
    mounts
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .any(|mp| mp == target)
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| OzError::Other(format!("path {} has no parent", path.display())))?;
    std::fs::create_dir_all(parent)?;
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".tmp_{}_{}_{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("file"),
        std::process::id(),
        seq
    ));
    let json = serde_json::to_string_pretty(value)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
