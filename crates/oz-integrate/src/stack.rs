// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-memory stacked-branch rebasing (`oz stack rebase`) and stack inspection (`oz stack status`)
//! preserving `Change-Id:` trailers across every rebased commit, with atomic multi-ref CAS updates,
//! checked-out workspace syncing, conflict sandbox continuation, and `< 10 ms` `oz undo`.

use crate::sandbox::{
    IntegrationMode, IntegrationSession, RebasedCommitInfo, StackRebaseSessionState,
    StackedBranchPlan, apply_tree_delta_with_cleanup, cleanup_session_and_sandbox, commit_tree_id,
    conflicting_dirty_paths, extract_unresolved_conflicts, find_merge_base_commit,
    integrate_sandbox_path, is_ancestor, load_session, materialize_integrate_sandbox, merge_labels,
    normalize_branch_short, open_gix_with_fallback, parse_oid, preserve_or_add_change_id,
    resolve_branch_or_revision, resolve_local_branch, resolve_main_repo, resolve_signatures,
    save_session,
    sync_git_index_for_delta, verify_and_build_resolved_tree, write_commit_in_odb,
    write_workspace_state_atomic,
};
use chrono::Utc;
use gix::bstr::ByteSlice;
use gix::hash::ObjectId;
use gix::merge::tree::TreatAsUnresolved;
use gix::refs::transaction::{PreviousValue, RefEdit};
use gix::refs::{FullName, Target};
use oz_core::{MountHealth, OzError, OzoneRepo, WorkspaceState, compute_status, Result};
use oz_cow::list_workspaces;
use oz_cow::materialize::compute_tree_delta;
use oz_oplog::{ActorInfo, MutationTransaction};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Options controlling [`stack_rebase`].
#[derive(Debug, Clone, Default)]
pub struct StackRebaseOptions {
    /// Stacked branch names to rebase (if empty, auto-discovers stacked branches above `onto`).
    pub branches: Vec<String>,
    /// Target base branch to rebase onto (defaults to `"main"`).
    pub onto: Option<String>,
}

/// Summary for a single branch updated by [`stack_rebase`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebasedBranchOutcome {
    pub branch: String,
    pub previous_oid: String,
    pub new_oid: String,
    pub commit_count: usize,
}

/// Complete outcome returned by [`stack_rebase`] or [`continue_stack_rebase`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackRebaseOutcome {
    pub onto_branch: String,
    pub onto_oid: String,
    pub branches: Vec<RebasedBranchOutcome>,
    pub rebased_commits: Vec<RebasedCommitInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op_id: Option<String>,
    #[serde(default)]
    pub updated_workspaces: Vec<String>,
}

/// Summary of a single commit inside a stacked branch for [`stack_status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackCommitSummary {
    pub oid: String,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
}

/// Status of a single stacked branch returned by [`stack_status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackBranchStatus {
    pub branch: String,
    pub head_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub ahead_count: usize,
    pub behind_onto_count: usize,
    pub needs_rebase: bool,
    pub commits: Vec<StackCommitSummary>,
}

/// Full stack status report returned by [`stack_status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackStatusReport {
    pub onto_branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onto_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_branch: Option<String>,
    pub branches: Vec<StackBranchStatus>,
}

