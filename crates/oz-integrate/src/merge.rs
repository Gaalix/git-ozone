// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Atomic 100%-in-memory 3-way multi-source integration (`oz integrate`) with
//! ephemeral CoW `_integrate/` sandbox, `--verify` gate, main working-tree + `.git/index` sync,
//! and OpLog `< 10 ms` undo support.

use crate::guardrails::{
    IntegrationGuardrailOptions, check_branch_guardrails_for_integration,
    check_workspace_scope_for_integration,
};
use crate::sandbox::{
    IntegrateSessionState, IntegrationMode, IntegrationSession, ResolvedIntegrateSource,
    apply_tree_delta_with_cleanup, cleanup_session_and_sandbox, commit_tree_id,
    conflicting_dirty_paths, extract_unresolved_conflicts, integrate_sandbox_path, is_ancestor,
    load_session, materialize_integrate_sandbox, merge_labels, normalize_branch_short,
    open_gix_with_fallback, parse_oid, preserve_or_add_change_id, resolve_branch_or_revision,
    resolve_local_branch, resolve_main_repo, resolve_signatures, run_verify_command, save_session,
    sync_git_index_for_delta, verify_and_build_resolved_tree, write_commit_in_odb,
    write_workspace_state_atomic,
};
use chrono::Utc;
use gix::hash::ObjectId;
use gix::merge::tree::TreatAsUnresolved;
use gix::refs::transaction::{PreviousValue, RefEdit};
use gix::refs::{FullName, Target};
use oz_core::{MountHealth, OzError, OzoneRepo, Result, WorkspaceState, compute_status};
use oz_cow::list_workspaces;
use oz_cow::materialize::compute_tree_delta;
use oz_oplog::{ActorInfo, MutationTransaction};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Options controlling [`integrate`].
#[derive(Debug, Clone, Default)]
pub struct IntegrateOptions {
    /// Workspace names or branch names to integrate, in order.
    pub sources: Vec<String>,
    /// Target branch to integrate into (defaults to `"main"`).
    pub into: Option<String>,
    /// Verification shell command executed inside `../<repo>.oz/_integrate` before advancing target ref.
    pub verify_cmd: Option<String>,
    /// Override `--scope` and `--deny` path guardrails.
    pub allow_out_of_scope: bool,
    /// Override content guardrails (secrets, conflict markers, oversized blobs).
    pub force: bool,
    /// Optional custom merge commit message.
    pub message: Option<String>,
}

/// Summary returned after [`integrate`] or [`continue_integration`] succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrateOutcome {
    pub into_branch: String,
    pub previous_oid: String,
    pub commit_oid: String,
    pub tree_oid: String,
    pub merged_sources: Vec<String>,
    #[serde(default)]
    pub already_integrated: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op_id: Option<String>,
    pub main_worktree_synced: bool,
}

/// Integrates one or more workspaces/branches into `opts.into` (default `"main"`)
/// 100% in memory via `gix_repo.merge_commits(...)`, materializing conflicts or `--verify`
/// builds inside `../<repo>.oz/_integrate` and atomically syncing `refs/heads/<into>` +
/// `repo.repo_root` + `.git/index` inside `MutationTransaction::begin_watching_all_branches`.
pub fn integrate(
    repo: &OzoneRepo,
    opts: &IntegrateOptions,
    actor: ActorInfo,
) -> Result<IntegrateOutcome> {
    if opts.sources.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "no source workspaces or branches provided to integrate".to_string(),
            recovery: vec!["oz integrate <workspace-or-branch>... --into main".to_string()],
        });
    }

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
                "oz integrate --continue".to_string(),
                "oz integrate --abort".to_string(),
            ],
        });
    }

    let gix_repo = open_gix_with_fallback(&main_repo.repo_root)?;
    let into_branch = normalize_branch_short(opts.into.as_deref().unwrap_or("main")).to_string();
    let Some((_, original_into_oid)) = resolve_local_branch(&gix_repo, &into_branch)? else {
        return Err(OzError::InvalidUsage {
            message: format!("target branch '{into_branch}' does not exist"),
            recovery: vec![format!("Create '{into_branch}' or pass --into <existing-branch>.")],
        });
    };
    let original_into_tree = commit_tree_id(&gix_repo, original_into_oid)?;

    let resolved_sources =
        resolve_and_validate_sources(&main_repo, &gix_repo, opts, &into_branch, original_into_oid)?;

    let merge_ctx = IntegrateExecutionCtx {
        into_branch: into_branch.clone(),
        original_into_oid,
        original_into_tree,
        verify_cmd: opts.verify_cmd.clone(),
        allow_out_of_scope: opts.allow_out_of_scope,
        force: opts.force,
        message: opts.message.clone(),
    };

    run_merge_pipeline(
        repo,
        &main_repo,
        &gix_repo,
        &merge_ctx,
        original_into_oid,
        Vec::new(),
        Vec::new(),
        &resolved_sources,
        None,
        actor,
    )
}

