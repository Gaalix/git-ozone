// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bounded `.git/ozone/oplog/` retention, DAG compaction, and garbage collection (`oz gc`).

use crate::store::{
    atomic_write_bytes, entry_path, head_path, lock_head, ops_dir, process_oplog_mutex,
    read_head_op_id, validate_op_id,
};
use crate::types::{
    DEFAULT_WORKSPACE_KEY, DirtyFileState, OpKind, OpLogEntry, OpLogGcOptions, OpLogGcOutcome,
};
use chrono::{DateTime, Duration, Utc};
use gix::bstr::ByteSlice;
use gix::refs::FullName;
use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};
use oz_core::{OzError, OzoneLayout, OzoneRepo, Result};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;

/// Compacts `.git/ozone/oplog/` and removes expired OpLog entries and checkpoint snapshot refs
/// while preserving `oplog/HEAD`, active workspace heads, undo-chain consistency, and all
/// Git objects referenced by surviving entries.
pub fn compact_oplog(repo: &OzoneRepo, opts: &OpLogGcOptions) -> Result<OpLogGcOutcome> {
    let now = opts.now.unwrap_or_else(Utc::now);
    let cutoff = compute_cutoff(now, opts.oplog_days);
    let layout = &repo.layout;

    if !layout.oplog_dir().is_dir() || !ops_dir(layout).is_dir() {
        return Ok(OpLogGcOutcome {
            dry_run: opts.dry_run,
            oplog_days: opts.oplog_days,
            cutoff_timestamp: cutoff,
            head_op_id: None,
            total_entries_before: 0,
            removed_entries: Vec::new(),
            retained_entries: 0,
            protected_entries: 0,
            relinked_entries: 0,
            pruned_checkpoint_refs: Vec::new(),
            removed_temp_files: Vec::new(),
            preserved_object_count: 0,
            reclaimed_bytes: 0,
            remaining_bytes: 0,
        });
    }

    let proc_mutex = process_oplog_mutex(&layout.oplog_dir());
    let _lock = if opts.dry_run {
        None
    } else {
        Some(lock_head(layout, &proc_mutex)?)
    };

    let head_op_id = read_head_op_id(layout).map_err(|err| OzError::CheckFailed {
        message: format!("cannot run oplog gc with invalid oplog/HEAD: {err}"),
        recovery: vec!["Run `oz fsck --repair` first.".to_string()],
    })?;

    let LoadedOps {
        entries,
        file_sizes,
        temp_files,
    } = load_all_ops_strict(layout)?;
    let total_entries_before = entries.len();

    let entries_by_id: HashMap<String, OpLogEntry> = entries
        .iter()
        .map(|e| (e.op_id.clone(), e.clone()))
        .collect();

    if let Some(ref hid) = head_op_id
        && !entries_by_id.contains_key(hid)
    {
        return Err(OzError::CheckFailed {
            message: format!("oplog/HEAD points to missing entry '{hid}'"),
            recovery: vec!["Run `oz fsck --repair` before `oz gc`.".to_string()],
        });
    }
    if head_op_id.is_none() && !entries.is_empty() {
        return Err(OzError::CheckFailed {
            message: format!(
                "oplog/HEAD is missing while {} entry/entries exist in ops/",
                entries.len()
            ),
            recovery: vec!["Run `oz fsck --repair` before `oz gc`.".to_string()],
        });
    }

    if entries.is_empty() && temp_files.is_empty() {
        let gix_repo = repo.open_gix().ok();
        let pruned_checkpoint_refs = if let Some(ref gr) = gix_repo {
            collect_stale_op_checkpoint_refs(gr, &HashSet::new())?
        } else {
            Vec::new()
        };
        if !opts.dry_run
            && !pruned_checkpoint_refs.is_empty()
            && let Some(ref gr) = gix_repo
        {
            delete_refs(gr, &pruned_checkpoint_refs)?;
        }
        return Ok(OpLogGcOutcome {
            dry_run: opts.dry_run,
            oplog_days: opts.oplog_days,
            cutoff_timestamp: cutoff,
            head_op_id,
            total_entries_before: 0,
            removed_entries: Vec::new(),
            retained_entries: 0,
            protected_entries: 0,
            relinked_entries: 0,
            pruned_checkpoint_refs,
            removed_temp_files: Vec::new(),
            preserved_object_count: 0,
            reclaimed_bytes: 0,
            remaining_bytes: 0,
        });
    }

    let active_workspaces = discover_active_workspaces(repo);
    let mut protected_ids = compute_protected_entries(&entries, head_op_id.as_deref(), &active_workspaces);

    // Initial retention set: protected entries + entries newer than `cutoff`.
    let mut keep_ids: HashSet<String> = protected_ids.clone();
    let mut non_protected_in_window: Vec<&OpLogEntry> = entries
        .iter()
        .filter(|e| !protected_ids.contains(&e.op_id) && e.timestamp >= cutoff)
        .collect();

    if let Some(cap) = opts.max_entries {
        let non_protected_budget = cap.saturating_sub(protected_ids.len());
        if non_protected_in_window.len() > non_protected_budget {
            // Keep newest entries within budget.
            let drop_count = non_protected_in_window.len() - non_protected_budget;
            non_protected_in_window.drain(0..drop_count);
        }
    }
    for e in non_protected_in_window {
        keep_ids.insert(e.op_id.clone());
    }

    // Close `keep_ids` under `inverse_of` and `undone_ops` so undo/redo chains stay consistent.
    expand_undo_chain_closure(&entries_by_id, &mut keep_ids);
    expand_undo_chain_closure(&entries_by_id, &mut protected_ids);

    let removed_ids: HashSet<String> = entries
        .iter()
        .filter(|e| !keep_ids.contains(&e.op_id))
        .map(|e| e.op_id.clone())
        .collect();

    let mut removed_entries: Vec<String> = removed_ids.iter().cloned().collect();
    removed_entries.sort();

    // Compute DAG parent relinking for surviving entries whose parents are removed.
    let mut relinked: Vec<(OpLogEntry, usize)> = Vec::new();
    let mut relinked_ids: HashSet<String> = HashSet::new();
    for entry in &entries {
        if !keep_ids.contains(&entry.op_id) {
            continue;
        }
        let new_parents = compute_relinked_parents(
            entry,
            &entries_by_id,
            &keep_ids,
            &removed_ids,
        );
        if new_parents != entry.parents {
            let mut updated = entry.clone();
            updated.parents = new_parents;
            let serialized = serde_json::to_vec_pretty(&updated)?;
            relinked_ids.insert(updated.op_id.clone());
            relinked.push((updated, serialized.len()));
        }
    }

    let gix_repo = repo.open_gix()?;
    let pruned_checkpoint_refs = collect_stale_op_checkpoint_refs(&gix_repo, &keep_ids)?;
    let preserved_object_count = count_referenced_objects(&entries, &keep_ids);

    let head_bytes = fs::metadata(head_path(layout))
        .map(|m| m.len())
        .unwrap_or(0);
    let mut reclaimed_bytes = 0u64;
    for id in &removed_entries {
        reclaimed_bytes += file_sizes.get(id).copied().unwrap_or(0);
    }
    let mut removed_temp_files = Vec::new();
    for tmp in &temp_files {
        reclaimed_bytes += fs::metadata(tmp).map(|m| m.len()).unwrap_or(0);
        if let Ok(rel) = tmp.strip_prefix(&repo.git_common_dir) {
            removed_temp_files.push(rel.to_string_lossy().replace('\\', "/"));
        } else {
            removed_temp_files.push(tmp.to_string_lossy().into_owned());
        }
    }
    removed_temp_files.sort();

    let mut remaining_bytes = head_bytes;
    for entry in &entries {
        if !keep_ids.contains(&entry.op_id) {
            continue;
        }
        if let Some((_, new_len)) = relinked.iter().find(|(u, _)| u.op_id == entry.op_id) {
            remaining_bytes += *new_len as u64;
        } else {
            remaining_bytes += file_sizes.get(&entry.op_id).copied().unwrap_or(0);
        }
    }

    let relinked_entries = relinked.len();
    let retained_entries = keep_ids.len();
    let protected_entries = protected_ids.intersection(&keep_ids).count();

    if !opts.dry_run {
        // 1. Write relinked parent chains BEFORE deleting expired ancestors.
        for (updated, _) in &relinked {
            let p = entry_path(layout, &updated.op_id)?;
            let json = serde_json::to_vec_pretty(updated)?;
            atomic_write_bytes(&p, &json)?;
        }

        // 2. Delete expired `ops/<op_id>.json` files.
        for id in &removed_entries {
            let p = entry_path(layout, id)?;
            match fs::remove_file(&p) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(OzError::Io(err)),
            }
        }

        // 3. Delete expired/orphaned `refs/ozone/op-checkpoints/<ws>/<op_id>` refs
        //    (never touching `refs/ozone/checkpoints/<ws>`).
        if !pruned_checkpoint_refs.is_empty() {
            delete_refs(&gix_repo, &pruned_checkpoint_refs)?;
        }

        // 4. Remove stale `.tmp.*` files in `oplog/` and `oplog/ops/`.
        for tmp in &temp_files {
            let _ = fs::remove_file(tmp);
        }
    }

    Ok(OpLogGcOutcome {
        dry_run: opts.dry_run,
        oplog_days: opts.oplog_days,
        cutoff_timestamp: cutoff,
        head_op_id,
        total_entries_before,
        removed_entries,
        retained_entries,
        protected_entries,
        relinked_entries,
        pruned_checkpoint_refs,
        removed_temp_files,
        preserved_object_count,
        reclaimed_bytes,
        remaining_bytes,
    })
}