/// Inspects local stacked branches relative to `onto` (default `"main"`).
pub fn stack_status(repo: &OzoneRepo, onto: Option<&str>) -> Result<StackStatusReport> {
    let main_repo = resolve_main_repo(repo)?;
    let gix_repo = open_gix_with_fallback(&main_repo.repo_root)?;
    let onto_branch = normalize_branch_short(onto.unwrap_or("main")).to_string();
    let onto_oid = resolve_local_branch(&gix_repo, &onto_branch)?.map(|(_, id)| id);

    let ws_by_branch = map_workspaces_by_branch(&main_repo)?;
    let mut all_branches: Vec<(String, ObjectId)> = Vec::new();
    let platform = gix_repo
        .references()
        .map_err(|err| OzError::Git(err.to_string()))?;
    for item in platform
        .local_branches()
        .map_err(|err| OzError::Git(err.to_string()))?
    {
        let mut r = item.map_err(|err| OzError::Git(err.to_string()))?;
        let full = r.name().as_bstr().to_str_lossy().into_owned();
        let short = normalize_branch_short(&full).to_string();
        if short == onto_branch {
            continue;
        }
        if let Ok(id) = r.peel_to_id() {
            let oid = id.detach();
            if let Some(o_id) = onto_oid
                && is_ancestor(&gix_repo, oid, o_id)?
            {
                // Already merged into `onto_branch`; include only if explicitly backed by an active workspace?
                // Skip merged branches in stack view unless they have unmerged descendants.
                continue;
            }
            all_branches.push((short, oid));
        }
    }

    let ordered = sort_branches_topologically(&gix_repo, &all_branches)?;
    let mut statuses = Vec::with_capacity(ordered.len());
    let mut branch_needs_rebase: HashMap<String, bool> = HashMap::new();

    for (idx, (branch, oid)) in ordered.iter().enumerate() {
        let parent_branch = find_parent_branch_in_list(&gix_repo, *oid, &ordered[..idx])?;
        let parent_oid = match &parent_branch {
            Some(pb) => ordered
                .iter()
                .find(|(b, _)| b == pb)
                .map(|(_, id)| *id),
            None => match onto_oid {
                Some(o_id) => find_merge_base_commit(&gix_repo, o_id, *oid)?,
                None => None,
            },
        };

        let commits = collect_commit_range(&gix_repo, *oid, parent_oid)?;
        let commit_summaries = commits
            .iter()
            .filter_map(|cid| summarize_commit(&gix_repo, *cid).ok())
            .collect::<Vec<_>>();

        let behind_onto_count = match onto_oid {
            Some(o_id) => {
                let mb = find_merge_base_commit(&gix_repo, o_id, *oid)?;
                count_first_parent_commits(&gix_repo, o_id, mb)?
            }
            None => 0,
        };

        let parent_needs = parent_branch
            .as_deref()
            .and_then(|p| branch_needs_rebase.get(p).copied())
            .unwrap_or(false);
        let needs_rebase = behind_onto_count > 0 || parent_needs;
        branch_needs_rebase.insert(branch.clone(), needs_rebase);

        statuses.push(StackBranchStatus {
            branch: branch.clone(),
            head_oid: oid.to_hex().to_string(),
            parent_branch,
            workspace: ws_by_branch.get(branch).map(|w| w.name.clone()),
            ahead_count: commit_summaries.len(),
            behind_onto_count,
            needs_rebase,
            commits: commit_summaries,
        });
    }

    Ok(StackStatusReport {
        onto_branch,
        onto_oid: onto_oid.map(|id| id.to_hex().to_string()),
        active_branch: repo.branch.clone(),
        branches: statuses,
    })
}

/// Rebases one or more stacked branches onto `opts.onto` (default `"main"`) 100% in memory
/// via `gix_repo.merge_trees(parent_tree, current_onto_tree, commit_tree, ...)`, preserving
/// `Change-Id:` trailers across every rebased commit.
pub fn stack_rebase(
    repo: &OzoneRepo,
    opts: &StackRebaseOptions,
    actor: ActorInfo,
) -> Result<StackRebaseOutcome> {
    let main_repo = resolve_main_repo(repo)?;
    main_repo.layout.ensure_initialized()?;

    if let Some(existing) = load_session(&main_repo)? {
        return Err(OzError::InvalidUsage {
            message: format!(
                "an unresolved {:?} conflict session is already active at {}",
                existing.mode,
                existing.sandbox_path.display()
            ),
            recovery: vec![
                "oz resolve --all --take ours --continue".to_string(),
                "oz stack rebase --continue".to_string(),
                "oz integrate --abort".to_string(),
            ],
        });
    }

    let gix_repo = open_gix_with_fallback(&main_repo.repo_root)?;
    let onto_branch = normalize_branch_short(opts.onto.as_deref().unwrap_or("main")).to_string();
    let Some((_, onto_commit_id)) = resolve_branch_or_revision(&gix_repo, &onto_branch)? else {
        return Err(OzError::InvalidUsage {
            message: format!("target base branch '{onto_branch}' does not exist"),
            recovery: vec![format!("Create '{onto_branch}' or pass --onto <existing-branch>.")],
        });
    };

    let target_branches = resolve_stack_branches_to_rebase(repo, &main_repo, &gix_repo, &onto_branch, opts)?;
    if target_branches.is_empty() {
        return Err(OzError::InvalidUsage {
            message: format!("no stacked branches found to rebase onto '{onto_branch}'"),
            recovery: vec![format!(
                "oz stack rebase <branch1> [<branch2>...] --onto {onto_branch}"
            )],
        });
    }

    let ordered = sort_branches_topologically(&gix_repo, &target_branches)?;
    let mut plans = Vec::with_capacity(ordered.len());

    for (idx, (branch, orig_oid)) in ordered.iter().enumerate() {
        let orig_tree = commit_tree_id(&gix_repo, *orig_oid)?;
        let parent_branch = find_parent_branch_in_list(&gix_repo, *orig_oid, &ordered[..idx])?;

        let base_stop = match &parent_branch {
            Some(pb) => ordered
                .iter()
                .find(|(b, _)| b == pb)
                .map(|(_, id)| *id),
            None => find_merge_base_commit(&gix_repo, onto_commit_id, *orig_oid)?,
        };

        let commits_oldest_first = collect_commit_range(&gix_repo, *orig_oid, base_stop)?;
        let commit_hexes: Vec<String> = commits_oldest_first
            .iter()
            .map(|id| id.to_hex().to_string())
            .collect();

        plans.push(StackedBranchPlan {
            branch: branch.clone(),
            original_oid: orig_oid.to_hex().to_string(),
            original_tree: orig_tree.to_hex().to_string(),
            commits: commit_hexes,
            parent_branch,
        });
    }

    let mut state = StackRebaseSessionState {
        onto_branch,
        onto_commit_oid: onto_commit_id.to_hex().to_string(),
        branches: plans,
        current_branch_index: 0,
        current_commit_index: 0,
        current_onto_oid: onto_commit_id.to_hex().to_string(),
        conflict_merged_tree_oid: String::new(),
        completed_branch_tips: BTreeMap::new(),
        rebased_commits: Vec::new(),
    };

    execute_stack_rebase_loop(repo, &main_repo, &gix_repo, &mut state, actor)
}

