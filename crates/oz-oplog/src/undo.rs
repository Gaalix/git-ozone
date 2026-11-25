// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sub-10 ms `oz undo` and `oz redo` via atomic `gix` CAS ref updates and `O(dirty_files)`
//! working-tree restoration.

use crate::dirty::{
    SnapshotSlice, ensure_mount_healthy, make_ws_snapshot, open_gix_for_workspace, parse_oid,
    resolve_workspace_head, restore_working_tree, scan_dirty_files, snapshots_match,
    sync_git_index_diff, sync_workspace_state_file, workspace_key,
};
use crate::new_op_id;
use crate::store::{append_entry, ensure_oplog_dirs, list_entries, load_entry};
use crate::types::{
    ActorInfo, DEFAULT_WORKSPACE_KEY, OpKind, OpLogEntry, RedoOutcome, RefDelta, UndoOutcome,
    UndoTarget, WsSnapshot,
};
use chrono::Utc;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::{FullName, Target};
use oz_core::{FileChange, OzError, OzoneRepo, Result};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Returns `true` if `entry` belongs to or affects `ws_key`.
fn entry_affects_workspace(entry: &OpLogEntry, ws_key: &str) -> bool {
    if entry.ws_snapshots.contains_key(ws_key) {
        return true;
    }
    match entry.actor.workspace.as_deref() {
        Some(ws) => ws == ws_key,
        None => ws_key == DEFAULT_WORKSPACE_KEY,
    }
}

struct WorkspaceTimeline<'a> {
    active_forward: Vec<&'a OpLogEntry>,
    latest_unredone_undo: Option<&'a OpLogEntry>,
    undone_set: HashSet<String>,
}

fn analyze_workspace_timeline<'a>(
    entries: &'a [OpLogEntry],
    ws_key: &str,
) -> WorkspaceTimeline<'a> {
    let mut entry_by_id: HashMap<&str, &OpLogEntry> = HashMap::with_capacity(entries.len());
    for e in entries {
        entry_by_id.insert(e.op_id.as_str(), e);
    }

    let mut undone_set: HashSet<String> = HashSet::new();
    let mut undo_stack: Vec<&'a OpLogEntry> = Vec::new();
    let mut forward_entries: Vec<&'a OpLogEntry> = Vec::new();

    for entry in entries {
        if !entry_affects_workspace(entry, ws_key) {
            continue;
        }
        match entry.kind {
            OpKind::Mutate | OpKind::Checkpoint => {
                // Any new forward mutation or checkpoint clears the automatic redo stack.
                undo_stack.clear();
                forward_entries.push(entry);
            }
            OpKind::Undo => {
                // If an explicit `undo(op_id)` targeted a prior `Undo`, treat it like a Redo.
                if let Some(ref inv) = entry.inverse_of
                    && let Some(target_entry) = entry_by_id.get(inv.as_str())
                    && target_entry.kind == OpKind::Undo
                {
                    undo_stack.retain(|u| u.op_id != *inv);
                    if !target_entry.rewound_uncheckpointed {
                        for id in undone_targets(target_entry) {
                            undone_set.remove(&id);
                        }
                    }
                    continue;
                }

                if !entry.rewound_uncheckpointed {
                    for id in undone_targets(entry) {
                        undone_set.insert(id);
                    }
                }
                undo_stack.push(entry);
            }
            OpKind::Redo => {
                if let Some(ref undo_id) = entry.inverse_of {
                    undo_stack.retain(|u| u.op_id != *undo_id);
                    if let Some(target_undo) = entry_by_id.get(undo_id.as_str())
                        && !target_undo.rewound_uncheckpointed
                    {
                        for id in undone_targets(target_undo) {
                            undone_set.remove(&id);
                        }
                    }
                }
            }
        }
    }

    let active_forward = forward_entries
        .into_iter()
        .filter(|e| !undone_set.contains(&e.op_id))
        .collect();

    WorkspaceTimeline {
        active_forward,
        latest_unredone_undo: undo_stack.last().copied(),
        undone_set,
    }
}

fn undone_targets(entry: &OpLogEntry) -> Vec<String> {
    if !entry.undone_ops.is_empty() {
        entry.undone_ops.clone()
    } else {
        entry.inverse_of.iter().cloned().collect()
    }
}

