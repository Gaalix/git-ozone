// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `oz sync` workflow: fetch remote, fast-forward `main`, detect merged branches (via GitHub PRs
//! and offline ancestor/`Change-Id:` fallback), auto-retarget downstream stacked PR bases on GitHub,
//! rebase local downstream stacked branches via `oz-integrate`, GC merged CoW workspaces &
//! checkpoints, and refresh the quiescent `oz-warm` baseline.

use crate::api::{GitHubClient, GitHubPullRequest, UpdatePullRequest};
use crate::auth::{parse_git_remote, read_remote_url_from_gix};
use crate::pr::{
    OZ_STACK_START, StackPrItem, extract_change_ids_from_text, format_pr_status,
    normalize_short_branch, render_stack_navigation_block, upsert_stack_navigation_block,
};
use crate::wire::{
    FetchOptions, HttpAuthScheme, PushOptions, fetch_remote, is_ancestor_commit,
    open_gix_unrestricted, push_refs, read_ref_oid,
};
use chrono::Utc;
use gix::bstr::ByteSlice;
use gix::hash::ObjectId;
use gix::merge::tree::TreatAsUnresolved;
use gix::objs::Commit;
use gix::refs::FullName;
use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};
use oz_core::{MountHealth, OzError, OzoneRepo, Result, WorkspaceState, compute_status};
use oz_cow::materialize::{apply_tree_delta_to_dir, compute_tree_delta};
use oz_cow::{
    GcWorkspacesOptions, RemoveWorkspaceOptions, gc_workspaces, list_workspaces, remove_workspace,
};
use oz_integrate::{StackRebaseOptions, stack_rebase, stack_status};
use oz_oplog::{ActorInfo, MutationTransaction, list_entries};
use oz_warm::{BaselineSource, gc_baselines, resolve_or_capture_baseline};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufWriter, Write};
use std::path::Path;

/// Options controlling [`sync_repo`].
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    /// Remote name (defaults to `"origin"`).
    pub remote: Option<String>,
    /// Base trunk branch (defaults to `"main"`).
    pub base_branch: Option<String>,
    /// Skip network `fetch_remote` step (`--no-fetch`).
    pub no_fetch: bool,
    /// Skip rebasing downstream local stacked branches (`--no-rebase`).
    pub no_rebase: bool,
    /// Optionally push rebased local branches to remote (`--push`).
    pub push_rebased: bool,
    /// Explicit GitHub token override.
    pub auth_token: Option<String>,
    /// Explicit GitHub API base URL override (also honors `OZ_GITHUB_API_URL`).
    pub api_url: Option<String>,
    /// HTTP auth scheme for Git wire fetch/push.
    pub git_auth_scheme: HttpAuthScheme,
}

/// Summary of a downstream GitHub PR whose `base` branch was automatically retargeted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetargetedPrSummary {
    pub number: u64,
    pub head_branch: String,
    pub old_base: String,
    pub new_base: String,
}

/// Complete report returned by [`sync_repo`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOutcome {
    pub remote: String,
    pub base_branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_base_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_oid: Option<String>,
    pub fetched: bool,
    pub fast_forwarded: bool,
    pub merged_branches: Vec<String>,
    pub retargeted_prs: Vec<RetargetedPrSummary>,
    pub rebased_branches: Vec<String>,
    pub removed_workspaces: Vec<String>,
    pub deleted_branches: Vec<String>,
    pub pruned_checkpoints: Vec<String>,
    pub baseline_refreshed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Synchronizes the repository with `<remote>` (default `"origin"`):
