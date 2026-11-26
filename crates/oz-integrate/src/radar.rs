// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Live cross-workspace collision radar (`oz radar`) projecting dirty worktrees and
//! branch-diverged commits onto base-branch line coordinates via `gix::diff::blob`.

use crate::guardrails::CombinedScopePolicy;
use crate::sandbox::{
    collect_tree_leaf_map, commit_tree_id, merge_base_tree_id, normalize_branch_short,
    open_gix_with_fallback, parse_oid, resolve_local_branch, resolve_main_repo,
};
use crate::{CollisionReportItem, CollisionSeverity, WorkspaceLineSpan};
use gix::diff::blob::{Algorithm, Diff, Hunk, InternedInput, sources};
use gix::hash::ObjectId;
use oz_core::{
    ChangeKind, OzError, OzoneRepo, Result, WorkspaceState, compute_status, is_binary_bytes,
};
use oz_cow::list_workspaces;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;

/// Options controlling [`scan_radar`].
#[derive(Debug, Clone)]
pub struct RadarOptions {
    /// Base branch to project workspace edits onto (defaults to active repo branch or `"main"`).
    pub base_branch: Option<String>,
    /// Include branch-diverged commits in addition to uncommitted working-tree edits (default `true`).
    pub include_committed: bool,
    /// Optional subset of workspace names to scan (empty = all active workspaces).
    pub workspaces: Vec<String>,
}

impl Default for RadarOptions {
    fn default() -> Self {
        Self {
            base_branch: None,
            include_committed: true,
            workspaces: Vec::new(),
        }
    }
}

/// Complete collision radar report returned by [`scan_radar`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RadarReport {
    pub base_branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    pub scanned_workspaces: Vec<String>,
    pub collisions: Vec<CollisionReportItem>,
}

#[derive(Debug, Clone)]
struct ProjectedSpan {
    start_0: u32,
    end_0: u32,
    disp_start: u32,
    disp_end: u32,
}

#[derive(Debug, Clone)]
struct WorkspaceFileEdit {
    ws: String,
    spans: Vec<ProjectedSpan>,
    lines: String,
    is_binary: bool,
    is_new_file: bool,
    is_deleted: bool,
    content_hash: Option<ObjectId>,
    out_of_scope: bool,
}

/// Scans all active workspaces (`oz_cow::list_workspaces`), combining uncommitted dirty
/// working-tree edits (`oz_core::compute_status`) and (by default) branch-diverged commits
/// projected onto `base_branch` coordinates via `gix::diff::blob`, returning deterministic collisions.
pub fn scan_radar(repo: &OzoneRepo, opts: &RadarOptions) -> Result<RadarReport> {
    let main_repo = resolve_main_repo(repo)?;
    let gix_repo = open_gix_with_fallback(&main_repo.repo_root)?;

    let base_branch =
        resolve_radar_base_branch(repo, &main_repo, &gix_repo, opts.base_branch.as_deref());
    let base_commit_id = resolve_local_branch(&gix_repo, &base_branch)?.map(|(_, id)| id);
    let base_tree_id = match base_commit_id {
        Some(cid) => commit_tree_id(&gix_repo, cid)?,
        None => gix_repo.empty_tree().id().detach(),
    };
    let base_leaves = collect_tree_leaf_map(&gix_repo, base_tree_id)?;

    let mut summaries = list_workspaces(&main_repo)?;
    if !opts.workspaces.is_empty() {
        let filter: BTreeSet<&str> = opts.workspaces.iter().map(String::as_str).collect();
        summaries.retain(|ws| filter.contains(ws.state.name.as_str()));
    }

    let mut scanned_workspaces = Vec::with_capacity(summaries.len());
    let mut by_path: BTreeMap<String, Vec<WorkspaceFileEdit>> = BTreeMap::new();

    for summary in &summaries {
        let ws = &summary.state;
        scanned_workspaces.push(ws.name.clone());

        let edits = collect_workspace_edits(
            &main_repo,
            &gix_repo,
            ws,
            summary.branch_head_oid.as_deref(),
            base_commit_id,
            &base_leaves,
            opts.include_committed,
        )?;

        for (path, edit) in edits {
            by_path.entry(path).or_default().push(edit);
        }
    }

    let mut collisions = Vec::new();
    for (path, mut edits) in by_path {
        edits.sort_by(|a, b| a.ws.cmp(&b.ws));
        if let Some(item) = classify_path_collisions(path, &edits) {
            collisions.push(item);
        }
    }

    collisions.sort_by(|a, b| {
        severity_rank(a.severity)
            .cmp(&severity_rank(b.severity))
            .then_with(|| a.path.cmp(&b.path))
    });

    Ok(RadarReport {
        base_branch,
        base_commit: base_commit_id.map(|id| id.to_hex().to_string()),
        scanned_workspaces,
        collisions,
    })
}