/// Applies atomic `gix` CAS ref updates transitioning each ref from `old` (expected current)
/// to `new` (desired target), and updates `HEAD` symbolically or detached when `src` and `dst`
/// branch/HEAD mode differ (for main repo or linked git worktrees with their own `HEAD`).
fn apply_cas_ref_deltas_and_head(
    update_git_head: bool,
    gix_repo: &gix::Repository,
    transitions: &BTreeMap<String, RefDelta>,
    src: &SnapshotSlice,
    dst: &SnapshotSlice,
    reflog_msg: &str,
) -> Result<()> {
    let mut edits = Vec::with_capacity(transitions.len() + 1);
    let head_changed = src.branch != dst.branch
        || (src.branch.is_none() && dst.branch.is_none() && src.head_commit != dst.head_commit);

    let head_edit = if update_git_head && head_changed && !transitions.contains_key("HEAD") {
        let new_target = if let Some(ref br) = dst.branch {
            let full = FullName::try_from(format!("refs/heads/{br}").as_str()).map_err(|err| {
                OzError::Git(format!("invalid branch ref 'refs/heads/{br}': {err}"))
            })?;
            Some(Target::Symbolic(full))
        } else {
            dst.head_commit.map(Target::Object)
        };
        new_target.map(|target| RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: reflog_msg.into(),
                },
                expected: PreviousValue::Any,
                new: target,
            },
            name: FullName::try_from("HEAD").expect("HEAD is valid ref name"),
            deref: false,
        })
    } else {
        None
    };

    // Place HEAD update before any ref deletions so deleting a previously checked-out branch succeeds.
    if let Some(edit) = head_edit {
        edits.push(edit);
    }

    for (ref_name, delta) in transitions {
        if delta.old == delta.new {
            continue;
        }
        let full_name = FullName::try_from(ref_name.as_str())
            .map_err(|err| OzError::Git(format!("invalid ref name '{ref_name}': {err}")))?;

        let edit = match (&delta.old, &delta.new) {
            (Some(curr_hex), Some(target_hex)) => {
                let curr_oid = parse_oid(curr_hex)?;
                let target_oid = parse_oid(target_hex)?;
                RefEdit::update(
                    full_name,
                    target_oid,
                    PreviousValue::MustExistAndMatch(Target::Object(curr_oid)),
                    reflog_msg,
                )
            }
            (Some(curr_hex), None) => {
                let curr_oid = parse_oid(curr_hex)?;
                RefEdit::delete(
                    full_name,
                    PreviousValue::MustExistAndMatch(Target::Object(curr_oid)),
                )
            }
            (None, Some(target_hex)) => {
                let target_oid = parse_oid(target_hex)?;
                RefEdit::update(
                    full_name,
                    target_oid,
                    PreviousValue::MustNotExist,
                    reflog_msg,
                )
            }
            (None, None) => continue,
        };
        edits.push(edit);
    }

    if edits.is_empty() {
        return Ok(());
    }

    gix_repo.edit_references(edits).map_err(|err| OzError::Conflict {
        message: format!("atomic CAS ref update failed during {reflog_msg}: {err}"),
        conflicts: Vec::new(),
        recovery: vec![
            "Another operation modified a target Git ref concurrently; inspect `git show-ref` and retry."
                .to_string(),
        ],
    })?;

    Ok(())
}

fn invert_deltas(deltas: &BTreeMap<String, RefDelta>) -> BTreeMap<String, RefDelta> {
    deltas
        .iter()
        .map(|(k, d)| {
            (
                k.clone(),
                RefDelta {
                    old: d.new.clone(),
                    new: d.old.clone(),
                },
            )
        })
        .collect()
}