/// Resumes a paused `StackRebase` conflict session after conflicts in `../<repo>.oz/_integrate`
/// have been resolved, builds the resolved commit preserving its `Change-Id:`, replays remaining
/// commits/branches in the stack, and atomically updates all `refs/heads/*`.
pub fn continue_stack_rebase(repo: &OzoneRepo, actor: ActorInfo) -> Result<StackRebaseOutcome> {
    let main_repo = resolve_main_repo(repo)?;
    let Some(session) = load_session(&main_repo)? else {
        return Err(OzError::InvalidUsage {
            message: "no active stack rebase conflict session to continue".to_string(),
            recovery: vec!["Run `oz stack rebase <branches...> --onto main` first.".to_string()],
        });
    };
    if session.mode != IntegrationMode::StackRebase {
        return Err(OzError::InvalidUsage {
            message: "active conflict session belongs to `oz integrate`, not `oz stack rebase`"
                .to_string(),
            recovery: vec!["Run `oz integrate --continue` or `oz integrate --abort`.".to_string()],
        });
    }
    let mut state = session
        .stack_rebase
        .clone()
        .ok_or_else(|| OzError::Other("session missing `stack_rebase` state".to_string()))?;

    let gix_repo = open_gix_with_fallback(&main_repo.repo_root)?;
    let resolved_tree_id = verify_and_build_resolved_tree(&main_repo, &gix_repo, &session)?;

    let plan = &state.branches[state.current_branch_index];
    let orig_commit_hex = &plan.commits[state.current_commit_index];
    let orig_commit_oid = parse_oid(orig_commit_hex)?;
    let current_onto_oid = parse_oid(&state.current_onto_oid)?;

    let (new_commit_oid, change_id) = build_rebased_commit_from_original(
        &gix_repo,
        &actor,
        orig_commit_oid,
        current_onto_oid,
        resolved_tree_id,
    )?;

    state.rebased_commits.push(RebasedCommitInfo {
        branch: plan.branch.clone(),
        original_oid: orig_commit_hex.clone(),
        new_oid: new_commit_oid.to_hex().to_string(),
        change_id,
    });
    state.current_onto_oid = new_commit_oid.to_hex().to_string();
    state.current_commit_index += 1;

    if state.current_commit_index >= plan.commits.len() {
        state
            .completed_branch_tips
            .insert(plan.branch.clone(), state.current_onto_oid.clone());
        state.current_branch_index += 1;
        state.current_commit_index = 0;
        if state.current_branch_index < state.branches.len() {
            let next_onto = initial_onto_for_branch(&state, state.current_branch_index);
            state.current_onto_oid = next_onto;
        }
    }

    execute_stack_rebase_loop(repo, &main_repo, &gix_repo, &mut state, actor)
}