/// Resumes a paused `Integrate` conflict session after conflicts in `../<repo>.oz/_integrate`
/// have been resolved, builds the resolved tree, merges any remaining sources, runs `--verify`,
/// and commits atomically.
pub fn continue_integration(repo: &OzoneRepo, actor: ActorInfo) -> Result<IntegrateOutcome> {
    continue_integration_with_verify(repo, None, actor)
}

/// Same as [`continue_integration`], allowing `--verify` to be supplied or overridden at continuation time.
pub fn continue_integration_with_verify(
    repo: &OzoneRepo,
    verify_override: Option<&str>,
    actor: ActorInfo,
) -> Result<IntegrateOutcome> {
    let main_repo = resolve_main_repo(repo)?;
    let Some(session) = load_session(&main_repo)? else {
        return Err(OzError::InvalidUsage {
            message: "no active integration conflict session to continue".to_string(),
            recovery: vec!["Run `oz integrate <source> --into <branch>` first.".to_string()],
        });
    };
    if session.mode != IntegrationMode::Integrate {
        return Err(OzError::InvalidUsage {
            message: "active conflict session belongs to `oz stack rebase`, not `oz integrate`"
                .to_string(),
            recovery: vec!["Run `oz stack rebase --continue` or `oz integrate --abort`.".to_string()],
        });
    }
    let state = session.integrate.as_ref().ok_or_else(|| {
        OzError::Other("integration session missing `integrate` state".to_string())
    })?;

    let gix_repo = open_gix_with_fallback(&main_repo.repo_root)?;
    let resolved_tree_id = verify_and_build_resolved_tree(&main_repo, &gix_repo, &session)?;

    let current_tip_oid = parse_oid(&state.current_tip_oid)?;
    let src_commit_oid = parse_oid(&state.current_source.commit_oid)?;
    let merge_commit_oid = create_merge_commit(
        &gix_repo,
        &actor,
        current_tip_oid,
        src_commit_oid,
        resolved_tree_id,
        &state.current_source.name,
        &state.into_branch,
        state.message.as_deref(),
    )?;

    let mut merged_sources = state.merged_sources_so_far.clone();
    merged_sources.push(state.current_source.name.clone());

    let merge_ctx = IntegrateExecutionCtx {
        into_branch: state.into_branch.clone(),
        original_into_oid: parse_oid(&state.original_into_oid)?,
        original_into_tree: parse_oid(&state.original_into_tree)?,
        verify_cmd: verify_override
            .map(str::to_string)
            .or_else(|| state.verify_cmd.clone()),
        allow_out_of_scope: state.allow_out_of_scope,
        force: state.force,
        message: state.message.clone(),
    };

    run_merge_pipeline(
        repo,
        &main_repo,
        &gix_repo,
        &merge_ctx,
        merge_commit_oid,
        merged_sources,
        state.already_integrated.clone(),
        &state.remaining_sources,
        Some((&session.sandbox_path, resolved_tree_id)),
        actor,
    )
}

