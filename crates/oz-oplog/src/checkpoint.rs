// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sub-10 ms dirty working-tree checkpointing into `.git/objects/` + `refs/ozone/checkpoints/<ws>`.

use crate::dirty::{
    SnapshotSlice, build_dirty_tree, ensure_mount_healthy, make_ws_snapshot, open_gix_for_workspace,
    parse_oid, read_ref_oid_hex, resolve_workspace_head, scan_dirty_files,
    sync_workspace_state_file, workspace_key,
};
use crate::new_op_id;
use crate::store::{append_entry, ensure_oplog_dirs, walk_entries_rev};
use crate::types::{
    ActorInfo, CheckpointOutcome, OpKind, OpLogEntry, RefDelta, WsSnapshot,
};
use chrono::Utc;
use gix::refs::FullName;
use gix::refs::transaction::{PreviousValue, RefEdit};
use oz_core::{FileChange, OzError, OzoneLayout, OzoneRepo, Result};
use std::collections::BTreeMap;

/// Returns the latest recorded [`WsSnapshot`] for `ws_key` by walking backwards from `oplog/HEAD`.
pub(crate) fn find_latest_ws_snapshot(
    layout: &OzoneLayout,
    ws_key: &str,
) -> Result<Option<WsSnapshot>> {
    let mut found = None;
    walk_entries_rev(layout, |entry| {
        if let Some(snap) = entry.ws_snapshots.get(ws_key) {
            found = Some(snap.clone());
            return Ok(false);
        }
        Ok(true)
    })?;
    Ok(found)
}

/// Captures an `O(dirty_files)` snapshot of uncommitted working-tree edits into `.git/objects/`,
/// builds a real Git `dirty_tree` via `gix::objs::tree::Editor` over `base_tree`, updates
/// `refs/ozone/checkpoints/<ws>` (and `refs/ozone/op-checkpoints/<ws>/<op_id>`), and appends
/// an [`OpKind::Checkpoint`] entry to the MVCC OpLog.
pub fn checkpoint(
    repo: &OzoneRepo,
    command: &str,
    message: Option<&str>,
    actor: ActorInfo,
    dirty_override: Option<&[FileChange]>,
) -> Result<CheckpointOutcome> {
    ensure_mount_healthy(repo)?;
    ensure_oplog_dirs(&repo.layout)?;

    let gix_repo = open_gix_for_workspace(repo)?;
    let (branch, head_commit, base_tree) = resolve_workspace_head(repo, &gix_repo)?;
    let dirty_files = scan_dirty_files(repo, &gix_repo, base_tree, dirty_override)?;
    let dirty_tree = build_dirty_tree(&gix_repo, base_tree, &dirty_files)?;

    let op_id = new_op_id();
    let ws_key = workspace_key(repo).to_string();

    let checkpoint_ref = format!("refs/ozone/checkpoints/{ws_key}");
    // Stored under a disjoint prefix (`op-checkpoints/`) so `refs/ozone/checkpoints/<ws>`
    // never hits a Git loose-ref Directory/File (D/F) conflict.
    let op_checkpoint_ref = format!("refs/ozone/op-checkpoints/{ws_key}/{op_id}");

    let prev_checkpoint_oid = read_ref_oid_hex(&gix_repo, &checkpoint_ref)?;

    let cp_full = FullName::try_from(checkpoint_ref.as_str())
        .map_err(|err| OzError::Git(format!("invalid checkpoint ref '{checkpoint_ref}': {err}")))?;
    let op_cp_full = FullName::try_from(op_checkpoint_ref.as_str()).map_err(|err| {
        OzError::Git(format!(
            "invalid op checkpoint ref '{op_checkpoint_ref}': {err}"
        ))
    })?;

    let reflog_msg = message
        .map(|m| format!("oz checkpoint: {m}"))
        .unwrap_or_else(|| format!("oz checkpoint {op_id}"));

    let edits = [
        RefEdit::update(cp_full, dirty_tree, PreviousValue::Any, reflog_msg.as_str()),
        RefEdit::update(
            op_cp_full,
            dirty_tree,
            PreviousValue::MustNotExist,
            reflog_msg.as_str(),
        ),
    ];
    gix_repo
        .edit_references(edits)
        .map_err(|err| OzError::Git(err.to_string()))?;

    let prior_snap = find_latest_ws_snapshot(&repo.layout, &ws_key)?;
    let pre_slice = match prior_snap {
        Some(prev) => Some(SnapshotSlice {
            base_tree: parse_oid(&prev.base_tree)?,
            dirty_tree: prev.dirty_tree.as_deref().map(parse_oid).transpose()?,
            dirty_files: prev.dirty_files,
            head_commit: prev.head_commit.as_deref().map(parse_oid).transpose()?,
            branch: prev.branch,
        }),
        None => None,
    };

    let base_tree_hex = base_tree.to_hex().to_string();
    let dirty_tree_hex = dirty_tree.to_hex().to_string();
    let dirty_file_count = dirty_files.len();

    let post_slice = SnapshotSlice {
        base_tree,
        dirty_tree: Some(dirty_tree),
        dirty_files: dirty_files.clone(),
        head_commit,
        branch: branch.clone(),
    };
    let ws_snap = make_ws_snapshot(post_slice, pre_slice);

    let mut ref_deltas = BTreeMap::new();
    ref_deltas.insert(
        checkpoint_ref.clone(),
        RefDelta {
            old: prev_checkpoint_oid,
            new: Some(dirty_tree_hex.clone()),
        },
    );

    let mut ws_snapshots = BTreeMap::new();
    ws_snapshots.insert(ws_key.clone(), ws_snap);

    let entry = OpLogEntry {
        op_id: op_id.clone(),
        parents: Vec::new(),
        timestamp: Utc::now(),
        actor,
        kind: OpKind::Checkpoint,
        command: command.to_string(),
        message: message.map(str::to_string),
        inverse_of: None,
        undone_ops: Vec::new(),
        rewound_uncheckpointed: false,
        ref_deltas,
        ws_snapshots,
    };

    let stored_entry = append_entry(&repo.layout, entry)?;
    sync_workspace_state_file(
        repo,
        &gix_repo,
        head_commit,
        base_tree,
        &dirty_files,
        Some(&dirty_tree_hex),
        branch.as_deref(),
    )?;

    Ok(CheckpointOutcome {
        op_id,
        workspace: ws_key,
        base_tree: base_tree_hex,
        dirty_tree: dirty_tree_hex,
        dirty_file_count,
        checkpoint_ref,
        op_checkpoint_ref,
        entry: stored_entry,
    })
}