fn resolve_radar_base_branch(
    repo: &OzoneRepo,
    main_repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    explicit: Option<&str>,
) -> String {
    if let Some(b) = explicit {
        return normalize_branch_short(b).to_string();
    }
    if repo.workspace.is_none()
        && let Some(ref br) = repo.branch
    {
        return br.clone();
    }
    if let Ok(Some(_)) = resolve_local_branch(gix_repo, "main") {
        return "main".to_string();
    }
    main_repo
        .branch
        .clone()
        .or_else(|| repo.branch.clone())
        .unwrap_or_else(|| "main".to_string())
}

fn collect_workspace_edits(
    main_repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    ws: &WorkspaceState,
    branch_head_hex: Option<&str>,
    base_commit_id: Option<ObjectId>,
    base_leaves: &BTreeMap<String, (ObjectId, gix::object::tree::EntryMode)>,
    include_committed: bool,
) -> Result<Vec<(String, WorkspaceFileEdit)>> {
    let ws_head_commit = match branch_head_hex {
        Some(hex) => Some(parse_oid(hex)?),
        None => resolve_local_branch(gix_repo, &ws.branch)?.map(|(_, id)| id),
    };
    let ws_head_tree_id = match ws_head_commit {
        Some(cid) => commit_tree_id(gix_repo, cid)?,
        None => gix_repo.empty_tree().id().detach(),
    };

    let fork_tree_id = if include_committed {
        match (base_commit_id, ws_head_commit) {
            (Some(b_cid), Some(w_cid)) => merge_base_tree_id(gix_repo, b_cid, w_cid)?,
            _ => match ws.base_commit.as_deref() {
                Some(hex) => commit_tree_id(gix_repo, parse_oid(hex)?)?,
                None => gix_repo.empty_tree().id().detach(),
            },
        }
    } else {
        ws_head_tree_id
    };

    let fork_leaves = if fork_tree_id == ws_head_tree_id {
        BTreeMap::new()
    } else {
        collect_tree_leaf_map(gix_repo, fork_tree_id)?
    };
    let ws_head_leaves = if fork_tree_id == ws_head_tree_id {
        BTreeMap::new()
    } else {
        collect_tree_leaf_map(gix_repo, ws_head_tree_id)?
    };

    let mut touched_paths = BTreeSet::new();
    if fork_tree_id != ws_head_tree_id {
        for (path, fork_val) in &fork_leaves {
            if ws_head_leaves.get(path) != Some(fork_val) {
                touched_paths.insert(path.clone());
            }
        }
        for (path, head_val) in &ws_head_leaves {
            if fork_leaves.get(path) != Some(head_val) {
                touched_paths.insert(path.clone());
            }
        }
    }

    // Collect live uncommitted working-tree edits if accessible.
    let mut dirty_map: BTreeMap<String, ChangeKind> = BTreeMap::new();
    if (ws.worktree_path.is_dir() || ws.backend.requires_daemon())
        && let Ok(ws_repo) = OzoneRepo::discover(Some(&ws.worktree_path))
        && let Ok(status) = compute_status(&ws_repo)
    {
        for change in status.changes {
            touched_paths.insert(change.path.clone());
            dirty_map.insert(change.path, change.kind);
        }
    }

    if touched_paths.is_empty() {
        return Ok(Vec::new());
    }

    let scope_policy = CombinedScopePolicy::from_repo_and_workspace(main_repo, Some(ws))?;
    let mut out = Vec::new();

    for path in touched_paths {
        let fork_bytes = if fork_tree_id == ws_head_tree_id {
            read_leaf_bytes(gix_repo, &ws_head_tree_id, &path)?
        } else {
            match fork_leaves.get(&path) {
                Some((oid, _)) => Some(read_blob_bytes(gix_repo, *oid)?),
                None => None,
            }
        };

        let effective_bytes = match dirty_map.get(&path) {
            Some(ChangeKind::Deleted) => None,
            Some(_) => read_workspace_disk_bytes(main_repo, ws, &path)?,
            None => {
                if fork_tree_id == ws_head_tree_id {
                    fork_bytes.clone()
                } else {
                    match ws_head_leaves.get(&path) {
                        Some((oid, _)) => Some(read_blob_bytes(gix_repo, *oid)?),
                        None => None,
                    }
                }
            }
        };

        let out_of_scope = scope_policy.is_out_of_scope(&path);

        // If working tree reverted committed edits back to fork state, skip unless mode/scope differs.
        if fork_bytes == effective_bytes {
            if out_of_scope && dirty_map.contains_key(&path) {
                out.push((
                    path,
                    WorkspaceFileEdit {
                        ws: ws.name.clone(),
                        spans: Vec::new(),
                        lines: "mode".to_string(),
                        is_binary: false,
                        is_new_file: false,
                        is_deleted: false,
                        content_hash: None,
                        out_of_scope,
                    },
                ));
            }
            continue;
        }

        let base_bytes = match base_leaves.get(&path) {
            Some((oid, _)) => Some(read_blob_bytes(gix_repo, *oid)?),
            None => None,
        };

        let edit = build_workspace_file_edit(
            &ws.name,
            gix_repo.object_hash(),
            fork_bytes.as_deref(),
            effective_bytes.as_deref(),
            base_bytes.as_deref(),
            out_of_scope,
        );
        out.push((path, edit));
    }

    Ok(out)
}