/// 1. Fetches `<remote>` with `--prune`
/// 2. Fast-forwards local `base_branch` (default `"main"`) when safe
/// 3. Detects merged branches via GitHub PR API (gracefully degrading to local ancestor / `Change-Id:` detection)
/// 4. Auto-retargets open downstream stacked PR bases on GitHub
/// 5. Rebases local downstream stacked branches onto updated `main` via `oz-integrate`
/// 6. Removes & GCs merged CoW workspaces (`oz-cow`) and prunes merged checkpoints (`refs/ozone/*checkpoints/*`)
/// 7. Refreshes the quiescent `oz-warm` baseline
pub fn sync_repo(
    repo: &OzoneRepo,
    opts: &SyncOptions,
    actor: ActorInfo,
) -> Result<SyncOutcome> {
    let main_repo = if repo.is_linked_worktree || repo.workspace.is_some() {
        OzoneRepo::discover(Some(&repo.repo_root))?
    } else {
        repo.clone()
    };
    main_repo.layout.ensure_initialized()?;

    let remote_name = opts
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("origin")
        .to_string();
    let base_branch = opts
        .base_branch
        .as_deref()
        .map(normalize_short_branch)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string());

    let mut warnings = Vec::new();
    let mut last_op_id: Option<String> = None;

    let gix_initial = open_gix_unrestricted(&main_repo.repo_root)?;
    let previous_base_oid = read_ref_oid(&gix_initial, &format!("refs/heads/{base_branch}"))?;
    let remote_url_opt = read_remote_url_from_gix(&gix_initial, &remote_name, false)?;

    // Capture pre-sync branch OIDs & branches that have recorded commits/rebases in OpLog.
    let pre_branches = list_local_branch_oids(&gix_initial)?;
    let pre_workspaces = list_workspaces(&main_repo)?;
    let has_workspace_set: HashSet<String> = pre_workspaces
        .iter()
        .map(|w| w.state.branch.clone())
        .collect();
    let branches_with_commits = collect_branches_with_recorded_commits(&main_repo);

    // 1. Fetch `origin`
    let mut fetched = false;
    if !opts.no_fetch {
        if remote_url_opt.is_some() {
            let fetch_out = fetch_remote(
                &main_repo,
                &FetchOptions {
                    remote: Some(remote_name.clone()),
                    refspecs: Vec::new(),
                    prune: true,
                    filter: None,
                    tags: true,
                    auth_token: opts.auth_token.clone(),
                    auth_scheme: opts.git_auth_scheme,
                },
                actor.clone(),
            )?;
            fetched = true;
            warnings.extend(fetch_out.warnings);
            if fetch_out.op_id.is_some() {
                last_op_id = fetch_out.op_id;
            }
        } else {
            warnings.push(format!(
                "remote '{remote_name}' is not configured; running local-only sync"
            ));
        }
    }

    // 2. Fast-forward local `base_branch` (`main`) when safe!
    let gix_after_fetch = open_gix_unrestricted(&main_repo.repo_root)?;
    let tracking_base_ref = format!("refs/remotes/{remote_name}/{base_branch}");
    let local_base_ref = format!("refs/heads/{base_branch}");
    let remote_base_oid = read_ref_oid(&gix_after_fetch, &tracking_base_ref)?;
    let mut current_base_oid = read_ref_oid(&gix_after_fetch, &local_base_ref)?;
    let mut fast_forwarded = false;

    if let Some(target_oid) = remote_base_oid
        && current_base_oid != Some(target_oid)
    {
        let can_ff = match current_base_oid {
            None => true,
            Some(old_oid) => is_ancestor_commit(&gix_after_fetch, old_oid, target_oid)?,
        };

        if can_ff {
            let refreshed_main = OzoneRepo::discover(Some(&main_repo.repo_root))?;
            match fast_forward_local_base(
                &refreshed_main,
                &gix_after_fetch,
                &base_branch,
                current_base_oid,
                target_oid,
                actor.clone(),
            )? {
                FastForwardResult::Updated { op_id } => {
                    fast_forwarded = true;
                    current_base_oid = Some(target_oid);
                    last_op_id = Some(op_id);
                }
                FastForwardResult::SkippedDirty { conflicting_paths } => {
                    warnings.push(format!(
                        "skipped fast-forwarding '{base_branch}' due to uncommitted changes in: {}",
                        conflicting_paths.join(", ")
                    ));
                }
            }
        } else {
            warnings.push(format!(
                "local '{base_branch}' has diverged from '{remote_name}/{base_branch}'; skipped fast-forward"
            ));
        }
    }

    let gix_repo = open_gix_unrestricted(&main_repo.repo_root)?;

    // 3. Query GitHub PR states (when token + GitHub remote available; gracefully degrade offline!).
    let mut github_prs: Option<(GitHubClient, String, String, Vec<GitHubPullRequest>)> = None;
    let has_explicit_api_override =
        opts.api_url.is_some() || std::env::var_os("OZ_GITHUB_API_URL").is_some();
    if let Some(ref raw_url) = remote_url_opt
        && let Ok(parsed_remote) = parse_git_remote(raw_url, opts.api_url.as_deref())
        && (parsed_remote.scheme != crate::auth::RemoteScheme::File || has_explicit_api_override)
        && let (Some(owner), Some(gh_repo)) =
            (parsed_remote.owner.clone(), parsed_remote.repo.clone())
        && let Ok((client, _)) = GitHubClient::for_repo(
            &main_repo,
            Some(&remote_name),
            opts.auth_token.as_deref(),
            opts.api_url.as_deref(),
        )
        && (client.has_token() || has_explicit_api_override)
    {
        match client.list_pull_requests(&owner, &gh_repo, Some("all"), None, None) {
            Ok(prs) => {
                github_prs = Some((client, owner, gh_repo, prs));
            }
            Err(err) => {
                warnings.push(format!(
                    "GitHub PR query unavailable ({err}); falling back to local merge detection"
                ));
            }
        }
    }

    // Collect merged branches from GitHub PRs + local ancestor / Change-Id detection.
    let mut merged_set: BTreeSet<String> = BTreeSet::new();
    let mut merged_pr_bases: HashMap<String, String> = HashMap::new();
    let mut merged_change_ids: HashSet<String> = HashSet::new();

    if let Some((_, _, _, ref prs)) = github_prs {
        for pr in prs {
            if pr.is_merged() && pr.head.ref_name != base_branch {
                merged_set.insert(pr.head.ref_name.clone());
                merged_pr_bases.insert(pr.head.ref_name.clone(), pr.base.ref_name.clone());
                if let Some(ref body) = pr.body {
                    for cid in extract_change_ids_from_text(body) {
                        merged_change_ids.insert(cid);
                    }
                }
            }
        }
    }

    if let Some(base_id) = current_base_oid {
        for cid in collect_recent_change_ids(&gix_repo, base_id, previous_base_oid, 256)? {
            merged_change_ids.insert(cid);
        }

        for (branch, &br_oid) in &pre_branches {
            if branch == &base_branch || merged_set.contains(branch) {
                continue;
            }
            let was_ahead_before_ff = previous_base_oid.is_some_and(|prev_id| {
                !is_ancestor_commit(&gix_repo, br_oid, prev_id).unwrap_or(false)
            });
            let had_work = was_ahead_before_ff
                || branches_with_commits.contains(branch)
                || (!has_workspace_set.contains(branch) && br_oid != base_id);

            if had_work && is_ancestor_commit(&gix_repo, br_oid, base_id)? {
                merged_set.insert(branch.clone());
                continue;
            }

            // Offline squash/rebase merge detection via `Change-Id:` trailers!
            if !merged_change_ids.is_empty()
                && branch_commits_all_merged_by_change_id(
                    &gix_repo,
                    br_oid,
                    base_id,
                    &merged_change_ids,
                )?
            {
                merged_set.insert(branch.clone());
            }
        }
    }

    // 4. Auto-retarget downstream open PR bases on GitHub when an upstream stack PR merges!
    let mut retargeted_prs = Vec::new();
    if let Some((ref client, ref owner, ref gh_repo, ref mut prs)) = github_prs
        && !merged_set.is_empty()
    {
        for pr_item in prs.iter_mut() {
            if pr_item.state != "open" || pr_item.is_merged() {
                continue;
            }
            let old_base = pr_item.base.ref_name.clone();
            if !merged_set.contains(&old_base) {
                continue;
            }

            let mut resolved_base = old_base.clone();
            let mut hops = 0usize;
            while merged_set.contains(&resolved_base) && hops < 32 {
                hops += 1;
                resolved_base = merged_pr_bases
                    .get(&resolved_base)
                    .cloned()
                    .unwrap_or_else(|| base_branch.clone());
            }
            if merged_set.contains(&resolved_base) {
                resolved_base = base_branch.clone();
            }

            if resolved_base != old_base && resolved_base != pr_item.head.ref_name {
                let updated_pr = client.retarget_pull_request_base(
                    owner,
                    gh_repo,
                    pr_item.number,
                    &resolved_base,
                )?;
                retargeted_prs.push(RetargetedPrSummary {
                    number: updated_pr.number,
                    head_branch: updated_pr.head.ref_name.clone(),
                    old_base,
                    new_base: resolved_base,
                });
                *pr_item = updated_pr;
            }
        }

        // Refresh `<!-- oz-stack-start -->` blocks on remaining open stack PRs if present.
        refresh_open_stack_blocks(client, owner, gh_repo, &base_branch, prs, &mut warnings);
    }

    // 5. Rebase downstream local unmerged stacked branches onto updated `base_branch` via `oz-integrate`!
    let mut rebased_branches = Vec::new();
    if !opts.no_rebase && current_base_oid.is_some() {
        let refreshed_main = OzoneRepo::discover(Some(&main_repo.repo_root))?;

        // First, if any merged branch was squash-merged (so its original tip `M_oid` is NOT a git ancestor
        // of `current_base_oid`), transplant direct unmerged child branches `(M_oid..child_oid]` onto
        // `current_base_oid` so `stack_rebase` never re-applies already-squash-merged commits!
        if let Some(new_base_id) = current_base_oid {
            transplant_children_of_squash_merged_branches(
                &refreshed_main,
                &base_branch,
                new_base_id,
                &pre_branches,
                &merged_set,
                &actor,
                &mut rebased_branches,
                &mut last_op_id,
                &mut warnings,
            )?;
        }

        let refreshed_main = OzoneRepo::discover(Some(&main_repo.repo_root))?;
        let st_report = stack_status(&refreshed_main, Some(&base_branch))?;
        let candidates: Vec<String> = st_report
            .branches
            .iter()
            .filter(|b| !merged_set.contains(&b.branch) && b.needs_rebase)
            .map(|b| b.branch.clone())
            .collect();

        if !candidates.is_empty() {
            match stack_rebase(
                &refreshed_main,
                &StackRebaseOptions {
                    branches: candidates,
                    onto: Some(base_branch.clone()),
                },
                actor.clone(),
            ) {
                Ok(rb_out) => {
                    for br in rb_out.branches {
                        if br.previous_oid != br.new_oid && !rebased_branches.contains(&br.branch) {
                            rebased_branches.push(br.branch);
                        }
                    }
                    if rb_out.op_id.is_some() {
                        last_op_id = rb_out.op_id;
                    }
                }
                Err(err) => {
                    return Err(err);
                }
            }
        }

        if opts.push_rebased && !rebased_branches.is_empty() && remote_url_opt.is_some() {
            let refreshed_main = OzoneRepo::discover(Some(&main_repo.repo_root))?;
            let push_out = push_refs(
                &refreshed_main,
                &PushOptions {
                    remote: Some(remote_name.clone()),
                    refspecs: rebased_branches.clone(),
                    atomic: true,
                    force: false,
                    force_with_lease: true,
                    lease_specs: Vec::new(),
                    delete: false,
                    set_upstream: false,
                    auth_token: opts.auth_token.clone(),
                    auth_scheme: opts.git_auth_scheme,
                },
                actor.clone(),
            )?;
            if push_out.op_id.is_some() {
                last_op_id = push_out.op_id;
            }
        }
    }

    // 6. Remove & GC merged CoW workspaces (`oz-cow`), delete merged local branches, and prune merged checkpoints!
    let refreshed_main = OzoneRepo::discover(Some(&main_repo.repo_root))?;
    let mut removed_workspaces = Vec::new();
    let mut deleted_branches = Vec::new();

    let gc_report = gc_workspaces(
        &refreshed_main,
        &GcWorkspacesOptions {
            target_branch: Some(base_branch.clone()),
            prune_merged: true,
            prune_missing: true,
            dry_run: false,
        },
    )?;
    for ws_name in gc_report.removed {
        if !removed_workspaces.contains(&ws_name) {
            removed_workspaces.push(ws_name);
        }
    }

    // Also remove workspaces whose branches were detected as merged via GitHub PR / Change-Id (e.g. squash merges).
    let remaining_ws = list_workspaces(&refreshed_main)?;
    for summary in remaining_ws {
        if !merged_set.contains(&summary.state.branch) {
            continue;
        }
        let ws_name = summary.state.name.clone();
        let is_dirty = !summary.state.dirty_paths.is_empty()
            || (summary.healthy
                && OzoneRepo::discover(Some(&summary.state.worktree_path))
                    .ok()
                    .and_then(|wr| compute_status(&wr).ok())
                    .is_some_and(|st| !st.changes.is_empty()));

        if is_dirty {
            warnings.push(format!(
                "skipped removing merged workspace '{ws_name}' because it has uncommitted changes"
            ));
            continue;
        }

        let rem = remove_workspace(
            &refreshed_main,
            &ws_name,
            &RemoveWorkspaceOptions {
                force: false,
                delete_branch: true,
            },
        )?;
        if rem.branch_deleted
            && let Some(br) = rem.branch
            && !deleted_branches.contains(&br)
        {
            deleted_branches.push(br);
        }
        if !removed_workspaces.contains(&ws_name) {
            removed_workspaces.push(ws_name);
        }
    }

    // Delete any remaining merged local branches not checked out in any worktree.
    let gix_cleanup = open_gix_unrestricted(&refreshed_main.repo_root)?;
    let checked_out_refs = collect_checked_out_branch_refs(&gix_cleanup, &refreshed_main.git_common_dir)?;
    for merged_br in &merged_set {
        let full_ref = format!("refs/heads/{merged_br}");
        if checked_out_refs.contains(&full_ref) {
            continue;
        }
        if let Ok(Some(reference)) = gix_cleanup.try_find_reference(&full_ref)
            && reference.delete().is_ok()
            && !deleted_branches.contains(merged_br)
        {
            deleted_branches.push(merged_br.clone());
        }
    }

    // Prune `refs/ozone/checkpoints/<ws>` and `refs/ozone/op-checkpoints/<ws>/*` for removed workspaces.
    let pruned_checkpoints = prune_workspace_checkpoints(&gix_cleanup, &removed_workspaces)?;

    // 7. Refresh quiescent `oz-warm` baseline!
    let mut baseline_refreshed = false;
    if let Some(base_id) = current_base_oid {
        let base_hex = base_id.to_hex().to_string();
        if let Some(res) = resolve_or_capture_baseline(
            &refreshed_main.repo_root,
            &refreshed_main.layout.ozone_dir,
            &base_hex,
            &refreshed_main.config,
        )? {
            baseline_refreshed = matches!(
                res.source,
                BaselineSource::Captured | BaselineSource::Existing
            );
            let _ = gc_baselines(
                &refreshed_main.layout.ozone_dir,
                refreshed_main.config.max_baseline_gb,
            );
        }
    }

    Ok(SyncOutcome {
        remote: remote_name,
        base_branch,
        previous_base_oid: previous_base_oid.map(|id| id.to_hex().to_string()),
        base_oid: current_base_oid.map(|id| id.to_hex().to_string()),
        fetched,
        fast_forwarded,
        merged_branches: merged_set.into_iter().collect(),
        retargeted_prs,
        rebased_branches,
        removed_workspaces,
        deleted_branches,
        pruned_checkpoints,
        baseline_refreshed,
        op_id: last_op_id,
        warnings,
    })
}