/// Folds inverse `ref_deltas` across `ops_rev` (ordered newest-to-oldest) into a single
/// `(expected_current -> target)` CAS delta map.
fn fold_inverse_ref_deltas(ops_rev: &[&OpLogEntry]) -> Result<BTreeMap<String, RefDelta>> {
    let mut folded: BTreeMap<String, RefDelta> = BTreeMap::new();

    for op in ops_rev {
        for (ref_name, delta) in &op.ref_deltas {
            if delta.old == delta.new {
                continue;
            }
            match folded.get_mut(ref_name) {
                Some(existing) => {
                    if existing.new != delta.new {
                        return Err(OzError::Conflict {
                            message: format!(
                                "non-contiguous ref history for '{ref_name}' while folding op '{}'",
                                op.op_id
                            ),
                            conflicts: Vec::new(),
                            recovery: vec!["Undo newer conflicting operations first.".to_string()],
                        });
                    }
                    existing.new.clone_from(&delta.old);
                }
                None => {
                    folded.insert(
                        ref_name.clone(),
                        RefDelta {
                            old: delta.new.clone(),
                            new: delta.old.clone(),
                        },
                    );
                }
            }
        }
    }

    folded.retain(|_, d| d.old != d.new);
    Ok(folded)
}

fn post_slice(snap: &WsSnapshot) -> Result<SnapshotSlice> {
    Ok(SnapshotSlice {
        base_tree: parse_oid(&snap.base_tree)?,
        dirty_tree: snap.dirty_tree.as_deref().map(parse_oid).transpose()?,
        dirty_files: snap.dirty_files.clone(),
        head_commit: snap.head_commit.as_deref().map(parse_oid).transpose()?,
        branch: if snap.post_detached {
            None
        } else {
            snap.branch.clone()
        },
    })
}

fn pre_slice(snap: &WsSnapshot, op_id: &str) -> Result<SnapshotSlice> {
    let pre_base = snap
        .pre_base_tree
        .as_deref()
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!(
                "operation '{op_id}' has no recorded pre-operation workspace snapshot"
            ),
            recovery: vec!["No prior state exists before the earliest checkpoint.".to_string()],
        })?;
    let branch = if snap.pre_detached {
        None
    } else {
        snap.pre_branch.clone().or_else(|| snap.branch.clone())
    };
    Ok(SnapshotSlice {
        base_tree: parse_oid(pre_base)?,
        dirty_tree: snap.pre_dirty_tree.as_deref().map(parse_oid).transpose()?,
        dirty_files: snap.pre_dirty_files.clone(),
        head_commit: snap.pre_head_commit.as_deref().map(parse_oid).transpose()?,
        branch,
    })
}

/// Undoes one or more operations (or un-checkpointed dirty edits after a checkpoint) and appends
/// an [`OpKind::Undo`] entry to the OpLog.
pub fn undo(
    repo: &OzoneRepo,
    target: impl Into<UndoTarget>,
    actor: ActorInfo,
) -> Result<UndoOutcome> {
    undo_with_dirty(repo, target, actor, None)
}

