// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Domain types for the Ozone MVCC operation log, checkpoints, and undo/redo.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Default workspace key used in `ws_snapshots` and `refs/ozone/checkpoints/<ws>`
/// when operating directly in the main repository worktree (`repo.workspace == None`).
pub const DEFAULT_WORKSPACE_KEY: &str = "default";

/// Originator of an Ozone operation recorded in the OpLog.
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

/// Classification of an operation recorded in the MVCC OpLog.
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
}

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