fn initial_onto_for_branch(state: &StackRebaseSessionState, branch_idx: usize) -> String {
    let plan = &state.branches[branch_idx];
    if let Some(ref pb) = plan.parent_branch
        && let Some(tip) = state.completed_branch_tips.get(pb)
    {
        return tip.clone();
    }
    state.onto_commit_oid.clone()
}

fn execute_stack_rebase_loop(
    caller_repo: &OzoneRepo,
    main_repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    state: &mut StackRebaseSessionState,
    actor: ActorInfo,
) -> Result<StackRebaseOutcome> {
    let onto_oid = parse_oid(&state.onto_commit_oid)?;

    while state.current_branch_index < state.branches.len() {
        if state.current_commit_index == 0 {
            state.current_onto_oid = initial_onto_for_branch(state, state.current_branch_index);

            let plan = &state.branches[state.current_branch_index];
            let orig_oid = parse_oid(&plan.original_oid)?;
            let target_onto_oid = parse_oid(&state.current_onto_oid)?;

            // Fast-skip replay when branch is already linearly on top of `target_onto_oid`.
            let mb = find_merge_base_commit(gix_repo, target_onto_oid, orig_oid)?;
            if mb == Some(target_onto_oid) && plan.parent_branch.is_none() {
                state
                    .completed_branch_tips
                    .insert(plan.branch.clone(), plan.original_oid.clone());
                state.current_branch_index += 1;
                continue;
            }
            if let Some(ref pb) = plan.parent_branch {
                let orig_parent_oid = state
                    .branches
                    .iter()
                    .find(|p| p.branch == *pb)
                    .map(|p| p.original_oid.as_str());
                if orig_parent_oid == Some(state.current_onto_oid.as_str()) {
                    state
                        .completed_branch_tips
                        .insert(plan.branch.clone(), plan.original_oid.clone());
                    state.current_branch_index += 1;
                    continue;
                }
            }
        }

        let plan_branch = state.branches[state.current_branch_index].branch.clone();
        let commits_len = state.branches[state.current_branch_index].commits.len();

        while state.current_commit_index < commits_len {
            let orig_commit_hex =
                state.branches[state.current_branch_index].commits[state.current_commit_index]
                    .clone();
            let orig_commit_oid = parse_oid(&orig_commit_hex)?;
            let orig_commit = gix_repo
                .find_commit(orig_commit_oid)
                .map_err(|err| OzError::Git(err.to_string()))?;
            let commit_tree = orig_commit
                .tree_id()
                .map_err(|err| OzError::Git(err.to_string()))?
                .detach();
            let parent_tree = match orig_commit.parent_ids().next() {
                Some(pid) => commit_tree_id(gix_repo, pid.detach())?,
                None => gix_repo.empty_tree().id().detach(),
            };

            let current_onto_oid = parse_oid(&state.current_onto_oid)?;
            let current_onto_tree = commit_tree_id(gix_repo, current_onto_oid)?;

            let merge_opts = gix_repo
                .tree_merge_options()
                .map_err(|err| OzError::Git(err.to_string()))?;
            let short_hex = &orig_commit_hex[..orig_commit_hex.len().min(8)];
            let labels = merge_labels("parent", &state.onto_branch, short_hex);
            let mut outcome = gix_repo
                .merge_trees(parent_tree, current_onto_tree, commit_tree, labels, merge_opts)
                .map_err(|err| OzError::Git(err.to_string()))?;

            if outcome.has_unresolved_conflicts(TreatAsUnresolved::git()) {
                let merged_tree_id = outcome
                    .tree
                    .write()
                    .map_err(|err| OzError::Git(err.to_string()))?
                    .detach();
                let (stored_conflicts, conflict_entries) = extract_unresolved_conflicts(
                    gix_repo,
                    &outcome.conflicts,
                    merged_tree_id,
                )?;

                let sandbox_dir = integrate_sandbox_path(main_repo)?;
                materialize_integrate_sandbox(
                    main_repo,
                    gix_repo,
                    &sandbox_dir,
                    merged_tree_id,
                    Some(onto_oid),
                )?;

                state.conflict_merged_tree_oid = merged_tree_id.to_hex().to_string();
                let now = Utc::now();
                let session = IntegrationSession {
                    schema_version: IntegrationSession::CURRENT_SCHEMA_VERSION,
                    mode: IntegrationMode::StackRebase,
                    sandbox_path: sandbox_dir.clone(),
                    created_at: now,
                    updated_at: now,
                    conflicts: stored_conflicts,
                    integrate: None,
                    stack_rebase: Some(state.clone()),
                };
                save_session(main_repo, &session)?;

                let mut recovery: Vec<String> = conflict_entries
                    .iter()
                    .flat_map(|c| c.suggested_commands.iter().cloned())
                    .collect();
                recovery.push("oz resolve --all --take ours --continue".to_string());
                recovery.push("oz stack rebase --continue".to_string());
                recovery.push("oz integrate --abort".to_string());

                return Err(OzError::Conflict {
                    message: format!(
                        "{} conflict(s) rebasing commit {} on branch '{}' onto '{}' (materialized in {})",
                        conflict_entries.len(),
                        short_hex,
                        plan_branch,
                        state.onto_branch,
                        sandbox_dir.display()
                    ),
                    conflicts: conflict_entries,
                    recovery,
                });
            }

            let new_tree_id = outcome
                .tree
                .write()
                .map_err(|err| OzError::Git(err.to_string()))?
                .detach();

            let (new_commit_oid, change_id) = build_rebased_commit_from_original(
                gix_repo,
                &actor,
                orig_commit_oid,
                current_onto_oid,
                new_tree_id,
            )?;

            state.rebased_commits.push(RebasedCommitInfo {
                branch: plan_branch.clone(),
                original_oid: orig_commit_hex,
                new_oid: new_commit_oid.to_hex().to_string(),
                change_id,
            });
            state.current_onto_oid = new_commit_oid.to_hex().to_string();
            state.current_commit_index += 1;
        }

        state
            .completed_branch_tips
            .insert(plan_branch, state.current_onto_oid.clone());
        state.current_branch_index += 1;
        state.current_commit_index = 0;
    }

    finalize_stack_rebase(caller_repo, main_repo, gix_repo, state, actor)
}