/// Same as [`undo`], accepting an optional precomputed `dirty_override` list for Wave 2+ CLI callers.
pub fn undo_with_dirty(
    repo: &OzoneRepo,
    target: impl Into<UndoTarget>,
    actor: ActorInfo,
    dirty_override: Option<&[FileChange]>,
) -> Result<UndoOutcome> {
    ensure_mount_healthy(repo)?;
    ensure_oplog_dirs(&repo.layout)?;

    let target = target.into();
    let entries = list_entries(&repo.layout)?;
    if entries.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "nothing to undo (operation log is empty)".to_string(),
            recovery: vec!["Run `oz checkpoint` or a mutating command first.".to_string()],
        });
    }

    let gix_repo = open_gix_for_workspace(repo)?;
    let ws_key = workspace_key(repo).to_string();
    let timeline = analyze_workspace_timeline(&entries, &ws_key);

    match target {
        UndoTarget::Steps(steps) => {
            if steps == 0 {
                return Err(OzError::InvalidUsage {
                    message: "undo steps must be at least 1".to_string(),
                    recovery: vec!["Pass `--steps 1` or omit `--steps`.".to_string()],
                });
            }

            let Some(&latest_active) = timeline.active_forward.last() else {
                return Err(OzError::InvalidUsage {
                    message: format!("nothing left to undo in workspace '{ws_key}'"),
                    recovery: vec!["Run `oz redo` to re-apply an undone operation.".to_string()],
                });
            };

            // If the latest active op is a Checkpoint and un-checkpointed edits exist on disk,
            // step 1 rewinds live working-tree edits back to that checkpoint.
            if latest_active.kind == OpKind::Checkpoint
                && let Some(cp_snap) = latest_active.ws_snapshots.get(&ws_key)
            {
                let (live_branch, live_head, live_base_tree) =
                    resolve_workspace_head(repo, &gix_repo)?;
                let live_dirty = scan_dirty_files(repo, &gix_repo, live_base_tree, dirty_override)?;
                let live_base_hex = live_base_tree.to_hex().to_string();

                if !snapshots_match(
                    &live_base_hex,
                    &live_dirty,
                    &cp_snap.base_tree,
                    &cp_snap.dirty_files,
                ) {
                    let live_slice = SnapshotSlice {
                        base_tree: live_base_tree,
                        dirty_tree: None,
                        dirty_files: live_dirty,
                        head_commit: live_head,
                        branch: live_branch,
                    };
                    if steps == 1 {
                        return rewind_to_checkpoint(
                            repo,
                            &gix_repo,
                            &ws_key,
                            latest_active,
                            cp_snap,
                            live_slice,
                            actor,
                        );
                    }

                    // steps > 1: rewinding un-checkpointed edits counts as step 1, then undo
                    // `steps - 1` active ops starting from `latest_active`.
                    let remaining = steps - 1;
                    let available = timeline.active_forward.len();
                    if remaining > available {
                        return Err(OzError::InvalidUsage {
                            message: format!(
                                "cannot undo {steps} steps: only {available} active operations exist after rewinding edits"
                            ),
                            recovery: vec![format!("Try `oz undo --steps {}`.", available + 1)],
                        });
                    }
                    let ops_rev: Vec<&OpLogEntry> = timeline
                        .active_forward
                        .iter()
                        .rev()
                        .take(remaining)
                        .copied()
                        .collect();
                    return execute_undo_ops(
                        repo,
                        &gix_repo,
                        &ws_key,
                        &ops_rev,
                        Some(live_slice),
                        actor,
                    );
                }
            }

            if steps > timeline.active_forward.len() {
                return Err(OzError::InvalidUsage {
                    message: format!(
                        "cannot undo {steps} steps: only {} active operation(s) in workspace '{ws_key}'",
                        timeline.active_forward.len()
                    ),
                    recovery: vec![format!(
                        "Try `oz undo --steps {}`.",
                        timeline.active_forward.len()
                    )],
                });
            }

            let ops_rev: Vec<&OpLogEntry> = timeline
                .active_forward
                .iter()
                .rev()
                .take(steps)
                .copied()
                .collect();
            execute_undo_ops(repo, &gix_repo, &ws_key, &ops_rev, None, actor)
        }
        UndoTarget::OpId(op_id) => {
            let target_entry = load_entry(&repo.layout, &op_id)?;
            let target_ws_key = if target_entry.ws_snapshots.contains_key(&ws_key) {
                ws_key.clone()
            } else if target_entry.ws_snapshots.len() == 1 {
                target_entry
                    .ws_snapshots
                    .keys()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| ws_key.clone())
            } else {
                ws_key.clone()
            };

            if target_entry.kind == OpKind::Undo {
                return redo_specific_undo(
                    repo,
                    &gix_repo,
                    &target_ws_key,
                    &target_entry,
                    actor,
                    false,
                );
            }
            if target_entry.kind == OpKind::Redo
                && let Some(ref orig_undo_id) = target_entry.inverse_of
            {
                let orig_undo = load_entry(&repo.layout, orig_undo_id)?;
                return undo_from_redo(
                    repo,
                    &gix_repo,
                    &target_ws_key,
                    &target_entry,
                    &orig_undo,
                    actor,
                );
            }

            let target_timeline = if target_ws_key == ws_key {
                timeline
            } else {
                analyze_workspace_timeline(&entries, &target_ws_key)
            };

            if target_timeline.undone_set.contains(&op_id) {
                return Err(OzError::InvalidUsage {
                    message: format!("operation '{op_id}' has already been undone"),
                    recovery: vec!["Run `oz redo` to re-apply it.".to_string()],
                });
            }

            if target_entry.kind == OpKind::Checkpoint
                && target_timeline
                    .active_forward
                    .last()
                    .is_some_and(|last| last.op_id == op_id)
                && let Some(cp_snap) = target_entry.ws_snapshots.get(&target_ws_key)
            {
                let (live_branch, live_head, live_base_tree) =
                    resolve_workspace_head(repo, &gix_repo)?;
                let live_dirty = scan_dirty_files(repo, &gix_repo, live_base_tree, dirty_override)?;
                let live_base_hex = live_base_tree.to_hex().to_string();

                if !snapshots_match(
                    &live_base_hex,
                    &live_dirty,
                    &cp_snap.base_tree,
                    &cp_snap.dirty_files,
                ) {
                    let live_slice = SnapshotSlice {
                        base_tree: live_base_tree,
                        dirty_tree: None,
                        dirty_files: live_dirty,
                        head_commit: live_head,
                        branch: live_branch,
                    };
                    return rewind_to_checkpoint(
                        repo,
                        &gix_repo,
                        &target_ws_key,
                        &target_entry,
                        cp_snap,
                        live_slice,
                        actor,
                    );
                }
            }

            let Some(pos) = target_timeline
                .active_forward
                .iter()
                .position(|e| e.op_id == op_id)
            else {
                return Err(OzError::InvalidUsage {
                    message: format!(
                        "operation '{op_id}' is not an active operation for workspace '{target_ws_key}'"
                    ),
                    recovery: vec!["Check `op_id` in `.git/ozone/oplog/ops/`.".to_string()],
                });
            };

            let ops_rev: Vec<&OpLogEntry> = target_timeline.active_forward[pos..]
                .iter()
                .rev()
                .copied()
                .collect();
            execute_undo_ops(repo, &gix_repo, &target_ws_key, &ops_rev, None, actor)
        }
    }
}

