// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared domain types used across Ozone workspace crates.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

/// Copy-on-Write or VFS workspace materialization backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// macOS APFS native directory/file reflink via `clonefile(2)`.
    ApfsClonefile,
    /// Linux Btrfs / XFS / bcachefs native reflink via `ioctl(FICLONE)`.
    LinuxFiclone,
    /// Linux `ext4` unprivileged FUSE3 overlay daemon (`oz-vfsd`).
    LinuxFuse3,
    /// Windows Dev Drive / ReFS block clone via `FSCTL_DUPLICATE_EXTENTS_TO_FILE`.
    WindowsRefsClone,
    /// Windows NTFS virtualization via `oz-vfsd` (`windows-projfs`).
    WindowsProjFs,
    /// Universal fallback: read-only CAS hardlinks for immutable deps + copied sources.
    CasHardlink,
}

impl BackendKind {
    /// Stable lowercase identifier for CLI and JSON output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApfsClonefile => "apfs_clonefile",
            Self::LinuxFiclone => "linux_ficlone",
            Self::LinuxFuse3 => "linux_fuse3",
            Self::WindowsRefsClone => "windows_refs_clone",
            Self::WindowsProjFs => "windows_proj_fs",
            Self::CasHardlink => "cas_hardlink",
        }
    }

    /// Returns `true` if this backend relies on the background `oz-vfsd` daemon.
    #[must_use]
    pub const fn requires_daemon(self) -> bool {
        matches!(self, Self::LinuxFuse3 | Self::WindowsProjFs)
    }

    /// Returns `true` if this backend uses native kernel filesystem reflinks.
    #[must_use]
    pub const fn is_native_reflink(self) -> bool {
        matches!(
            self,
            Self::ApfsClonefile | Self::LinuxFiclone | Self::WindowsRefsClone
        )
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for BackendKind {
    type Err = String;

    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        let normalized = raw.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "apfs_clonefile" | "apfs" => Ok(Self::ApfsClonefile),
            "linux_ficlone" | "ficlone" | "reflink" => Ok(Self::LinuxFiclone),
            "linux_fuse3" | "fuse3" | "fuse" => Ok(Self::LinuxFuse3),
            "windows_refs_clone" | "refs" => Ok(Self::WindowsRefsClone),
            "windows_proj_fs" | "projfs" => Ok(Self::WindowsProjFs),
            "cas_hardlink" | "hardlink" => Ok(Self::CasHardlink),
            other => Err(format!(
                "unknown CoW backend '{other}'; valid values: auto, cas_hardlink, linux_fuse3, linux_ficlone, apfs_clonefile, windows_refs_clone, windows_proj_fs"
            )),
        }
    }
}

/// Detected host filesystem kind for a repository or workspace path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemKind {
    Ext4,
    Btrfs,
    Xfs,
    Bcachefs,
    Apfs,
    HfsPlus,
    Ntfs,
    Refs,
    Tmpfs,
    Overlayfs,
    Fuse,
    Nfs,
    Zfs,
    Unknown(String),
}

impl FilesystemKind {
    /// Stable string identifier for the detected filesystem.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Ext4 => "ext4",
            Self::Btrfs => "btrfs",
            Self::Xfs => "xfs",
            Self::Bcachefs => "bcachefs",
            Self::Apfs => "apfs",
            Self::HfsPlus => "hfs+",
            Self::Ntfs => "ntfs",
            Self::Refs => "refs",
            Self::Tmpfs => "tmpfs",
            Self::Overlayfs => "overlayfs",
            Self::Fuse => "fuse",
            Self::Nfs => "nfs",
            Self::Zfs => "zfs",
            Self::Unknown(name) => name.as_str(),
        }
    }

    /// Returns `true` if the filesystem natively supports zero-copy CoW reflinks.
    #[must_use]
    pub fn supports_reflinks(&self) -> bool {
        matches!(
            self,
            Self::Apfs | Self::Btrfs | Self::Xfs | Self::Bcachefs | Self::Refs
        )
    }
}

impl fmt::Display for FilesystemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Kind of change observed for a workspace file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

impl ChangeKind {
    /// Single-character status indicator used in compact CLI output.
    #[must_use]
    pub const fn short_code(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Copied => "C",
            Self::TypeChanged => "T",
            Self::Untracked => "?",
            Self::Conflicted => "U",
        }
    }
}

/// Line operation inside a unified diff hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    Context,
    Add,
    Delete,
}

/// Single line within a [`DiffHunk`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_line: Option<u32>,
    pub content: String,
}

/// Addressable diff hunk with deterministic session-local ID (`h1a`, `h1b`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub id: String,
    pub file_id: String,
    pub path: String,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub header: String,
    #[serde(default)]
    pub lines: Vec<DiffLine>,
}

/// Addressable working-tree file change with deterministic ID (`f1`, `f2`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub id: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub kind: ChangeKind,
    pub additions: u32,
    pub deletions: u32,
    #[serde(default)]
    pub is_binary: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub out_of_scope: bool,
    #[serde(default)]
    pub hunks: Vec<DiffHunk>,
}

/// Dirty file entry persisted in `.git/ozone/state/<ws>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyEntry {
    pub kind: ChangeKind,
    pub recorded_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oid_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_nanos: Option<i128>,
}

/// Indexless per-workspace state stored in `.git/ozone/state/<ws>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub schema_version: u32,
    pub name: String,
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    pub base_tree: String,
    pub backend: BackendKind,
    pub worktree_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_worktree_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_id: Option<String>,
    #[serde(default)]
    pub dirty_paths: BTreeMap<String, DirtyEntry>,
    #[serde(default)]
    pub pinned_paths: BTreeSet<String>,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_checkpoint_oid: Option<String>,
}

impl WorkspaceState {
    /// Current schema version for `.git/ozone/state/<ws>.json`.
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;
}

/// Persistent mount metadata stored on real host disk at `.git/ozone/ws/<ws>/mount.json`.
///
/// Crucially, this file lives inside `<repo>/.git/ozone/` rather than inside the FUSE
/// mountpoint itself, allowing Ozone commands to locate the repository and remount
/// cleanly even if a dead FUSE mount at `../<repo>.oz/<ws>` returns `ENOTCONN`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountRecord {
    pub schema_version: u32,
    pub workspace: String,
    pub branch: String,
    pub backend: BackendKind,
    pub repo_root: PathBuf,
    pub git_dir: PathBuf,
    pub mount_point: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upper_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_tree: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_pid: Option<u32>,
    pub mounted: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl MountRecord {
    /// Current schema version for `.git/ozone/ws/<ws>/mount.json`.
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;
}