enum FastForwardResult {
    Updated { op_id: String },
    SkippedDirty { conflicting_paths: Vec<String> },
}

fn fast_forward_local_base(
    main_repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    base_branch: &str,
    old_oid: Option<ObjectId>,
    new_oid: ObjectId,
    actor: ActorInfo,
) -> Result<FastForwardResult> {
    let old_tree = match old_oid {
        Some(id) => gix_repo
            .find_commit(id)
            .map_err(|err| OzError::Git(err.to_string()))?
            .tree_id()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach(),
        None => gix_repo.empty_tree().id().detach(),
    };
    let new_tree = gix_repo
        .find_commit(new_oid)
        .map_err(|err| OzError::Git(err.to_string()))?
        .tree_id()
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();

    let delta = compute_tree_delta(gix_repo, old_tree, new_tree)?;
    let main_checked_out_on_base = main_repo.branch.as_deref() == Some(base_branch);

    let mut pre_dirty = None;
    if main_checked_out_on_base && (!delta.upserts.is_empty() || !delta.deletions.is_empty()) {
        let status = compute_status(main_repo)?;
        let mut conflicts = Vec::new();
        for ch in &status.changes {
            let collides = delta.deletions.iter().any(|d| paths_overlap(&ch.path, d))
                || delta
                    .upserts
                    .iter()
                    .any(|(u, _, _)| paths_overlap(&ch.path, u));
            if collides {
                conflicts.push(ch.path.clone());
            }
        }
        if !conflicts.is_empty() {
            return Ok(FastForwardResult::SkippedDirty {
                conflicting_paths: conflicts,
            });
        }
        pre_dirty = Some(status.changes);
    }

    let full_ref = format!("refs/heads/{base_branch}");
    let tx = MutationTransaction::begin_with_refs(
        main_repo,
        format!("oz sync (fast-forward {base_branch})"),
        actor,
        &[full_ref.as_str()],
        pre_dirty.as_deref(),
    )?;

    let full_name = FullName::try_from(full_ref.as_str())
        .map_err(|err| OzError::Git(format!("invalid ref '{full_ref}': {err}")))?;
    let expected = match old_oid {
        Some(old_id) => PreviousValue::MustExistAndMatch(gix::refs::Target::Object(old_id)),
        None => PreviousValue::Any,
    };
    gix_repo
        .edit_reference(RefEdit::update(
            full_name,
            new_oid,
            expected,
            format!("oz sync: fast-forward {base_branch}"),
        ))
        .map_err(|err| OzError::Git(format!("failed fast-forwarding '{base_branch}': {err}")))?;

    if main_checked_out_on_base {
        apply_tree_delta_to_dir(gix_repo, &delta, &main_repo.repo_root)?;
        cleanup_empty_dirs_after_deletions(&main_repo.repo_root, &delta.deletions);
        write_repo_index(gix_repo, new_tree)?;
    }

    let entry = tx.finish(main_repo, pre_dirty.as_deref())?;
    Ok(FastForwardResult::Updated {
        op_id: entry.op_id,
    })
}