struct TransitionPlan {
    kind: OpKind,
    command: String,
    message: String,
    inverse_of: Option<String>,
    undone_ops: Vec<String>,
    outcome_ids: Vec<String>,
    rewound_uncheckpointed: bool,
    ref_deltas: BTreeMap<String, RefDelta>,
    checkpoint_oid_hint: Option<String>,
    reflog_label: &'static str,
}

fn apply_and_record_transition(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    ws_key: &str,
    src: SnapshotSlice,
    dst: SnapshotSlice,
    plan: TransitionPlan,
    actor: ActorInfo,
) -> Result<UndoOutcome> {
    let update_git_head = repo.workspace.is_none() || repo.is_linked_worktree;
    apply_cas_ref_deltas_and_head(
        update_git_head,
        gix_repo,
        &plan.ref_deltas,
        &src,
        &dst,
        plan.reflog_label,
    )?;

    let (restored_files, touched_paths) = restore_working_tree(
        repo,
        gix_repo,
        src.base_tree,
        &src.dirty_files,
        dst.base_tree,
        &dst.dirty_files,
    )?;

    if !touched_paths.is_empty() {
        sync_git_index_diff(
            repo,
            gix_repo,
            dst.base_tree,
            &dst.dirty_files,
            &touched_paths,
        )?;
    }

    let op_id = new_op_id();
    let restored_base_tree = dst.base_tree.to_hex().to_string();
    let restored_dirty_tree = dst.dirty_tree.map(|id| id.to_hex().to_string());
    let dst_head = dst.head_commit;
    let dst_base = dst.base_tree;
    let dst_dirty_for_state = dst.dirty_files.clone();
    let dst_branch = dst.branch.clone();

    let ws_snap = make_ws_snapshot(dst, Some(src));
    let mut ws_snapshots = BTreeMap::new();
    ws_snapshots.insert(ws_key.to_string(), ws_snap);

    let entry = OpLogEntry {
        op_id: op_id.clone(),
        parents: Vec::new(),
        timestamp: Utc::now(),
        actor,
        kind: plan.kind,
        command: plan.command,
        message: Some(plan.message),
        inverse_of: plan.inverse_of,
        undone_ops: plan.undone_ops,
        rewound_uncheckpointed: plan.rewound_uncheckpointed,
        ref_deltas: plan.ref_deltas.clone(),
        ws_snapshots,
    };

    let stored = append_entry(&repo.layout, entry)?;
    sync_workspace_state_file(
        repo,
        gix_repo,
        dst_head,
        dst_base,
        &dst_dirty_for_state,
        plan.checkpoint_oid_hint.as_deref(),
        dst_branch.as_deref(),
    )?;

    Ok(UndoOutcome {
        op_id,
        workspace: ws_key.to_string(),
        undone_op_ids: plan.outcome_ids,
        restored_base_tree,
        restored_dirty_tree,
        restored_files,
        ref_deltas: plan.ref_deltas,
        entry: stored,
    })
}