fn compute_cutoff(now: DateTime<Utc>, oplog_days: u64) -> DateTime<Utc> {
    if oplog_days == 0 {
        return now + Duration::microseconds(1);
    }
    let clamped = oplog_days.min(365_000) as i64;
    now - Duration::days(clamped)
}

struct LoadedOps {
    entries: Vec<OpLogEntry>,
    file_sizes: HashMap<String, u64>,
    temp_files: Vec<PathBuf>,
}

fn load_all_ops_strict(layout: &OzoneLayout) -> Result<LoadedOps> {
    let oplog = layout.oplog_dir();
    let ops = ops_dir(layout);
    let mut temp_files = Vec::new();

    for dir in [&oplog, &ops] {
        if !dir.is_dir() {
            continue;
        }
        for item in fs::read_dir(dir)? {
            let item = item?;
            let name = item.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if name_str.starts_with(".tmp") && item.path().is_file() {
                temp_files.push(item.path());
            }
        }
    }

    let mut entries = Vec::new();
    let mut sizes = HashMap::new();
    for item in fs::read_dir(&ops)? {
        let item = item?;
        let path = item.path();
        let name = item.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.starts_with('.') {
            continue;
        }
        if !name_str.ends_with(".json") {
            return Err(OzError::CheckFailed {
                message: format!("unexpected non-json file in oplog/ops: '{}'", name_str),
                recovery: vec!["Run `oz fsck --repair` to clean oplog directory.".to_string()],
            });
        }
        let stem = &name_str[..name_str.len() - ".json".len()];
        validate_op_id(stem).map_err(|_| OzError::CheckFailed {
            message: format!("invalid oplog entry filename '{}'", name_str),
            recovery: vec!["Run `oz fsck --repair`.".to_string()],
        })?;
        let raw = fs::read_to_string(&path)?;
        let entry: OpLogEntry = serde_json::from_str(&raw).map_err(|err| OzError::CheckFailed {
            message: format!("corrupt oplog entry '{}': {err}", path.display()),
            recovery: vec!["Run `oz fsck --repair` to recover corrupt oplog entries.".to_string()],
        })?;
        if entry.op_id != stem {
            return Err(OzError::CheckFailed {
                message: format!(
                    "oplog entry '{}' has mismatched op_id '{}'",
                    path.display(),
                    entry.op_id
                ),
                recovery: vec!["Run `oz fsck --repair`.".to_string()],
            });
        }
        let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(raw.len() as u64);
        sizes.insert(entry.op_id.clone(), len);
        entries.push(entry);
    }

    entries.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.op_id.cmp(&b.op_id))
    });

    Ok(LoadedOps {
        entries,
        file_sizes: sizes,
        temp_files,
    })
}