#[expect(clippy::too_many_arguments)]
fn transplant_children_of_squash_merged_branches(
    main_repo: &OzoneRepo,
    base_branch: &str,
    new_base_id: ObjectId,
    pre_branches: &BTreeMap<String, ObjectId>,
    merged_set: &BTreeSet<String>,
    actor: &ActorInfo,
    rebased_branches: &mut Vec<String>,
    last_op_id: &mut Option<String>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let gix_repo = open_gix_unrestricted(&main_repo.repo_root)?;

    // Collect merged branch tips that are NOT git ancestors of `new_base_id` (i.e. squash/rebase merges).
    let squash_merged_tips: Vec<(String, ObjectId)> = merged_set
        .iter()
        .filter_map(|br| pre_branches.get(br).map(|&oid| (br.clone(), oid)))
        .filter(|(_, oid)| !is_ancestor_commit(&gix_repo, *oid, new_base_id).unwrap_or(false))
        .collect();

    if squash_merged_tips.is_empty() {
        return Ok(());
    }

    // Map any unmerged branch whose history contains a squash-merged tip!
    // Process in topological order (fewest commits above merged tip first).
    let mut unmerged_targets: Vec<(String, ObjectId, ObjectId, Vec<ObjectId>)> = Vec::new();
    for (br, &br_oid) in pre_branches {
        if br == base_branch || merged_set.contains(br) {
            continue;
        }
        // Pick the closest (most recent) squash-merged ancestor of `br_oid`.
        let mut best_merged: Option<(ObjectId, Vec<ObjectId>)> = None;
        for (_, m_oid) in &squash_merged_tips {
            if *m_oid != br_oid
                && is_ancestor_commit(&gix_repo, *m_oid, br_oid)?
                && let Ok(range) = collect_commits_between(&gix_repo, br_oid, *m_oid)
            {
                let replace = match &best_merged {
                    None => true,
                    Some((_, prev_range)) => range.len() < prev_range.len(),
                };
                if replace {
                    best_merged = Some((*m_oid, range));
                }
            }
        }
        if let Some((m_oid, commits)) = best_merged
            && !commits.is_empty()
        {
            unmerged_targets.push((br.clone(), br_oid, m_oid, commits));
        }
    }

    if unmerged_targets.is_empty() {
        return Ok(());
    }

    unmerged_targets.sort_by_key(|(_, _, _, commits)| commits.len());

    // Keep track of rewritten parent tips `(old_tip_oid -> new_tip_oid)` so multi-level stacks
    // (`stack-1 [squash-merged] <- stack-2 <- stack-3`) chain onto each other seamlessly!
    let mut rewritten_tips: HashMap<ObjectId, ObjectId> = HashMap::new();
    for (_, m_oid) in &squash_merged_tips {
        rewritten_tips.insert(*m_oid, new_base_id);
    }

    let ws_summaries = list_workspaces(main_repo)?;
    let tx = MutationTransaction::begin_watching_all_branches(
        main_repo,
        format!("oz sync (rebase downstream stack onto {base_branch})"),
        actor.clone(),
        None,
    )?;

    let mut ref_edits = Vec::new();
    let mut ws_updates: Vec<(WorkspaceState, MountHealth, ObjectId, ObjectId, ObjectId)> =
        Vec::new();

    for (br_name, orig_tip_oid, merged_base_oid, commits_above_merged) in unmerged_targets {
        // Check if an intermediate unmerged parent in `commits_above_merged` was already rewritten!
        let mut start_idx = 0usize;
        let mut current_onto = *rewritten_tips
            .get(&merged_base_oid)
            .unwrap_or(&new_base_id);

        for (idx, cid) in commits_above_merged.iter().enumerate() {
            if let Some(&already_new) = rewritten_tips.get(cid) {
                current_onto = already_new;
                start_idx = idx + 1;
            }
        }

        let mut aborted_branch = false;
        for &orig_cid in &commits_above_merged[start_idx..] {
            let orig_commit = gix_repo
                .find_commit(orig_cid)
                .map_err(|err| OzError::Git(err.to_string()))?;
            let commit_tree = orig_commit
                .tree_id()
                .map_err(|err| OzError::Git(err.to_string()))?
                .detach();
            let parent_tree = match orig_commit.parent_ids().next() {
                Some(pid) => gix_repo
                    .find_commit(pid.detach())
                    .map_err(|err| OzError::Git(err.to_string()))?
                    .tree_id()
                    .map_err(|err| OzError::Git(err.to_string()))?
                    .detach(),
                None => gix_repo.empty_tree().id().detach(),
            };
            let onto_tree = gix_repo
                .find_commit(current_onto)
                .map_err(|err| OzError::Git(err.to_string()))?
                .tree_id()
                .map_err(|err| OzError::Git(err.to_string()))?
                .detach();

            let merge_opts = gix_repo
                .tree_merge_options()
                .map_err(|err| OzError::Git(err.to_string()))?;
            let labels = gix::merge::blob::builtin_driver::text::Labels {
                ancestor: Some("parent".into()),
                current: Some(base_branch.into()),
                other: Some(br_name.as_str().into()),
            };
            let mut merged = gix_repo
                .merge_trees(parent_tree, onto_tree, commit_tree, labels, merge_opts)
                .map_err(|err| OzError::Git(err.to_string()))?;

            if merged.has_unresolved_conflicts(TreatAsUnresolved::git()) {
                warnings.push(format!(
                    "conflict rebasing '{br_name}' onto '{base_branch}'; run `oz stack rebase {br_name} --onto {base_branch}`"
                ));
                aborted_branch = true;
                break;
            }

            let new_tree_id = merged
                .tree
                .write()
                .map_err(|err| OzError::Git(err.to_string()))?
                .detach();
            let new_cid = write_rebased_commit(&gix_repo, orig_cid, current_onto, new_tree_id)?;
            rewritten_tips.insert(orig_cid, new_cid);
            current_onto = new_cid;
        }

        if aborted_branch || current_onto == orig_tip_oid {
            continue;
        }

        rewritten_tips.insert(orig_tip_oid, current_onto);
        let orig_tree = gix_repo
            .find_commit(orig_tip_oid)
            .map_err(|err| OzError::Git(err.to_string()))?
            .tree_id()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach();
        let new_tree = gix_repo
            .find_commit(current_onto)
            .map_err(|err| OzError::Git(err.to_string()))?
            .tree_id()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach();

        let full_ref = format!("refs/heads/{br_name}");
        let full_name = FullName::try_from(full_ref.as_str())
            .map_err(|err| OzError::Git(err.to_string()))?;
        ref_edits.push(RefEdit::update(
            full_name,
            current_onto,
            PreviousValue::MustExistAndMatch(gix::refs::Target::Object(orig_tip_oid)),
            format!("oz sync: rebase {br_name} onto {base_branch}"),
        ));

        for ws_sum in &ws_summaries {
            if ws_sum.state.branch == br_name {
                ws_updates.push((
                    ws_sum.state.clone(),
                    ws_sum.mount_health,
                    orig_tree,
                    new_tree,
                    current_onto,
                ));
            }
        }
        rebased_branches.push(br_name);
    }

    if ref_edits.is_empty() {
        return Ok(());
    }

    gix_repo
        .edit_references(ref_edits)
        .map_err(|err| OzError::Git(format!("failed updating rebased branch refs: {err}")))?;

    for (mut ws, health, old_t, new_t, new_oid) in ws_updates {
        let delta = compute_tree_delta(&gix_repo, old_t, new_t)?;
        if ws.worktree_path.is_dir()
            && (!ws.backend.requires_daemon() || health == MountHealth::Healthy)
        {
            apply_tree_delta_to_dir(&gix_repo, &delta, &ws.worktree_path)?;
            cleanup_empty_dirs_after_deletions(&ws.worktree_path, &delta.deletions);
        }
        if let Some(ref wt_git_dir) = ws.git_worktree_dir
            && wt_git_dir.is_dir()
        {
            let index_path = wt_git_dir.join("index");
            if let Ok(idx_file) = gix_repo.index_from_tree(&new_t)
                && let Ok(f) = std::fs::File::create(&index_path)
            {
                let mut writer = BufWriter::new(f);
                let _ = idx_file.write_to(&mut writer, gix::index::write::Options::default());
            }
        }
        ws.base_commit = Some(new_oid.to_hex().to_string());
        ws.base_tree = new_t.to_hex().to_string();
        ws.updated_at = Utc::now();
        let state_path = main_repo.layout.workspace_state_path(&ws.name);
        let raw = serde_json::to_vec_pretty(&ws)?;
        std::fs::write(state_path, raw)?;
    }

    if let Some(entry) = tx.finish_if_changed(main_repo, None)? {
        *last_op_id = Some(entry.op_id);
    }

    Ok(())
}