fn read_leaf_bytes(
    gix_repo: &gix::Repository,
    tree_id: &ObjectId,
    rel_path: &str,
) -> Result<Option<Vec<u8>>> {
    if *tree_id == gix_repo.empty_tree().id().detach() {
        return Ok(None);
    }
    let tree = gix_repo.find_tree(*tree_id).map_err(OzError::git_source)?;
    let Some(entry) = tree
        .lookup_entry_by_path(rel_path)
        .map_err(OzError::git_source)?
    else {
        return Ok(None);
    };
    if entry.mode().is_tree() || entry.mode().is_commit() {
        return Ok(None);
    }
    Ok(Some(read_blob_bytes(gix_repo, entry.object_id())?))
}

fn read_blob_bytes(gix_repo: &gix::Repository, oid: ObjectId) -> Result<Vec<u8>> {
    let blob = gix_repo.find_blob(oid).map_err(OzError::git_source)?;
    Ok(blob.data.clone())
}

fn read_workspace_disk_bytes(
    main_repo: &OzoneRepo,
    ws: &WorkspaceState,
    rel_path: &str,
) -> Result<Option<Vec<u8>>> {
    let candidate = if ws.backend.requires_daemon() {
        let upper = main_repo.layout.ws_upper_dir(&ws.name).join(rel_path);
        if fs::symlink_metadata(&upper).is_ok() {
            upper
        } else {
            ws.worktree_path.join(rel_path)
        }
    } else {
        ws.worktree_path.join(rel_path)
    };

    match fs::symlink_metadata(&candidate) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let target = fs::read_link(&candidate)?;
            Ok(Some(target.as_os_str().as_encoded_bytes().to_vec()))
        }
        Ok(meta) if !meta.is_dir() => Ok(Some(fs::read(&candidate)?)),
        Ok(_) => Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(OzError::Io(err)),
    }
}