fn finalize_stack_rebase(
    caller_repo: &OzoneRepo,
    main_repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    state: &StackRebaseSessionState,
    actor: ActorInfo,
) -> Result<StackRebaseOutcome> {
    let mut ref_edits = Vec::new();
    let mut branch_outcomes = Vec::with_capacity(state.branches.len());
    let mut changed_trees: HashMap<String, (ObjectId, ObjectId, ObjectId)> = HashMap::new();

    for plan in &state.branches {
        let new_oid_hex = state
            .completed_branch_tips
            .get(&plan.branch)
            .cloned()
            .unwrap_or_else(|| plan.original_oid.clone());
        let old_oid = parse_oid(&plan.original_oid)?;
        let new_oid = parse_oid(&new_oid_hex)?;
        let old_tree = parse_oid(&plan.original_tree)?;
        let new_tree = commit_tree_id(gix_repo, new_oid)?;

        let commit_count = state
            .rebased_commits
            .iter()
            .filter(|c| c.branch == plan.branch)
            .count();

        branch_outcomes.push(RebasedBranchOutcome {
            branch: plan.branch.clone(),
            previous_oid: plan.original_oid.clone(),
            new_oid: new_oid_hex,
            commit_count,
        });

        if old_oid != new_oid {
            let full_ref = format!("refs/heads/{}", plan.branch);
            let full_name = FullName::try_from(full_ref.as_str())
                .map_err(|err| OzError::Git(format!("invalid ref '{full_ref}': {err}")))?;
            ref_edits.push(RefEdit::update(
                full_name,
                new_oid,
                PreviousValue::MustExistAndMatch(Target::Object(old_oid)),
                format!("oz stack rebase onto {}", state.onto_branch),
            ));
            changed_trees.insert(plan.branch.clone(), (old_tree, new_tree, new_oid));
        }
    }

    if ref_edits.is_empty() {
        let _ = cleanup_session_and_sandbox(main_repo, None);
        return Ok(StackRebaseOutcome {
            onto_branch: state.onto_branch.clone(),
            onto_oid: state.onto_commit_oid.clone(),
            branches: branch_outcomes,
            rebased_commits: state.rebased_commits.clone(),
            op_id: None,
            updated_workspaces: Vec::new(),
        });
    }

    // Verify checked-out main worktree and checked-out Ozone workspaces before mutating any refs.
    let main_branch = match gix_repo.head() {
        Ok(h) => h
            .referent_name()
            .map(|n| String::from_utf8_lossy(n.shorten().as_ref()).into_owned()),
        Err(_) => None,
    };
    let main_checked_out_delta = if let Some(ref mb) = main_branch
        && let Some((old_t, new_t, _)) = changed_trees.get(mb)
    {
        let delta = compute_tree_delta(gix_repo, *old_t, *new_t)?;
        let st = compute_status(main_repo)?;
        let conflicts = conflicting_dirty_paths(&st, &delta);
        if !conflicts.is_empty() {
            return Err(OzError::Conflict {
                message: format!(
                    "main working tree on '{}' has conflicting uncommitted changes in: {}",
                    mb,
                    conflicts.join(", ")
                ),
                conflicts: Vec::new(),
                recovery: vec![
                    "oz commit -m \"save work\"".to_string(),
                    "oz checkpoint".to_string(),
                ],
            });
        }
        Some((st, *old_t, *new_t, delta))
    } else {
        None
    };

    let summaries = list_workspaces(main_repo)?;
    let mut ws_updates = Vec::new();
    let mut caller_ws_dirty = None;
    for summary in summaries {
        let ws = summary.state;
        if let Some((old_t, new_t, new_oid)) = changed_trees.get(&ws.branch).copied() {
            let delta = compute_tree_delta(gix_repo, old_t, new_t)?;
            if (ws.worktree_path.is_dir() || ws.backend.requires_daemon())
                && let Ok(ws_repo) = OzoneRepo::discover(Some(&ws.worktree_path))
                && let Ok(st) = compute_status(&ws_repo)
            {
                let conflicts = conflicting_dirty_paths(&st, &delta);
                if !conflicts.is_empty() {
                    return Err(OzError::Conflict {
                        message: format!(
                            "workspace '{}' on '{}' has conflicting uncommitted changes in: {}",
                            ws.name,
                            ws.branch,
                            conflicts.join(", ")
                        ),
                        conflicts: Vec::new(),
                        recovery: vec![format!(
                            "oz -C {} commit -m \"save work\"",
                            ws.worktree_path.display()
                        )],
                    });
                }
                if caller_repo.workspace.as_deref() == Some(ws.name.as_str()) {
                    caller_ws_dirty = Some(st.changes);
                }
            }
            ws_updates.push((ws, summary.mount_health, old_t, new_t, new_oid, delta));
        }
    }

    let use_caller_ws_tx = main_checked_out_delta.is_none()
        && caller_repo.workspace.is_some()
        && caller_repo
            .branch
            .as_deref()
            .is_some_and(|b| changed_trees.contains_key(b));
    let tx_repo = if use_caller_ws_tx {
        caller_repo
    } else {
        main_repo
    };
    let tx_dirty_override = if use_caller_ws_tx {
        caller_ws_dirty
    } else {
        main_checked_out_delta
            .as_ref()
            .map(|(st, _, _, _)| st.changes.clone())
    };
    let branch_names: Vec<&str> = state.branches.iter().map(|b| b.branch.as_str()).collect();
    let cmd_str = format!(
        "oz stack rebase {} --onto {}",
        branch_names.join(" "),
        state.onto_branch
    );
    let tx = MutationTransaction::begin_watching_all_branches(
        tx_repo,
        cmd_str,
        actor,
        tx_dirty_override.as_deref(),
    )?;

    // Single atomic CAS transaction across all stacked `refs/heads/*`.
    gix_repo.edit_references(ref_edits).map_err(|err| OzError::Conflict {
        message: format!("atomic CAS ref transaction failed during stack rebase: {err}"),
        conflicts: Vec::new(),
        recovery: vec!["Retry `oz stack rebase` after resolving concurrent branch updates.".to_string()],
    })?;

    if let Some((_, old_t, new_t, ref delta)) = main_checked_out_delta {
        apply_tree_delta_with_cleanup(gix_repo, old_t, new_t, &main_repo.repo_root)?;
        sync_git_index_for_delta(
            &main_repo.repo_root,
            &main_repo.repo_root,
            old_t,
            new_t,
            delta,
        )?;
    }

    let mut updated_workspaces = Vec::with_capacity(ws_updates.len());
    for (mut ws, mount_health, old_t, new_t, new_oid, delta) in ws_updates {
        if ws.worktree_path.is_dir()
            && (!ws.backend.requires_daemon() || mount_health == MountHealth::Healthy)
        {
            apply_tree_delta_with_cleanup(gix_repo, old_t, new_t, &ws.worktree_path)?;
        }
        if let Some(ref wt_git_dir) = ws.git_worktree_dir
            && wt_git_dir.is_dir()
        {
            sync_git_index_for_delta(wt_git_dir, &ws.worktree_path, old_t, new_t, &delta)?;
        }
        ws.base_commit = Some(new_oid.to_hex().to_string());
        ws.base_tree = new_t.to_hex().to_string();
        ws.updated_at = Utc::now();
        write_workspace_state_atomic(&main_repo.layout.workspace_state_path(&ws.name), &ws)?;
        updated_workspaces.push(ws.name);
    }
    updated_workspaces.sort();

    let _ = cleanup_session_and_sandbox(main_repo, None);
    let op_entry = tx.finish(tx_repo, tx_dirty_override.as_deref())?;

    Ok(StackRebaseOutcome {
        onto_branch: state.onto_branch.clone(),
        onto_oid: state.onto_commit_oid.clone(),
        branches: branch_outcomes,
        rebased_commits: state.rebased_commits.clone(),
        op_id: Some(op_entry.op_id),
        updated_workspaces,
    })
}

