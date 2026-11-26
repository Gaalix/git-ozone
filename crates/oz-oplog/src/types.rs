// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Domain types for the Ozone MVCC operation log, checkpoints, and undo/redo.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Default workspace key used in `ws_snapshots` and `refs/ozone/checkpoints/<ws>`
/// when operating directly in the main repository worktree (`repo.workspace == None`).
pub const DEFAULT_WORKSPACE_KEY: &str = "default";

/// Originator of an Ozone operation recorded in the `OpLog`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Human,
    Agent,
    System,
}

/// Actor metadata attached to an [`OpLogEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorInfo {
    pub kind: ActorKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// Classification of an operation recorded in the MVCC `OpLog`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    /// A state-changing command wrapped by `record_mutation` (e.g., `oz commit`, `oz ws spawn`).
    #[default]
    Mutate,
    /// A working-tree dirty-state checkpoint (`oz checkpoint`).
    Checkpoint,
    /// An `oz undo` operation inverting one or more prior operations or un-checkpointed edits.
    Undo,
    /// An `oz redo` operation reversing a prior `Undo` operation.
    Redo,
}

/// Reference transition recorded for lock-free CAS undo/redo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefDelta {
    pub old: Option<String>,
    pub new: Option<String>,
}

/// Recorded state of a single dirty file inside a [`WsSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DirtyFileState {
    /// Regular file, executable, or symlink present in the working tree (blob stored in `.git/objects/`).
    Present {
        blob_oid: String,
        /// Git tree entry mode (`0o100644`, `0o100755`, or `0o120000`).
        mode: u32,
    },
    /// Tracked file deleted from the working tree (or hidden by a FUSE whiteout).
    Deleted,
}

/// Snapshot manifest entry for a single dirty working-tree path (`O(dirty_files)` restore).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyFileSnapshot {
    pub path: String,
    pub state: DirtyFileState,
}

/// Per-workspace tree and dirty-file snapshot linked to an [`OpLogEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsSnapshot {
    /// Post-operation base tree OID (`HEAD` tree, or empty tree hex if unborn).
    pub base_tree: String,
    /// Post-operation Git tree OID built via `gix::objs::tree::Editor` over `base_tree`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty_tree: Option<String>,
    /// Post-operation dirty file manifest (`O(dirty_files)`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirty_files: Vec<DirtyFileSnapshot>,
    /// Pre-operation base tree OID (present for `Mutate`, `Undo`, `Redo`, and chained checkpoints).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_base_tree: Option<String>,
    /// Pre-operation dirty tree OID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_dirty_tree: Option<String>,
    /// Pre-operation dirty file manifest (`O(dirty_files)`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_dirty_files: Vec<DirtyFileSnapshot>,
    /// Post-operation `HEAD` commit hex OID (`None` when unborn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<String>,
    /// Pre-operation `HEAD` commit hex OID (`None` when unborn or unrecorded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_head_commit: Option<String>,
    /// Checked-out branch name for this workspace, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Pre-operation checked-out branch name (`None` if detached or legacy entry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_branch: Option<String>,
    /// True when pre-operation `HEAD` was detached (distinguishes detached from legacy omitted `pre_branch`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub pre_detached: bool,
    /// True when post-operation `HEAD` was detached.
    #[serde(default, skip_serializing_if = "is_false")]
    pub post_detached: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

/// Content-addressed operation log record stored under `.git/ozone/oplog/ops/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpLogEntry {
    pub op_id: String,
    #[serde(default)]
    pub parents: Vec<String>,
    pub timestamp: DateTime<Utc>,
    pub actor: ActorInfo,
    #[serde(default)]
    pub kind: OpKind,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse_of: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undone_ops: Vec<String>,
    /// True when an [`OpKind::Undo`] entry rewound un-checkpointed live working-tree edits
    /// back to a checkpoint rather than marking the checkpoint entry itself as undone.
    #[serde(default, skip_serializing_if = "is_false")]
    pub rewound_uncheckpointed: bool,
    #[serde(default)]
    pub ref_deltas: BTreeMap<String, RefDelta>,
    #[serde(default)]
    pub ws_snapshots: BTreeMap<String, WsSnapshot>,
}