fn rewind_to_checkpoint(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    ws_key: &str,
    checkpoint_entry: &OpLogEntry,
    cp_snap: &WsSnapshot,
    live_slice: SnapshotSlice,
    actor: ActorInfo,
) -> Result<UndoOutcome> {
    let dst = post_slice(cp_snap)?;
    let cp_oid_hex = dst.dirty_tree.map(|id| id.to_hex().to_string());
    apply_and_record_transition(
        repo,
        gix_repo,
        ws_key,
        live_slice,
        dst,
        TransitionPlan {
            kind: OpKind::Undo,
            command: format!("oz undo {}", checkpoint_entry.op_id),
            message: format!(
                "rewind working tree to checkpoint {}",
                checkpoint_entry.op_id
            ),
            inverse_of: Some(checkpoint_entry.op_id.clone()),
            undone_ops: Vec::new(),
            outcome_ids: vec![checkpoint_entry.op_id.clone()],
            rewound_uncheckpointed: true,
            ref_deltas: BTreeMap::new(),
            checkpoint_oid_hint: cp_oid_hex,
            reflog_label: "oz undo",
        },
        actor,
    )
}

fn execute_undo_ops(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    ws_key: &str,
    ops_rev: &[&OpLogEntry],
    live_override: Option<SnapshotSlice>,
    actor: ActorInfo,
) -> Result<UndoOutcome> {
    let (newest_op, oldest_op) = match ops_rev {
        [] => {
            return Err(OzError::InvalidUsage {
                message: "no operations selected to undo".to_string(),
                recovery: Vec::new(),
            });
        }
        [only] => (only, only),
        [first, .., last] => (first, last),
    };

    let src = match live_override {
        Some(live) => live,
        None => match newest_op.ws_snapshots.get(ws_key) {
            Some(snap) => post_slice(snap)?,
            None => {
                let (branch, head_commit, base_tree) = resolve_workspace_head(repo, gix_repo)?;
                SnapshotSlice {
                    base_tree,
                    dirty_tree: None,
                    dirty_files: Vec::new(),
                    head_commit,
                    branch,
                }
            }
        },
    };

    let dst = match oldest_op.ws_snapshots.get(ws_key) {
        Some(snap) => pre_slice(snap, &oldest_op.op_id)?,
        None => {
            return Err(OzError::InvalidUsage {
                message: format!(
                    "operation '{}' has no snapshot for workspace '{ws_key}'",
                    oldest_op.op_id
                ),
                recovery: Vec::new(),
            });
        }
    };

    let ref_deltas = fold_inverse_ref_deltas(ops_rev)?;
    let undone_op_ids: Vec<String> = ops_rev.iter().map(|e| e.op_id.clone()).collect();
    let primary_target = oldest_op.op_id.clone();
    let cp_ref = format!("refs/ozone/checkpoints/{ws_key}");
    let cp_oid_hint = ref_deltas.get(&cp_ref).and_then(|d| d.new.clone());

    apply_and_record_transition(
        repo,
        gix_repo,
        ws_key,
        src,
        dst,
        TransitionPlan {
            kind: OpKind::Undo,
            command: format!("oz undo {primary_target}"),
            message: format!("undo {}", undone_op_ids.join(", ")),
            inverse_of: Some(primary_target),
            undone_ops: undone_op_ids.clone(),
            outcome_ids: undone_op_ids,
            rewound_uncheckpointed: false,
            ref_deltas,
            checkpoint_oid_hint: cp_oid_hint,
            reflog_label: "oz undo",
        },
        actor,
    )
}