fn build_rebased_commit_from_original(
    gix_repo: &gix::Repository,
    actor: &ActorInfo,
    orig_commit_oid: ObjectId,
    new_parent_oid: ObjectId,
    new_tree_id: ObjectId,
) -> Result<(ObjectId, String)> {
    let orig_commit = gix_repo
        .find_commit(orig_commit_oid)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let orig_author = orig_commit
        .author()
        .map_err(|err| OzError::Git(err.to_string()))?
        .to_owned()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let orig_msg = orig_commit
        .message_raw_sloppy()
        .to_str_lossy()
        .into_owned();

    let (author, committer) = resolve_signatures(gix_repo, actor, Some(orig_author))?;
    let parents = [new_parent_oid];
    let (final_msg, change_id) =
        preserve_or_add_change_id(&orig_msg, new_tree_id, &parents, &author);
    let new_oid =
        write_commit_in_odb(gix_repo, &author, &committer, &final_msg, new_tree_id, &parents)?;
    Ok((new_oid, change_id))
}

fn map_workspaces_by_branch(main_repo: &OzoneRepo) -> Result<HashMap<String, WorkspaceState>> {
    let mut map = HashMap::new();
    for s in list_workspaces(main_repo)? {
        map.insert(s.state.branch.clone(), s.state);
    }
    Ok(map)
}