fn write_rebased_commit(
    gix_repo: &gix::Repository,
    orig_commit_oid: ObjectId,
    new_parent_oid: ObjectId,
    new_tree_id: ObjectId,
) -> Result<ObjectId> {
    let orig = gix_repo
        .find_commit(orig_commit_oid)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let decoded = orig
        .decode()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let author = decoded
        .author()
        .map_err(|err| OzError::Git(err.to_string()))?
        .to_owned()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let committer = gix_repo
        .committer()
        .and_then(std::result::Result::ok)
        .and_then(|s| s.to_owned().ok())
        .unwrap_or_else(|| author.clone());
    let message = decoded.message.to_str_lossy().into_owned();

    let commit_obj = Commit {
        tree: new_tree_id,
        parents: [new_parent_oid].into_iter().collect(),
        author,
        committer,
        encoding: None,
        message: message.into(),
        extra_headers: Vec::new(),
    };
    gix_repo
        .write_object(&commit_obj)
        .map(|id| id.detach())
        .map_err(|err| OzError::Git(err.to_string()))
}

fn refresh_open_stack_blocks(
    client: &GitHubClient,
    owner: &str,
    gh_repo: &str,
    root_base: &str,
    prs: &[GitHubPullRequest],
    warnings: &mut Vec<String>,
) {
    let stack_prs: Vec<&GitHubPullRequest> = prs
        .iter()
        .filter(|p| {
            p.body
                .as_deref()
                .is_some_and(|b| b.contains(OZ_STACK_START))
        })
        .collect();
    if stack_prs.is_empty() {
        return;
    }

    let mut sorted = stack_prs;
    sorted.sort_by_key(|p| p.number);

    let items: Vec<StackPrItem> = sorted
        .iter()
        .map(|p| StackPrItem {
            number: p.number,
            html_url: p.html_url.clone(),
            title: p.title.clone(),
            head_branch: p.head.ref_name.clone(),
            base_branch: p.base.ref_name.clone(),
            status: format_pr_status(p),
            created: false,
            change_ids: Vec::new(),
        })
        .collect();

    for pr in sorted {
        if pr.state != "open" || pr.is_merged() {
            continue;
        }
        let Some(ref existing_body) = pr.body else {
            continue;
        };
        let new_block = render_stack_navigation_block(root_base, &items, pr.number);
        let new_body = upsert_stack_navigation_block(existing_body, &new_block);
        if &new_body != existing_body
            && let Err(err) = client.update_pull_request(
                owner,
                gh_repo,
                pr.number,
                &UpdatePullRequest {
                    body: Some(new_body),
                    ..Default::default()
                },
            )
        {
            warnings.push(format!(
                "failed refreshing stack table on PR #{}: {err}",
                pr.number
            ));
        }
    }
}