fn undo_from_redo(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    ws_key: &str,
    redo_entry: &OpLogEntry,
    orig_undo: &OpLogEntry,
    actor: ActorInfo,
) -> Result<UndoOutcome> {
    let snap = redo_entry
        .ws_snapshots
        .get(ws_key)
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!("redo '{}' has no workspace snapshot", redo_entry.op_id),
            recovery: Vec::new(),
        })?;
    let src = post_slice(snap)?;
    let dst = pre_slice(snap, &redo_entry.op_id)?;
    let ref_deltas = invert_deltas(&redo_entry.ref_deltas);
    let cp_ref = format!("refs/ozone/checkpoints/{ws_key}");
    let cp_oid_hint = ref_deltas.get(&cp_ref).and_then(|d| d.new.clone());

    apply_and_record_transition(
        repo,
        gix_repo,
        ws_key,
        src,
        dst,
        TransitionPlan {
            kind: OpKind::Undo,
            command: format!("oz undo {}", redo_entry.op_id),
            message: format!("undo redo {}", redo_entry.op_id),
            inverse_of: orig_undo.inverse_of.clone(),
            undone_ops: orig_undo.undone_ops.clone(),
            outcome_ids: vec![redo_entry.op_id.clone()],
            rewound_uncheckpointed: orig_undo.rewound_uncheckpointed,
            ref_deltas,
            checkpoint_oid_hint: cp_oid_hint,
            reflog_label: "oz undo",
        },
        actor,
    )
}

/// Redoes the latest un-redone [`OpKind::Undo`] operation for the active workspace, reversing its
/// `ref_deltas` and restoring dirty working-tree files in `< 10 ms`.
pub fn redo(repo: &OzoneRepo, actor: ActorInfo) -> Result<RedoOutcome> {
    ensure_mount_healthy(repo)?;
    ensure_oplog_dirs(&repo.layout)?;

    let entries = list_entries(&repo.layout)?;
    let ws_key = workspace_key(repo).to_string();
    let timeline = analyze_workspace_timeline(&entries, &ws_key);

    let Some(target_undo) = timeline.latest_unredone_undo else {
        return Err(OzError::InvalidUsage {
            message: format!("nothing to redo in workspace '{ws_key}'"),
            recovery: vec!["Run `oz undo` first before `oz redo`.".to_string()],
        });
    };

    let gix_repo = open_gix_for_workspace(repo)?;
    let outcome = redo_specific_undo(repo, &gix_repo, &ws_key, target_undo, actor, true)?;
    Ok(RedoOutcome {
        op_id: outcome.op_id,
        workspace: outcome.workspace,
        redone_undo_op_id: target_undo.op_id.clone(),
        restored_base_tree: outcome.restored_base_tree,
        restored_dirty_tree: outcome.restored_dirty_tree,
        restored_files: outcome.restored_files,
        ref_deltas: outcome.ref_deltas,
        entry: outcome.entry,
    })
}

fn redo_specific_undo(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    ws_key: &str,
    target_undo: &OpLogEntry,
    actor: ActorInfo,
    emit_redo_kind: bool,
) -> Result<UndoOutcome> {
    let snap = target_undo
        .ws_snapshots
        .get(ws_key)
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!(
                "undo operation '{}' has no snapshot for workspace '{ws_key}'",
                target_undo.op_id
            ),
            recovery: Vec::new(),
        })?;

    let src = post_slice(snap)?;
    let dst = pre_slice(snap, &target_undo.op_id)?;
    let ref_deltas = invert_deltas(&target_undo.ref_deltas);
    let cp_ref = format!("refs/ozone/checkpoints/{ws_key}");
    let cp_oid_hint = ref_deltas.get(&cp_ref).and_then(|d| d.new.clone());

    apply_and_record_transition(
        repo,
        gix_repo,
        ws_key,
        src,
        dst,
        TransitionPlan {
            kind: if emit_redo_kind {
                OpKind::Redo
            } else {
                OpKind::Undo
            },
            command: if emit_redo_kind {
                format!("oz redo {}", target_undo.op_id)
            } else {
                format!("oz undo {}", target_undo.op_id)
            },
            message: format!("redo {}", target_undo.op_id),
            inverse_of: Some(target_undo.op_id.clone()),
            undone_ops: Vec::new(),
            outcome_ids: vec![target_undo.op_id.clone()],
            rewound_uncheckpointed: false,
            ref_deltas,
            checkpoint_oid_hint: cp_oid_hint,
            reflog_label: if emit_redo_kind { "oz redo" } else { "oz undo" },
        },
        actor,
    )
}