fn resolve_stack_branches_to_rebase(
    repo: &OzoneRepo,
    main_repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    onto_branch: &str,
    opts: &StackRebaseOptions,
) -> Result<Vec<(String, ObjectId)>> {
    if !opts.branches.is_empty() {
        let mut seen = BTreeSet::new();
        let mut out = Vec::with_capacity(opts.branches.len());
        for raw in &opts.branches {
            let short = normalize_branch_short(raw).to_string();
            if short == onto_branch {
                return Err(OzError::InvalidUsage {
                    message: format!("cannot rebase base branch '{onto_branch}' onto itself"),
                    recovery: Vec::new(),
                });
            }
            if !seen.insert(short.clone()) {
                continue;
            }
            let Some((_, oid)) = resolve_local_branch(gix_repo, &short)? else {
                return Err(OzError::InvalidUsage {
                    message: format!("branch '{short}' does not exist"),
                    recovery: vec!["Check available branches with `oz stack status`.".to_string()],
                });
            };
            out.push((short, oid));
        }
        return Ok(out);
    }

    let status = stack_status(main_repo, Some(onto_branch))?;
    if status.branches.is_empty() {
        return Ok(Vec::new());
    }

    if let Some(ref cur_branch) = repo.branch
        && cur_branch != onto_branch
        && status.branches.iter().any(|b| &b.branch == cur_branch)
    {
        // Include `cur_branch`, its ancestor stack branches, and any branches stacked atop it.
        let mut selected = BTreeSet::new();
        selected.insert(cur_branch.clone());

        let by_name: HashMap<&str, &StackBranchStatus> = status
            .branches
            .iter()
            .map(|b| (b.branch.as_str(), b))
            .collect();
        let mut cur = cur_branch.as_str();
        while let Some(info) = by_name.get(cur) {
            if let Some(ref p) = info.parent_branch {
                if !selected.insert(p.clone()) {
                    break;
                }
                cur = p.as_str();
            } else {
                break;
            }
        }
        loop {
            let mut added = false;
            for b in &status.branches {
                if !selected.contains(&b.branch)
                    && b.parent_branch
                        .as_deref()
                        .is_some_and(|p| selected.contains(p))
                {
                    selected.insert(b.branch.clone());
                    added = true;
                }
            }
            if !added {
                break;
            }
        }

        let mut out = Vec::new();
        for b in status.branches {
            if selected.contains(&b.branch) {
                out.push((b.branch, parse_oid(&b.head_oid)?));
            }
        }
        return Ok(out);
    }

    let mut out = Vec::with_capacity(status.branches.len());
    for b in status.branches {
        out.push((b.branch, parse_oid(&b.head_oid)?));
    }
    Ok(out)
}