fn discover_active_workspaces(repo: &OzoneRepo) -> HashSet<String> {
    let mut active = HashSet::new();
    active.insert(DEFAULT_WORKSPACE_KEY.to_string());
    if let Some(ref ws) = repo.workspace {
        active.insert(ws.clone());
    }

    let state_dir = repo.layout.state_dir();
    if let Ok(rd) = fs::read_dir(&state_dir) {
        for item in rd.flatten() {
            let p = item.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && !stem.starts_with('.')
            {
                active.insert(stem.to_string());
            }
        }
    }

    let ws_dir = repo.layout.ws_dir();
    if let Ok(rd) = fs::read_dir(&ws_dir) {
        for item in rd.flatten() {
            let p = item.path();
            if p.is_dir()
                && let Some(name) = p.file_name().and_then(|s| s.to_str())
                && !name.starts_with('.')
            {
                active.insert(name.to_string());
            }
        }
    }

    let wt_dir = repo.git_common_dir.join("worktrees");
    if let Ok(rd) = fs::read_dir(&wt_dir) {
        for item in rd.flatten() {
            let p = item.path();
            if p.is_dir()
                && let Some(name) = p.file_name().and_then(|s| s.to_str())
                && !name.starts_with('.')
            {
                active.insert(name.to_string());
            }
        }
    }

    active
}