fn collect_branches_with_recorded_commits(main_repo: &OzoneRepo) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(entries) = list_entries(&main_repo.layout) else {
        return out;
    };
    for entry in entries {
        for (ref_name, delta) in &entry.ref_deltas {
            if let Some(short) = ref_name.strip_prefix("refs/heads/")
                && delta.old.is_some()
                && delta.new.is_some()
                && delta.old != delta.new
            {
                out.insert(short.to_string());
            }
        }
    }
    out
}

fn list_local_branch_oids(gix_repo: &gix::Repository) -> Result<BTreeMap<String, ObjectId>> {
    let mut out = BTreeMap::new();
    let platform = gix_repo
        .references()
        .map_err(|err| OzError::Git(err.to_string()))?;
    for item in platform
        .local_branches()
        .map_err(|err| OzError::Git(err.to_string()))?
    {
        let mut r = item.map_err(|err| OzError::Git(err.to_string()))?;
        let short = r
            .name()
            .as_bstr()
            .to_str_lossy()
            .strip_prefix("refs/heads/")
            .unwrap_or_default()
            .to_string();
        if !short.is_empty()
            && let Ok(id) = r.peel_to_id()
        {
            out.insert(short, id.detach());
        }
    }
    Ok(out)
}

fn collect_recent_change_ids(
    gix_repo: &gix::Repository,
    tip: ObjectId,
    stop_opt: Option<ObjectId>,
    limit: usize,
) -> Result<HashSet<String>> {
    let mut ids = HashSet::new();
    let mut cur = Some(tip);
    let mut count = 0usize;
    while let Some(cid) = cur {
        if Some(cid) == stop_opt || count >= limit {
            break;
        }
        count += 1;
        let Ok(commit) = gix_repo.find_commit(cid) else {
            break;
        };
        let msg = commit.message_raw_sloppy().to_str_lossy();
        for c in extract_change_ids_from_text(&msg) {
            ids.insert(c);
        }
        cur = commit.parent_ids().next().map(|id| id.detach());
    }
    Ok(ids)
}