/// Target specification for [`crate::undo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoTarget {
    /// Undo the most recent `n` active operations (default `1`).
    Steps(usize),
    /// Undo a specific operation by `op_id`.
    OpId(String),
}

impl Default for UndoTarget {
    fn default() -> Self {
        Self::Steps(1)
    }
}

impl From<usize> for UndoTarget {
    fn from(steps: usize) -> Self {
        Self::Steps(steps)
    }
}

impl From<&str> for UndoTarget {
    fn from(op_id: &str) -> Self {
        Self::OpId(op_id.to_string())
    }
}

impl From<String> for UndoTarget {
    fn from(op_id: String) -> Self {
        Self::OpId(op_id)
    }
}

/// Result returned by [`crate::checkpoint`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointOutcome {
    pub op_id: String,
    pub workspace: String,
    pub base_tree: String,
    pub dirty_tree: String,
    pub dirty_file_count: usize,
    pub checkpoint_ref: String,
    pub op_checkpoint_ref: String,
    pub entry: OpLogEntry,
}

/// Result returned by [`crate::undo`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoOutcome {
    pub op_id: String,
    pub workspace: String,
    pub undone_op_ids: Vec<String>,
    pub restored_base_tree: String,
    pub restored_dirty_tree: Option<String>,
    pub restored_files: usize,
    pub ref_deltas: BTreeMap<String, RefDelta>,
    pub entry: OpLogEntry,
}

/// Result returned by [`crate::redo`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedoOutcome {
    pub op_id: String,
    pub workspace: String,
    pub redone_undo_op_id: String,
    pub restored_base_tree: String,
    pub restored_dirty_tree: Option<String>,
    pub restored_files: usize,
    pub ref_deltas: BTreeMap<String, RefDelta>,
    pub entry: OpLogEntry,
}

/// Default number of `OpLog` entries returned by [`crate::inspect_oplog`] (`oz oplog` / `oz undo --list`).
pub const DEFAULT_OPLOG_LIST_LIMIT: usize = 20;

/// Effective lifecycle status of an [`OpLogEntry`] within a workspace's undo/redo timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpLogEntryStatus {
    /// Forward mutation or checkpoint currently active (eligible for `oz undo`).
    Active,
    /// Forward mutation or checkpoint currently reverted by an active undo.
    Undone,
    /// An `undo` entry currently on the redo stack (eligible for `oz redo`).
    Redoable,
    /// An `undo` entry that was already reversed by a subsequent `oz redo` or explicit undo.
    Redone,
    /// An `undo` entry whose redo slot was cleared by a newer forward mutation or checkpoint.
    Superseded,
    /// A `redo` entry (or undo-of-undo) that re-applied a prior undone operation.
    Applied,
}

impl OpLogEntryStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Undone => "undone",
            Self::Redoable => "redoable",
            Self::Redone => "redone",
            Self::Superseded => "superseded",
            Self::Applied => "applied",
        }
    }
}

/// Options controlling read-only `OpLog` inspection ([`crate::inspect_oplog`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpLogListOptions {
    /// Maximum number of entries to return, ordered newest first (default `20`).
    pub limit: usize,
    /// Include operations from all workspaces instead of only the active workspace.
    pub all_workspaces: bool,
}

impl Default for OpLogListOptions {
    fn default() -> Self {
        Self {
            limit: DEFAULT_OPLOG_LIST_LIMIT,
            all_workspaces: false,
        }
    }
}

/// Single summarized operation returned by [`crate::inspect_oplog`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpLogListItem {
    pub op_id: String,
    pub timestamp: DateTime<Utc>,
    pub kind: OpKind,
    pub command: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub actor_kind: ActorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub status: OpLogEntryStatus,
    pub is_head: bool,
    pub is_next_undo: bool,
    pub is_next_redo: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse_of: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undone_ops: Vec<String>,
    pub ref_count: usize,
}