fn build_workspace_file_edit(
    ws_name: &str,
    hash_kind: gix::hash::Kind,
    fork_bytes: Option<&[u8]>,
    effective_bytes: Option<&[u8]>,
    base_bytes: Option<&[u8]>,
    out_of_scope: bool,
) -> WorkspaceFileEdit {
    let is_new_file = fork_bytes.is_none();
    let is_deleted = effective_bytes.is_none();

    let fork_slice = fork_bytes.unwrap_or(&[]);
    let eff_slice = effective_bytes.unwrap_or(&[]);
    let base_slice = base_bytes.unwrap_or(fork_slice);

    let content_hash = effective_bytes
        .and_then(|b| gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, b).ok());

    if is_binary_bytes(fork_slice) || is_binary_bytes(eff_slice) {
        return WorkspaceFileEdit {
            ws: ws_name.to_string(),
            spans: Vec::new(),
            lines: "binary".to_string(),
            is_binary: true,
            is_new_file,
            is_deleted,
            content_hash,
            out_of_scope,
        };
    }

    let input = InternedInput::new(fork_slice, eff_slice);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let proj_hunks: Vec<Hunk> = if fork_slice == base_slice {
        Vec::new()
    } else {
        let proj_input = InternedInput::new(fork_slice, base_slice);
        let mut p_diff = Diff::compute(Algorithm::Histogram, &proj_input);
        p_diff.postprocess_lines(&proj_input);
        p_diff.hunks().collect()
    };

    let base_line_count =
        u32::try_from(sources::byte_lines(base_slice).count()).unwrap_or(u32::MAX);
    let eff_line_count = u32::try_from(sources::byte_lines(eff_slice).count()).unwrap_or(u32::MAX);

    let mut spans = Vec::new();
    for rh in diff.hunks() {
        let projected = if proj_hunks.is_empty() {
            rh.before.clone()
        } else {
            project_range_onto_base(&rh.before, &proj_hunks)
        };

        let (disp_start, disp_end) = if is_new_file {
            (1, eff_line_count.max(1))
        } else if is_deleted {
            (1, base_line_count.max(1))
        } else if projected.end > projected.start {
            (projected.start + 1, projected.end)
        } else {
            let line = projected.start + 1;
            (line, line)
        };

        spans.push(ProjectedSpan {
            start_0: projected.start,
            end_0: projected.end,
            disp_start,
            disp_end,
        });
    }

    if spans.is_empty() && (is_new_file || is_deleted) {
        let end = if is_new_file {
            eff_line_count.max(1)
        } else {
            base_line_count.max(1)
        };
        spans.push(ProjectedSpan {
            start_0: 0,
            end_0: base_line_count,
            disp_start: 1,
            disp_end: end,
        });
    }

    let lines = format_projected_spans(&spans);

    WorkspaceFileEdit {
        ws: ws_name.to_string(),
        spans,
        lines,
        is_binary: false,
        is_new_file,
        is_deleted,
        content_hash,
        out_of_scope,
    }
}

/// Maps a 0-based half-open line range in `fork` coordinates onto `base` coordinates
/// using `proj_hunks` computed from `fork -> base`.
fn project_range_onto_base(range: &Range<u32>, proj_hunks: &[Hunk]) -> Range<u32> {
    let start = project_pos_onto_base(range.start, false, proj_hunks);
    let end = project_pos_onto_base(range.end, true, proj_hunks);
    if end < start {
        start..start
    } else {
        start..end
    }
}

fn clamp_projected_pos(pos: u32, offset: i64) -> u32 {
    let shifted = i64::from(pos)
        .saturating_add(offset)
        .clamp(0, i64::from(u32::MAX));
    u32::try_from(shifted).unwrap_or(u32::MAX)
}

fn project_pos_onto_base(pos: u32, is_end: bool, proj_hunks: &[Hunk]) -> u32 {
    let mut offset: i64 = 0;
    for ph in proj_hunks {
        if pos < ph.before.start {
            return clamp_projected_pos(pos, offset);
        }
        if pos <= ph.before.end {
            if pos == ph.before.start && !is_end {
                return ph.after.start;
            }
            if pos == ph.before.end && is_end {
                return ph.after.end;
            }
            let rel = pos.saturating_sub(ph.before.start);
            return ph.after.start.saturating_add(rel).min(ph.after.end);
        }
        let after_len = i64::try_from(ph.after.len()).unwrap_or(i64::from(u32::MAX));
        let before_len = i64::try_from(ph.before.len()).unwrap_or(i64::from(u32::MAX));
        offset = offset.saturating_add(after_len.saturating_sub(before_len));
    }
    clamp_projected_pos(pos, offset)
}