fn entry_affects_workspace(entry: &OpLogEntry, ws_key: &str) -> bool {
    if entry.ws_snapshots.contains_key(ws_key) {
        return true;
    }
    match entry.actor.workspace.as_deref() {
        Some(ws) => ws == ws_key,
        None => ws_key == DEFAULT_WORKSPACE_KEY,
    }
}

fn undone_targets(entry: &OpLogEntry) -> Vec<String> {
    if !entry.undone_ops.is_empty() {
        entry.undone_ops.clone()
    } else {
        entry.inverse_of.iter().cloned().collect()
    }
}

fn compute_protected_entries(
    entries: &[OpLogEntry],
    head_op_id: Option<&str>,
    active_workspaces: &HashSet<String>,
) -> HashSet<String> {
    let mut protected = HashSet::new();
    if let Some(hid) = head_op_id {
        protected.insert(hid.to_string());
    }

    let mut entry_by_id: HashMap<&str, &OpLogEntry> = HashMap::with_capacity(entries.len());
    for e in entries {
        entry_by_id.insert(e.op_id.as_str(), e);
    }

    for ws_key in active_workspaces {
        let mut latest_for_ws: Option<&str> = None;
        let mut undone_set: HashSet<String> = HashSet::new();
        let mut undo_stack: Vec<&OpLogEntry> = Vec::new();
        let mut forward_entries: Vec<&OpLogEntry> = Vec::new();

        for entry in entries {
            if !entry_affects_workspace(entry, ws_key) {
                continue;
            }
            latest_for_ws = Some(entry.op_id.as_str());
            match entry.kind {
                OpKind::Mutate | OpKind::Checkpoint => {
                    undo_stack.clear();
                    forward_entries.push(entry);
                }
                OpKind::Undo => {
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

        if let Some(latest) = latest_for_ws {
            protected.insert(latest.to_string());
        }
        if let Some(last_active) = forward_entries
            .into_iter()
            .rfind(|e| !undone_set.contains(&e.op_id))
        {
            protected.insert(last_active.op_id.clone());
        }
        for undo_entry in undo_stack {
            protected.insert(undo_entry.op_id.clone());
        }
    }

    protected
}

fn expand_undo_chain_closure(
    entries_by_id: &HashMap<String, OpLogEntry>,
    keep_ids: &mut HashSet<String>,
) {
    let mut queue: VecDeque<String> = keep_ids.iter().cloned().collect();
    while let Some(id) = queue.pop_front() {
        let Some(entry) = entries_by_id.get(&id) else {
            continue;
        };
        if let Some(ref inv) = entry.inverse_of
            && entries_by_id.contains_key(inv)
            && keep_ids.insert(inv.clone())
        {
            queue.push_back(inv.clone());
        }
        for u in &entry.undone_ops {
            if entries_by_id.contains_key(u) && keep_ids.insert(u.clone()) {
                queue.push_back(u.clone());
            }
        }
    }
}

fn compute_relinked_parents(
    entry: &OpLogEntry,
    entries_by_id: &HashMap<String, OpLogEntry>,
    keep_ids: &HashSet<String>,
    removed_ids: &HashSet<String>,
) -> Vec<String> {
    let mut result = Vec::new();
    let mut seen_result = HashSet::new();
    let mut visited = HashSet::new();
    visited.insert(entry.op_id.clone());

    let mut queue: VecDeque<String> = entry.parents.iter().cloned().collect();
    while let Some(pid) = queue.pop_front() {
        if !visited.insert(pid.clone()) {
            continue;
        }
        if keep_ids.contains(&pid) {
            if seen_result.insert(pid.clone()) {
                result.push(pid);
            }
        } else if removed_ids.contains(&pid)
            && let Some(removed_parent) = entries_by_id.get(&pid)
        {
            for gp in &removed_parent.parents {
                queue.push_back(gp.clone());
            }
        }
    }
    result
}

fn collect_stale_op_checkpoint_refs(
    gix_repo: &gix::Repository,
    keep_ids: &HashSet<String>,
) -> Result<Vec<String>> {
    let platform = gix_repo
        .references()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let mut refs_to_prune = Vec::new();
    let prefixed = platform
        .prefixed("refs/ozone/op-checkpoints/")
        .map_err(|err| OzError::Git(err.to_string()))?;

    for item in prefixed {
        let Ok(reference) = item else {
            continue;
        };
        let full_name = reference.name().as_bstr().to_str_lossy().into_owned();
        if let Some(op_id) = full_name.rsplit('/').next()
            && !keep_ids.contains(op_id)
        {
            refs_to_prune.push(full_name);
        }
    }
    refs_to_prune.sort();
    Ok(refs_to_prune)
}

fn delete_refs(gix_repo: &gix::Repository, ref_names: &[String]) -> Result<()> {
    let mut edits = Vec::with_capacity(ref_names.len());
    for name in ref_names {
        let full = FullName::try_from(name.as_str())
            .map_err(|err| OzError::Git(format!("invalid ref name '{name}': {err}")))?;
        edits.push(RefEdit {
            change: Change::Delete {
                expected: PreviousValue::Any,
                log: RefLog::AndReference,
            },
            name: full,
            deref: false,
        });
    }
    if !edits.is_empty() {
        gix_repo
            .edit_references(edits)
            .map_err(|err| OzError::Git(err.to_string()))?;
    }
    Ok(())
}

fn count_referenced_objects(entries: &[OpLogEntry], keep_ids: &HashSet<String>) -> usize {
    let mut oids = HashSet::new();
    for entry in entries {
        if !keep_ids.contains(&entry.op_id) {
            continue;
        }
        for snap in entry.ws_snapshots.values() {
            oids.insert(snap.base_tree.clone());
            if let Some(ref oid) = snap.dirty_tree {
                oids.insert(oid.clone());
            }
            if let Some(ref oid) = snap.pre_base_tree {
                oids.insert(oid.clone());
            }
            if let Some(ref oid) = snap.pre_dirty_tree {
                oids.insert(oid.clone());
            }
            if let Some(ref oid) = snap.head_commit {
                oids.insert(oid.clone());
            }
            if let Some(ref oid) = snap.pre_head_commit {
                oids.insert(oid.clone());
            }
            for df in snap.dirty_files.iter().chain(snap.pre_dirty_files.iter()) {
                if let DirtyFileState::Present { blob_oid, .. } = &df.state {
                    oids.insert(blob_oid.clone());
                }
            }
        }
    }
    oids.len()
}

/// Computes chronological summary counts by workspace for callers/tests.
#[allow(dead_code)]
pub(crate) fn summarize_workspace_entries(
    entries: &[OpLogEntry],
) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for entry in entries {
        if entry.ws_snapshots.is_empty() {
            let ws = entry
                .actor
                .workspace
                .as_deref()
                .unwrap_or(DEFAULT_WORKSPACE_KEY);
            *counts.entry(ws.to_string()).or_insert(0) += 1;
        } else {
            for ws in entry.ws_snapshots.keys() {
                *counts.entry(ws.clone()).or_insert(0) += 1;
            }
        }
    }
    counts
}