struct IntegrateExecutionCtx {
    into_branch: String,
    original_into_oid: ObjectId,
    original_into_tree: ObjectId,
    verify_cmd: Option<String>,
    allow_out_of_scope: bool,
    force: bool,
    message: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn run_merge_pipeline(
    caller_repo: &OzoneRepo,
    main_repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    ctx: &IntegrateExecutionCtx,
    mut current_tip_oid: ObjectId,
    mut merged_sources: Vec<String>,
    mut already_integrated: Vec<String>,
    sources: &[ResolvedIntegrateSource],
    existing_sandbox: Option<(&Path, ObjectId)>,
    actor: ActorInfo,
) -> Result<IntegrateOutcome> {
    for (idx, src) in sources.iter().enumerate() {
        let src_oid = parse_oid(&src.commit_oid)?;
        if is_ancestor(gix_repo, src_oid, current_tip_oid)? {
            already_integrated.push(src.name.clone());
            continue;
        }

        let merge_opts: gix::merge::commit::Options = gix_repo
            .tree_merge_options()
            .map_err(|err| OzError::Git(err.to_string()))?
            .into();
        let labels = merge_labels("base", &ctx.into_branch, &src.name);
        let mut outcome = gix_repo
            .merge_commits(current_tip_oid, src_oid, labels, merge_opts)
            .map_err(|err| OzError::Git(err.to_string()))?;

        if outcome
            .tree_merge
            .has_unresolved_conflicts(TreatAsUnresolved::git())
        {
            let merged_tree_id = outcome
                .tree_merge
                .tree
                .write()
                .map_err(|err| OzError::Git(err.to_string()))?
                .detach();
            let (stored_conflicts, conflict_entries) = extract_unresolved_conflicts(
                gix_repo,
                &outcome.tree_merge.conflicts,
                merged_tree_id,
            )?;

            let sandbox_dir = integrate_sandbox_path(main_repo)?;
            materialize_integrate_sandbox(
                main_repo,
                gix_repo,
                &sandbox_dir,
                merged_tree_id,
                Some(ctx.original_into_oid),
            )?;

            let now = Utc::now();
            let session = IntegrationSession {
                schema_version: IntegrationSession::CURRENT_SCHEMA_VERSION,
                mode: IntegrationMode::Integrate,
                sandbox_path: sandbox_dir.clone(),
                created_at: now,
                updated_at: now,
                conflicts: stored_conflicts,
                integrate: Some(IntegrateSessionState {
                    into_branch: ctx.into_branch.clone(),
                    original_into_oid: ctx.original_into_oid.to_hex().to_string(),
                    original_into_tree: ctx.original_into_tree.to_hex().to_string(),
                    current_tip_oid: current_tip_oid.to_hex().to_string(),
                    conflict_merged_tree_oid: merged_tree_id.to_hex().to_string(),
                    current_source: src.clone(),
                    merged_sources_so_far: merged_sources,
                    already_integrated,
                    remaining_sources: sources[idx + 1..].to_vec(),
                    verify_cmd: ctx.verify_cmd.clone(),
                    allow_out_of_scope: ctx.allow_out_of_scope,
                    force: ctx.force,
                    message: ctx.message.clone(),
                }),
                stack_rebase: None,
            };
            save_session(main_repo, &session)?;

            let mut recovery: Vec<String> = conflict_entries
                .iter()
                .flat_map(|c| c.suggested_commands.iter().cloned())
                .collect();
            recovery.push("oz resolve --all --take ours --continue".to_string());
            recovery.push("oz integrate --continue".to_string());
            recovery.push("oz integrate --abort".to_string());

            return Err(OzError::Conflict {
                message: format!(
                    "{} merge conflict(s) integrating '{}' into '{}' (materialized in {})",
                    conflict_entries.len(),
                    src.name,
                    ctx.into_branch,
                    sandbox_dir.display()
                ),
                conflicts: conflict_entries,
                recovery,
            });
        }

        let merged_tree_id = outcome
            .tree_merge
            .tree
            .write()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach();
        current_tip_oid = create_merge_commit(
            gix_repo,
            &actor,
            current_tip_oid,
            src_oid,
            merged_tree_id,
            &src.name,
            &ctx.into_branch,
            ctx.message.as_deref(),
        )?;
        merged_sources.push(src.name.clone());
    }

    if merged_sources.is_empty() {
        let _ = cleanup_session_and_sandbox(
            main_repo,
            existing_sandbox.map(|(p, _)| p),
        );
        return Ok(IntegrateOutcome {
            into_branch: ctx.into_branch.clone(),
            previous_oid: ctx.original_into_oid.to_hex().to_string(),
            commit_oid: ctx.original_into_oid.to_hex().to_string(),
            tree_oid: ctx.original_into_tree.to_hex().to_string(),
            merged_sources,
            already_integrated,
            op_id: None,
            main_worktree_synced: false,
        });
    }

    finalize_integration(
        caller_repo,
        main_repo,
        gix_repo,
        ctx,
        current_tip_oid,
        merged_sources,
        already_integrated,
        existing_sandbox,
        actor,
    )
}

#[allow(clippy::too_many_arguments)]
fn finalize_integration(
    caller_repo: &OzoneRepo,
    main_repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    ctx: &IntegrateExecutionCtx,
    final_commit_oid: ObjectId,
    merged_sources: Vec<String>,
    already_integrated: Vec<String>,
    existing_sandbox: Option<(&Path, ObjectId)>,
    actor: ActorInfo,
) -> Result<IntegrateOutcome> {
    let final_tree_id = commit_tree_id(gix_repo, final_commit_oid)?;
    let sandbox_dir = integrate_sandbox_path(main_repo)?;
    let mut created_temp_sandbox = false;

    if let Some(ref verify_cmd) = ctx.verify_cmd {
        match existing_sandbox {
            Some((existing_dir, resolved_tree_id)) => {
                if resolved_tree_id != final_tree_id {
                    apply_tree_delta_with_cleanup(
                        gix_repo,
                        resolved_tree_id,
                        final_tree_id,
                        existing_dir,
                    )?;
                }
                if let Err(err) = run_verify_command(existing_dir, verify_cmd) {
                    let _ = cleanup_session_and_sandbox(main_repo, Some(existing_dir));
                    return Err(err);
                }
            }
            None => {
                materialize_integrate_sandbox(
                    main_repo,
                    gix_repo,
                    &sandbox_dir,
                    final_tree_id,
                    Some(ctx.original_into_oid),
                )?;
                created_temp_sandbox = true;
                if let Err(err) = run_verify_command(&sandbox_dir, verify_cmd) {
                    let _ = cleanup_session_and_sandbox(main_repo, Some(&sandbox_dir));
                    return Err(err);
                }
            }
        }
    }

    // Check if `main_repo.repo_root` currently has `HEAD -> refs/heads/<into_branch>` checked out.
    let main_on_into = match gix_repo.head() {
        Ok(head) => head
            .referent_name()
            .is_some_and(|n| String::from_utf8_lossy(n.shorten().as_ref()) == ctx.into_branch),
        Err(_) => false,
    };

    let delta = compute_tree_delta(gix_repo, ctx.original_into_tree, final_tree_id)?;
    let main_status = if main_on_into {
        let st = compute_status(main_repo)?;
        let conflicts = conflicting_dirty_paths(&st, &delta);
        if !conflicts.is_empty() {
            if created_temp_sandbox {
                let _ = cleanup_session_and_sandbox(main_repo, Some(&sandbox_dir));
            }
            return Err(OzError::Conflict {
                message: format!(
                    "main working tree on '{}' has conflicting uncommitted changes in: {}",
                    ctx.into_branch,
                    conflicts.join(", ")
                ),
                conflicts: Vec::new(),
                recovery: vec![
                    "oz commit -m \"save local edits\"".to_string(),
                    "oz checkpoint -m \"before integrate\"".to_string(),
                ],
            });
        }
        Some(st)
    } else {
        None
    };

    // Check any active Ozone workspace currently checked out on `ctx.into_branch`.
    let summaries = list_workspaces(main_repo)?;
    let mut ws_updates = Vec::new();
    let mut caller_ws_dirty = None;
    for summary in summaries {
        let ws = summary.state;
        if ws.branch != ctx.into_branch {
            continue;
        }
        if (ws.worktree_path.is_dir() || ws.backend.requires_daemon())
            && let Ok(ws_repo) = OzoneRepo::discover(Some(&ws.worktree_path))
            && let Ok(st) = compute_status(&ws_repo)
        {
            let conflicts = conflicting_dirty_paths(&st, &delta);
            if !conflicts.is_empty() {
                if created_temp_sandbox {
                    let _ = cleanup_session_and_sandbox(main_repo, Some(&sandbox_dir));
                }
                return Err(OzError::Conflict {
                    message: format!(
                        "workspace '{}' on '{}' has conflicting uncommitted changes in: {}",
                        ws.name,
                        ws.branch,
                        conflicts.join(", ")
                    ),
                    conflicts: Vec::new(),
                    recovery: vec![format!(
                        "oz -C {} commit -m \"save local edits\"",
                        ws.worktree_path.display()
                    )],
                });
            }
            if caller_repo.workspace.as_deref() == Some(ws.name.as_str()) {
                caller_ws_dirty = Some(st.changes);
            }
        }
        ws_updates.push((ws, summary.mount_health));
    }

    let use_caller_ws_tx = !main_on_into
        && caller_repo.workspace.is_some()
        && caller_repo.branch.as_deref() == Some(ctx.into_branch.as_str());
    let tx_repo = if use_caller_ws_tx {
        caller_repo
    } else {
        main_repo
    };
    let tx_dirty_override = if use_caller_ws_tx {
        caller_ws_dirty.as_deref()
    } else {
        main_status.as_ref().map(|s| s.changes.as_slice())
    };

    let cmd_str = format!("oz integrate {} --into {}", merged_sources.join(" "), ctx.into_branch);
    let tx = MutationTransaction::begin_watching_all_branches(
        tx_repo,
        cmd_str,
        actor,
        tx_dirty_override,
    )?;

    let full_ref_name = format!("refs/heads/{}", ctx.into_branch);
    let full_name = FullName::try_from(full_ref_name.as_str())
        .map_err(|err| OzError::Git(format!("invalid ref '{full_ref_name}': {err}")))?;
    let reflog_msg = format!(
        "oz integrate {} into {}",
        merged_sources.join(", "),
        ctx.into_branch
    );

    if let Err(err) = gix_repo.edit_reference(RefEdit::update(
        full_name,
        final_commit_oid,
        PreviousValue::MustExistAndMatch(Target::Object(ctx.original_into_oid)),
        reflog_msg,
    )) {
        if created_temp_sandbox {
            let _ = cleanup_session_and_sandbox(main_repo, Some(&sandbox_dir));
        }
        return Err(OzError::Conflict {
            message: format!(
                "atomic CAS update on '{full_ref_name}' failed (branch moved concurrently): {err}"
            ),
            conflicts: Vec::new(),
            recovery: vec!["Retry `oz integrate` against the updated target branch.".to_string()],
        });
    }

    if main_on_into {
        apply_tree_delta_with_cleanup(
            gix_repo,
            ctx.original_into_tree,
            final_tree_id,
            &main_repo.repo_root,
        )?;
        sync_git_index_for_delta(
            &main_repo.repo_root,
            &main_repo.repo_root,
            ctx.original_into_tree,
            final_tree_id,
            &delta,
        )?;
    }

    for (mut ws, mount_health) in ws_updates {
        if ws.worktree_path.is_dir()
            && (!ws.backend.requires_daemon() || mount_health == MountHealth::Healthy)
        {
            apply_tree_delta_with_cleanup(
                gix_repo,
                ctx.original_into_tree,
                final_tree_id,
                &ws.worktree_path,
            )?;
        }
        if let Some(ref wt_git_dir) = ws.git_worktree_dir
            && wt_git_dir.is_dir()
        {
            sync_git_index_for_delta(
                wt_git_dir,
                &ws.worktree_path,
                ctx.original_into_tree,
                final_tree_id,
                &delta,
            )?;
        }
        ws.base_commit = Some(final_commit_oid.to_hex().to_string());
        ws.base_tree = final_tree_id.to_hex().to_string();
        ws.updated_at = Utc::now();
        write_workspace_state_atomic(&main_repo.layout.workspace_state_path(&ws.name), &ws)?;
    }

    let _ = cleanup_session_and_sandbox(
        main_repo,
        existing_sandbox.map(|(p, _)| p).or(Some(&sandbox_dir)),
    );

    let op_entry = tx.finish(tx_repo, tx_dirty_override)?;

    Ok(IntegrateOutcome {
        into_branch: ctx.into_branch.clone(),
        previous_oid: ctx.original_into_oid.to_hex().to_string(),
        commit_oid: final_commit_oid.to_hex().to_string(),
        tree_oid: final_tree_id.to_hex().to_string(),
        merged_sources,
        already_integrated,
        op_id: Some(op_entry.op_id),
        main_worktree_synced: main_on_into,
    })
}

fn resolve_and_validate_sources(
    main_repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    opts: &IntegrateOptions,
    into_branch: &str,
    target_commit_oid: ObjectId,
) -> Result<Vec<ResolvedIntegrateSource>> {
    let summaries = list_workspaces(main_repo)?;
    let mut by_ws_name: HashMap<String, WorkspaceState> = HashMap::new();
    let mut by_ws_branch: HashMap<String, WorkspaceState> = HashMap::new();
    for s in summaries {
        by_ws_branch.insert(s.state.branch.clone(), s.state.clone());
        by_ws_name.insert(s.state.name.clone(), s.state);
    }

    let guardrail_opts = IntegrationGuardrailOptions {
        allow_out_of_scope: opts.allow_out_of_scope,
        force: opts.force,
        into_branch: Some(into_branch.to_string()),
    };

    let mut out = Vec::with_capacity(opts.sources.len());
    for raw_src in &opts.sources {
        let short_src = normalize_branch_short(raw_src);
        let ws_opt = by_ws_name
            .get(raw_src)
            .or_else(|| by_ws_branch.get(short_src))
            .cloned();

        if let Some(ws) = ws_opt {
            if ws.branch == into_branch {
                return Err(OzError::InvalidUsage {
                    message: format!(
                        "cannot integrate workspace '{}' whose branch '{}' is the target branch",
                        ws.name, into_branch
                    ),
                    recovery: Vec::new(),
                });
            }

            if (ws.worktree_path.is_dir() || ws.backend.requires_daemon())
                && let Ok(ws_repo) = OzoneRepo::discover(Some(&ws.worktree_path))
            {
                let status = compute_status(&ws_repo)?;
                if !status.is_clean() {
                    return Err(OzError::InvalidUsage {
                        message: format!(
                            "workspace '{}' has {} uncommitted change(s); commit or discard them before integrating",
                            ws.name,
                            status.changes.len()
                        ),
                        recovery: vec![
                            format!(
                                "oz -C {} commit -m \"ready for integration\"",
                                ws.worktree_path.display()
                            ),
                            format!("oz -C {} status", ws.worktree_path.display()),
                        ],
                    });
                }
            }

            check_workspace_scope_for_integration(main_repo, &ws, &guardrail_opts)?;

            let Some((_, commit_id)) = resolve_local_branch(gix_repo, &ws.branch)? else {
                return Err(OzError::InvalidUsage {
                    message: format!(
                        "workspace '{}' branch '{}' has no commits yet",
                        ws.name, ws.branch
                    ),
                    recovery: vec![format!(
                        "oz -C {} commit -m \"initial\"",
                        ws.worktree_path.display()
                    )],
                });
            };

            out.push(ResolvedIntegrateSource {
                name: raw_src.clone(),
                branch: ws.branch,
                commit_oid: commit_id.to_hex().to_string(),
                workspace: Some(ws.name),
            });
            continue;
        }

        let Some((_, commit_id)) = resolve_branch_or_revision(gix_repo, raw_src)? else {
            return Err(OzError::WorkspaceNotFound {
                name: raw_src.clone(),
            });
        };
        if short_src == into_branch {
            return Err(OzError::InvalidUsage {
                message: format!("cannot integrate branch '{into_branch}' into itself"),
                recovery: Vec::new(),
            });
        }

        check_branch_guardrails_for_integration(
            main_repo,
            raw_src,
            commit_id,
            Some(target_commit_oid),
            None,
            &guardrail_opts,
        )?;

        out.push(ResolvedIntegrateSource {
            name: raw_src.clone(),
            branch: short_src.to_string(),
            commit_oid: commit_id.to_hex().to_string(),
            workspace: None,
        });
    }

    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn create_merge_commit(
    gix_repo: &gix::Repository,
    actor: &ActorInfo,
    first_parent: ObjectId,
    second_parent: ObjectId,
    tree_id: ObjectId,
    source_name: &str,
    into_branch: &str,
    custom_message: Option<&str>,
) -> Result<ObjectId> {
    let (author, committer) = resolve_signatures(gix_repo, actor, None)?;
    let base_msg = match custom_message {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => format!("Merge '{source_name}' into {into_branch}"),
    };
    let parents = [first_parent, second_parent];
    let (final_msg, _change_id) =
        preserve_or_add_change_id(&base_msg, tree_id, &parents, &author);
    write_commit_in_odb(gix_repo, &author, &committer, &final_msg, tree_id, &parents)
}