/// Read-only inspection report returned by [`crate::inspect_oplog`] (`oz oplog` / `oz undo --list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpLogListReport {
    pub workspace: String,
    pub all_workspaces: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_op_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_cursor_op_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redo_cursor_op_id: Option<String>,
    pub active_count: usize,
    pub redo_depth: usize,
    pub total_entries: usize,
    pub limit: usize,
    pub truncated: bool,
    pub entries: Vec<OpLogListItem>,
}

/// Default retention window (in days) for `OpLog` compaction (`oz gc --oplog-days`).
pub const DEFAULT_OPLOG_RETENTION_DAYS: u64 = 30;

/// Options controlling `OpLog` compaction and garbage collection (`oz gc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpLogGcOptions {
    /// Retain `OpLog` entries newer than this many days (default `30`).
    pub oplog_days: u64,
    /// Optional cap on retained non-protected `OpLog` entries (oldest evicted first).
    pub max_entries: Option<usize>,
    /// Preview compaction and GC without mutating `.git/ozone/oplog/` or refs.
    pub dry_run: bool,
    /// Optional reference timestamp for age computation (defaults to `Utc::now()`).
    pub now: Option<DateTime<Utc>>,
}

impl Default for OpLogGcOptions {
    fn default() -> Self {
        Self {
            oplog_days: DEFAULT_OPLOG_RETENTION_DAYS,
            max_entries: None,
            dry_run: false,
            now: None,
        }
    }
}

/// Summary returned by [`crate::compact_oplog`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpLogGcOutcome {
    pub dry_run: bool,
    pub oplog_days: u64,
    pub cutoff_timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_op_id: Option<String>,
    pub total_entries_before: usize,
    pub removed_entries: Vec<String>,
    pub retained_entries: usize,
    pub protected_entries: usize,
    pub relinked_entries: usize,
    pub pruned_checkpoint_refs: Vec<String>,
    pub removed_temp_files: Vec<String>,
    pub preserved_object_count: usize,
    pub reclaimed_bytes: u64,
    pub remaining_bytes: u64,
}

/// Options controlling [`crate::fsck_repo`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsckOptions {
    /// Safely recover/clean interrupted transactions, broken `OpLog` pointers, and orphaned
    /// metadata records without deleting user working-tree files.
    pub repair: bool,
}

/// Severity of an integrity finding reported by `oz fsck`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsckSeverity {
    Warning,
    Error,
}

/// Subsystem checked by `oz fsck`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsckCategory {
    Git,
    Oplog,
    Snapshot,
    Mount,
    Baseline,
    Transaction,
}

/// Status of a summary check section in `oz fsck`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsckCheckStatus {
    Ok,
    Repaired,
    Warn,
    Fail,
}

/// Individual integrity finding emitted by `oz fsck`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsckIssue {
    pub code: String,
    pub category: FsckCategory,
    pub severity: FsckSeverity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op_id: Option<String>,
    pub repairable: bool,
    pub repaired: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// High-level summary check emitted by `oz fsck`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsckCheckSummary {
    pub id: String,
    pub category: FsckCategory,
    pub status: FsckCheckStatus,
    pub summary: String,
}

/// Complete integrity verification and repair report returned by [`crate::fsck_repo`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsckReport {
    pub healthy: bool,
    pub repair_mode: bool,
    pub git_fsck_passed: bool,
    pub oplog_entries_checked: usize,
    pub snapshot_objects_checked: usize,
    pub mounts_checked: usize,
    pub baselines_checked: usize,
    pub issue_count: usize,
    pub unresolved_issue_count: usize,
    pub repaired_count: usize,
    pub checks: Vec<FsckCheckSummary>,
    pub issues: Vec<FsckIssue>,
    pub repaired_actions: Vec<String>,
}