fn format_projected_spans(spans: &[ProjectedSpan]) -> String {
    if spans.is_empty() {
        return "0-0".to_string();
    }
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for s in spans {
        if let Some(last) = merged.last_mut()
            && s.disp_start <= last.1 + 1
        {
            last.1 = last.1.max(s.disp_end);
            continue;
        }
        merged.push((s.disp_start, s.disp_end));
    }
    merged
        .into_iter()
        .map(|(s, e)| format!("{s}-{e}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn edits_overlap(a: &WorkspaceFileEdit, b: &WorkspaceFileEdit) -> bool {
    if a.is_binary || b.is_binary {
        return a.content_hash != b.content_hash;
    }
    if a.is_new_file && b.is_new_file {
        return a.content_hash != b.content_hash;
    }
    if a.is_deleted || b.is_deleted {
        return true;
    }
    for sa in &a.spans {
        for sb in &b.spans {
            if sa.start_0 <= sb.end_0 && sb.start_0 <= sa.end_0 {
                return true;
            }
        }
    }
    false
}

fn classify_path_collisions(
    path: String,
    edits: &[WorkspaceFileEdit],
) -> Option<CollisionReportItem> {
    if edits.is_empty() {
        return None;
    }

    let mut has_overlap = false;
    if edits.len() >= 2 {
        'outer: for i in 0..edits.len() {
            for j in (i + 1)..edits.len() {
                if edits_overlap(&edits[i], &edits[j]) {
                    has_overlap = true;
                    break 'outer;
                }
            }
        }
    }

    let has_out_of_scope = edits.iter().any(|e| e.out_of_scope);
    let all_identical_edits = edits.len() >= 2
        && edits
            .windows(2)
            .all(|w| w[0].is_deleted == w[1].is_deleted && w[0].content_hash == w[1].content_hash);

    let (severity, relevant_edits, suggested_action) = if has_overlap {
        let ws_names: Vec<&str> = edits.iter().map(|e| e.ws.as_str()).collect();
        let action = if ws_names.len() >= 2 {
            format!(
                "Pause {} or serialize integration after {}",
                ws_names[1], ws_names[0]
            )
        } else {
            "Serialize overlapping workspace integration".to_string()
        };
        (CollisionSeverity::OverlappingHunks, edits.to_vec(), action)
    } else if has_out_of_scope {
        let oos_edits: Vec<WorkspaceFileEdit> =
            edits.iter().filter(|e| e.out_of_scope).cloned().collect();
        let oos_names: Vec<&str> = oos_edits.iter().map(|e| e.ws.as_str()).collect();
        let action = format!(
            "Revert or move out-of-scope edits in {} (or integrate with --allow-out-of-scope)",
            oos_names.join(", ")
        );
        (CollisionSeverity::OutOfScope, oos_edits, action)
    } else if edits.len() >= 2 && !all_identical_edits {
        let ws_names: Vec<&str> = edits.iter().map(|e| e.ws.as_str()).collect();
        let action = format!(
            "Non-overlapping edits across {}; safe to auto-merge via `oz integrate {}`",
            ws_names.join(", "),
            ws_names.join(" ")
        );
        (
            CollisionSeverity::SameFileDifferentLines,
            edits.to_vec(),
            action,
        )
    } else {
        return None;
    };

    let workspaces = relevant_edits
        .into_iter()
        .map(|e| WorkspaceLineSpan {
            ws: e.ws,
            lines: e.lines,
        })
        .collect();

    Some(CollisionReportItem {
        path,
        severity,
        workspaces,
        suggested_action,
    })
}

const fn severity_rank(severity: CollisionSeverity) -> u8 {
    match severity {
        CollisionSeverity::OverlappingHunks => 0,
        CollisionSeverity::OutOfScope => 1,
        CollisionSeverity::SameFileDifferentLines => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::{Hunk, project_pos_onto_base};

    #[test]
    fn project_pos_onto_base_clamps_underflow_and_u32_max_overflow() {
        let shrink_hunk = [Hunk {
            before: 0..100,
            after: 0..5,
        }];
        assert_eq!(project_pos_onto_base(120, false, &shrink_hunk), 25);

        let huge_delete_then_gap = [
            Hunk {
                before: 0..500,
                after: 0..0,
            },
            Hunk {
                before: 600..610,
                after: 0..10,
            },
        ];
        // `pos = 510` minus `500` -> `10`; artificial extreme shrink clamps to `0`.
        assert_eq!(project_pos_onto_base(510, false, &huge_delete_then_gap), 10);

        let huge_insert = [Hunk {
            before: 0..1,
            after: 0..u32::MAX,
        }];
        assert_eq!(project_pos_onto_base(10, false, &huge_insert), u32::MAX);
        assert_eq!(
            project_pos_onto_base(u32::MAX, true, &huge_insert),
            u32::MAX
        );
    }
}