/// Topologically orders `(branch, tip_oid)` pairs from base to tip so any branch `A` whose
/// tip is an ancestor of `B`'s tip appears before `B`.
fn sort_branches_topologically(
    gix_repo: &gix::Repository,
    branches: &[(String, ObjectId)],
) -> Result<Vec<(String, ObjectId)>> {
    let n = branches.len();
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in 0..n {
        for j in 0..n {
            if i == j {
                continue;
            }
            if branches[i].1 != branches[j].1 && is_ancestor(gix_repo, branches[i].1, branches[j].1)?
            {
                deps[j].push(i);
            }
        }
    }

    let mut placed = vec![false; n];
    let mut ordered = Vec::with_capacity(n);
    while ordered.len() < n {
        let mut progress = false;
        for i in 0..n {
            if placed[i] {
                continue;
            }
            if deps[i].iter().all(|&d| placed[d]) {
                placed[i] = true;
                ordered.push(branches[i].clone());
                progress = true;
            }
        }
        if !progress {
            for (i, item) in branches.iter().enumerate() {
                if !placed[i] {
                    ordered.push(item.clone());
                }
            }
            break;
        }
    }
    Ok(ordered)
}

fn find_parent_branch_in_list(
    gix_repo: &gix::Repository,
    tip_oid: ObjectId,
    earlier_branches: &[(String, ObjectId)],
) -> Result<Option<String>> {
    // Walk `earlier_branches` in reverse (closest topological ancestors first).
    for (name, cand_oid) in earlier_branches.iter().rev() {
        if *cand_oid != tip_oid && is_ancestor(gix_repo, *cand_oid, tip_oid)? {
            return Ok(Some(name.clone()));
        }
    }
    Ok(None)
}

fn collect_commit_range(
    gix_repo: &gix::Repository,
    tip_oid: ObjectId,
    stop_exclusive: Option<ObjectId>,
) -> Result<Vec<ObjectId>> {
    if Some(tip_oid) == stop_exclusive {
        return Ok(Vec::new());
    }

    let mut commits = Vec::new();
    let mut cursor = Some(tip_oid);
    let mut visited = BTreeSet::new();

    while let Some(cid) = cursor {
        if Some(cid) == stop_exclusive || !visited.insert(cid) {
            break;
        }
        let commit = gix_repo
            .find_commit(cid)
            .map_err(|err| OzError::Git(err.to_string()))?;
        commits.push(cid);
        cursor = commit.parent_ids().next().map(|id| id.detach());
    }

    commits.reverse();
    Ok(commits)
}

fn count_first_parent_commits(
    gix_repo: &gix::Repository,
    tip_oid: ObjectId,
    stop_exclusive: Option<ObjectId>,
) -> Result<usize> {
    Ok(collect_commit_range(gix_repo, tip_oid, stop_exclusive)?.len())
}

fn summarize_commit(gix_repo: &gix::Repository, oid: ObjectId) -> Result<StackCommitSummary> {
    let commit = gix_repo
        .find_commit(oid)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let raw = commit.message_raw_sloppy().to_str_lossy().into_owned();
    let subject = raw.lines().next().unwrap_or("").trim().to_string();
    let change_id = extract_change_id_from_msg(&raw);
    Ok(StackCommitSummary {
        oid: oid.to_hex().to_string(),
        subject,
        change_id,
    })
}

fn extract_change_id_from_msg(msg: &str) -> Option<String> {
    for line in msg.trim_end().lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':')
            && k.eq_ignore_ascii_case("Change-Id")
        {
            let val = v.trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}