fn branch_commits_all_merged_by_change_id(
    gix_repo: &gix::Repository,
    branch_tip: ObjectId,
    base_tip: ObjectId,
    merged_change_ids: &HashSet<String>,
) -> Result<bool> {
    let Some(mb) = gix_repo
        .merge_base(branch_tip, base_tip)
        .ok()
        .flatten()
        .map(|id| id.detach())
    else {
        return Ok(false);
    };
    let commits = collect_commits_between(gix_repo, branch_tip, mb)?;
    if commits.is_empty() {
        return Ok(false);
    }
    for cid in commits {
        let commit = gix_repo
            .find_commit(cid)
            .map_err(|err| OzError::Git(err.to_string()))?;
        let msg = commit.message_raw_sloppy().to_str_lossy();
        let cids = extract_change_ids_from_text(&msg);
        if cids.is_empty() || !cids.iter().any(|c| merged_change_ids.contains(c)) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn collect_commits_between(
    gix_repo: &gix::Repository,
    tip: ObjectId,
    stop_exclusive: ObjectId,
) -> Result<Vec<ObjectId>> {
    let mut out = Vec::new();
    let mut cur = Some(tip);
    let mut guard = 0usize;
    while let Some(cid) = cur {
        if cid == stop_exclusive {
            out.reverse();
            return Ok(out);
        }
        if guard >= 512 {
            break;
        }
        guard += 1;
        out.push(cid);
        let commit = gix_repo
            .find_commit(cid)
            .map_err(|err| OzError::Git(err.to_string()))?;
        cur = commit.parent_ids().next().map(|id| id.detach());
    }
    out.reverse();
    Ok(out)
}

fn collect_checked_out_branch_refs(
    gix_repo: &gix::Repository,
    git_common_dir: &Path,
) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    if let Ok(Some(name)) = gix_repo.head_name() {
        out.insert(String::from_utf8_lossy(name.as_bstr()).into_owned());
    }
    let wt_dir = git_common_dir.join("worktrees");
    if wt_dir.is_dir() {
        for entry in std::fs::read_dir(&wt_dir)? {
            let entry = entry?;
            let head_file = entry.path().join("HEAD");
            if let Ok(raw) = std::fs::read_to_string(&head_file)
                && let Some(target) = raw.trim().strip_prefix("ref:")
            {
                out.insert(target.trim().to_string());
            }
        }
    }
    Ok(out)
}

fn prune_workspace_checkpoints(
    gix_repo: &gix::Repository,
    removed_workspaces: &[String],
) -> Result<Vec<String>> {
    if removed_workspaces.is_empty() {
        return Ok(Vec::new());
    }

    let mut edits = Vec::new();
    let mut pruned = Vec::new();

    let platform = gix_repo
        .references()
        .map_err(|err| OzError::Git(err.to_string()))?;
    for ws in removed_workspaces {
        let direct_cp = format!("refs/ozone/checkpoints/{ws}");
        if gix_repo
            .try_find_reference(&direct_cp)
            .ok()
            .flatten()
            .is_some()
            && let Ok(full) = FullName::try_from(direct_cp.as_str())
        {
            edits.push(RefEdit {
                change: Change::Delete {
                    expected: PreviousValue::Any,
                    log: RefLog::AndReference,
                },
                name: full,
                deref: false,
            });
            pruned.push(direct_cp);
        }

        let op_prefix = format!("refs/ozone/op-checkpoints/{ws}/");
        if let Ok(prefixed) = platform.prefixed(op_prefix.as_str()) {
            for item in prefixed.flatten() {
                let name_str = item.name().as_bstr().to_str_lossy().into_owned();
                if let Ok(full) = FullName::try_from(name_str.as_str()) {
                    edits.push(RefEdit {
                        change: Change::Delete {
                            expected: PreviousValue::Any,
                            log: RefLog::AndReference,
                        },
                        name: full,
                        deref: false,
                    });
                    pruned.push(name_str);
                }
            }
        }
    }

    if !edits.is_empty() {
        let _ = gix_repo.edit_references(edits);
    }

    pruned.sort();
    Ok(pruned)
}

fn write_repo_index(gix_repo: &gix::Repository, tree_id: ObjectId) -> Result<()> {
    let index_path = gix_repo.index_path();
    let index_file = gix_repo
        .index_from_tree(&tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let mut out = BufWriter::new(std::fs::File::create(&index_path)?);
    index_file
        .write_to(&mut out, gix::index::write::Options::default())
        .map_err(|err| OzError::Git(err.to_string()))?;
    out.flush()?;
    Ok(())
}

fn cleanup_empty_dirs_after_deletions(root: &Path, deleted_rel_paths: &[String]) {
    for rel in deleted_rel_paths {
        let mut cur = root.join(rel);
        while let Some(parent) = cur.parent() {
            if parent == root || !parent.starts_with(root) {
                break;
            }
            if std::fs::remove_dir(parent).is_err() {
                break;
            }
            cur = parent.to_path_buf();
        }
    }
}

fn paths_overlap(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}
