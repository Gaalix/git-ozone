// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tiered Copy-on-Write workspace backend trait and host filesystem capability detection.

pub mod backends;
pub mod detect;
pub mod lifecycle;
pub mod materialize;

pub use backends::{CasHardlinkBackend, backend_for_kind};
#[cfg(target_os = "macos")]
pub use backends::ClonefileBackend;
#[cfg(target_os = "linux")]
pub use backends::{FicloneBackend, Fuse3VfsBackend};
#[cfg(target_os = "windows")]
pub use backends::RefsCloneBackend;
pub use detect::{
    FilesystemProbe, LinuxFuseReadiness, detect_filesystem, detect_filesystem_with_ozone,
    probe_ozone_reflink, probe_reflink_in_dir, select_backend,
};
pub use lifecycle::{
    GcWorkspacesOptions, GcWorkspacesReport, RemoveWorkspaceOptions, RemovedWorkspace,
    SpawnWorkspaceOptions, SpawnedWorkspaceInfo, WorkspaceSummary, gc_workspaces, list_workspaces,
    remove_workspace, spawn_workspace,
};

use oz_core::{BackendKind, MountRecord, OzError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Parameters passed to a [`WorkspaceBackend`] when creating an isolated workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnRequest {
    pub workspace_name: String,
    pub branch: String,
    pub base_commit: Option<String>,
    pub base_tree: Option<String>,
    pub repo_root: PathBuf,
    pub git_dir: PathBuf,
    pub target_path: PathBuf,
    pub upper_dir: Option<PathBuf>,
    pub meta_dir: Option<PathBuf>,
    pub baseline_dir: Option<PathBuf>,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    pub materialize_files: bool,
}

/// Result returned after a [`WorkspaceBackend`] materializes or mounts a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnedWorkspace {
    pub workspace_name: String,
    pub branch: String,
    pub backend: BackendKind,
    pub workspace_path: PathBuf,
    pub upper_dir: Option<PathBuf>,
    pub meta_dir: Option<PathBuf>,
    pub mount_record: Option<MountRecord>,
    pub untouched_disk_bytes: u64,
}

/// Health snapshot for an existing CoW or VFS workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendHealth {
    pub healthy: bool,
    pub backend: BackendKind,
    pub mount_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Stable cross-platform trait implemented by APFS, FICLONE, FUSE3, ReFS, ProjFS, and CAS backends.
pub trait WorkspaceBackend: Send + Sync {
    /// Backend identifier.
    fn kind(&self) -> BackendKind;

    /// Human-readable backend name.
    fn name(&self) -> &'static str {
        self.kind().as_str()
    }

    /// Returns `true` if this backend can operate on `repo_root`.
    fn is_available(&self, repo_root: &Path) -> Result<bool>;

    /// Materializes or mounts a workspace at `req.target_path`.
    fn spawn(&self, req: &SpawnRequest) -> Result<SpawnedWorkspace>;

    /// Unmounts and cleans up the workspace at `workspace_path`.
    fn destroy(&self, workspace_path: &Path) -> Result<()>;

    /// Checks whether the workspace at `workspace_path` is healthy and accessible.
    fn health_check(&self, workspace_path: &Path) -> Result<BackendHealth>;
}

/// Milestone 0 placeholder backend for compile-time verification of the [`WorkspaceBackend`] trait.
#[derive(Debug, Clone, Copy)]
pub struct UnimplementedBackend {
    kind: BackendKind,
}

impl UnimplementedBackend {
    /// Creates a placeholder backend descriptor for `kind`.
    #[must_use]
    pub const fn new(kind: BackendKind) -> Self {
        Self { kind }
    }
}

impl WorkspaceBackend for UnimplementedBackend {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn is_available(&self, repo_root: &Path) -> Result<bool> {
        let probe = detect_filesystem(repo_root)?;
        Ok(match self.kind {
            BackendKind::ApfsClonefile => cfg!(target_os = "macos") && probe.reflink_supported,
            BackendKind::LinuxFiclone => cfg!(target_os = "linux") && probe.reflink_supported,
            BackendKind::LinuxFuse3 => probe.fuse.as_ref().is_some_and(|f| f.ready),
            BackendKind::WindowsRefsClone => cfg!(target_os = "windows") && probe.reflink_supported,
            BackendKind::WindowsProjFs => probe.projfs_ready == Some(true),
            BackendKind::CasHardlink => true,
        })
    }

    fn spawn(&self, req: &SpawnRequest) -> Result<SpawnedWorkspace> {
        Err(OzError::Other(format!(
            "backend '{}' spawn not implemented in Milestone 0 (target: {})",
            self.kind,
            req.target_path.display()
        )))
    }

    fn destroy(&self, workspace_path: &Path) -> Result<()> {
        Err(OzError::Other(format!(
            "backend '{}' destroy not implemented in Milestone 0 (target: {})",
            self.kind,
            workspace_path.display()
        )))
    }

    fn health_check(&self, workspace_path: &Path) -> Result<BackendHealth> {
        let exists = workspace_path.exists();
        Ok(BackendHealth {
            healthy: exists,
            backend: self.kind,
            mount_active: exists,
            message: (!exists).then(|| "workspace path does not exist".to_string()),
        })
    }
}
