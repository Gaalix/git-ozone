// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Milestone 1 subcommand implementations for `oz` and `git-ozone`.

use crate::recovery::discover_and_recover_repo;
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use oz_core::{
    BackendKind, CommitOptions, CommitOutcome, DiffReport, Envelope, ExitCode, FileChange,
    InteractionMode, OzError, OzoneRepo, Result, compute_diff, compute_status, render_unified_diff,
};
use oz_cow::{
    GcWorkspacesOptions, RemoveWorkspaceOptions, SpawnWorkspaceOptions, detect_filesystem,
    gc_workspaces, list_workspaces, remove_workspace, select_backend, spawn_workspace,
};
use oz_oplog::{
    ActorInfo, ActorKind, CheckpointOutcome, MutationTransaction, RedoOutcome, UndoOutcome,
    UndoTarget, checkpoint, redo, undo,
};
use oz_warm::{BaselineSource, gc_baselines};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Workspace lifecycle subcommands (`oz ws ...`).
#[derive(Debug, Subcommand)]
pub enum WsCommand {
    /// Spawn a new isolated CoW/FUSE3 workspace on a dedicated branch.
    Spawn(WsSpawnArgs),
    /// List registered Ozone workspaces and live mount health.
    List,
    /// Remove a workspace, unmount VFS if active, and clean colocated metadata.
    Rm(WsRmArgs),
    /// Garbage-collect merged/missing workspaces and LRU warm baselines.
    Gc(WsGcArgs),
}

#[derive(Debug, Args)]
pub struct WsSpawnArgs {
    /// Workspace name (materialized at `../<repo>.oz/<name>` by default).
    pub name: String,
    /// Branch name to create or check out (defaults to `<name>`).
    #[arg(long, value_name = "BRANCH")]
    pub branch: Option<String>,
    /// Base revision or commit to branch from (defaults to `HEAD`).
    #[arg(long = "from", value_name = "REV")]
    pub from_ref: Option<String>,
    /// Skip warm build/dependency cache baseline capture and mtime preservation.
    #[arg(long)]
    pub no_warm: bool,
    /// Allowed path glob patterns for pre-commit scope enforcement.
    #[arg(long, num_args = 1.., action = clap::ArgAction::Append, value_name = "GLOB")]
    pub scope: Vec<String>,
    /// Denied path glob patterns enforced before commit.
    #[arg(long, num_args = 1.., action = clap::ArgAction::Append, value_name = "GLOB")]
    pub deny: Vec<String>,
}

#[derive(Debug, Args)]
pub struct WsRmArgs {
    /// Name of the workspace to remove.
    pub name: String,
    /// Force removal even if the workspace has recorded dirty changes.
    #[arg(long)]
    pub force: bool,
    /// Also delete the workspace's local branch after removal.
    #[arg(long)]
    pub delete_branch: bool,
}

#[derive(Debug, Args)]
pub struct WsGcArgs {
    /// Maximum warm baseline cache budget in GiB (defaults to config `max_baseline_gb`).
    #[arg(long, value_name = "GB")]
    pub max_gb: Option<f64>,
    /// Target branch used to detect merged workspace branches.
    #[arg(long, value_name = "BRANCH")]
    pub target_branch: Option<String>,
    /// Preview deletions without modifying workspaces or baselines.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct DiffArgs {
    /// Optional file paths, directory prefixes, file IDs (`f1`), or hunk IDs (`h1a`).
    pub targets: Vec<String>,
}

#[derive(Debug, Args)]
pub struct CommitArgs {
    /// Commit message (a `Change-Id:` trailer is appended automatically if missing).
    #[arg(short = 'm', long = "message", value_name = "MSG")]
    pub message: String,
    /// Select specific file IDs (`f1`), hunk IDs (`h1a`), or paths to commit without staging.
    #[arg(long, num_args = 1.., action = clap::ArgAction::Append, value_name = "ID")]
    pub pick: Vec<String>,
    /// Exclude specific file IDs (`f3`), hunk IDs (`h1b`), or paths from this commit.
    #[arg(long, num_args = 1.., action = clap::ArgAction::Append, value_name = "ID")]
    pub exclude: Vec<String>,
    /// Override secret, conflict-marker, and oversized-blob pre-commit guardrails.
    #[arg(long)]
    pub force: bool,
    /// Override workspace `--scope` and `--deny` path guardrails.
    #[arg(long)]
    pub allow_out_of_scope: bool,
    /// Allow creating an empty commit when no dirty changes exist.
    #[arg(long)]
    pub allow_empty: bool,
}

#[derive(Debug, Args)]
pub struct CheckpointArgs {
    /// Optional message describing this working-tree checkpoint.
    #[arg(short = 'm', long = "message", value_name = "MSG")]
    pub message: Option<String>,
    /// Suppress human stdout output on success (ignored with `--json`).
    #[arg(short = 'q', long)]
    pub quiet: bool,
}

#[derive(Debug, Args)]
pub struct UndoArgs {
    /// Specific operation ID (`op_...`) to undo instead of stepping back sequentially.
    pub op_id: Option<String>,
    /// Number of sequential operations to undo (defaults to `1`).
    #[arg(long, value_name = "N")]
    pub steps: Option<usize>,
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// Install a `systemd --user` service unit for `oz-vfsd` on Linux.
    #[arg(long)]
    pub systemd_user: bool,
}

#[derive(Debug, Serialize)]
struct WsSpawnData {
    workspace: String,
    branch: String,
    worktree_path: PathBuf,
    git_worktree_dir: PathBuf,
    backend: BackendKind,
    base_commit: Option<String>,
    base_tree: String,
    baseline_id: Option<String>,
    baseline_source: Option<&'static str>,
    scope: Vec<String>,
    deny: Vec<String>,
    mounted: bool,
    spawn_elapsed_ms: f64,
}

#[derive(Debug, Serialize)]
struct WsListItem {
    name: String,
    branch: String,
    backend: BackendKind,
    worktree_path: PathBuf,
    base_commit: Option<String>,
    branch_head_oid: Option<String>,
    mount_health: &'static str,
    healthy: bool,
    mounted: Option<bool>,
    daemon_pid: Option<u32>,
    dirty_count: usize,
    pinned_count: usize,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct WsListData {
    workspaces: Vec<WsListItem>,
}

#[derive(Debug, Serialize)]
struct WsRmData {
    name: String,
    worktree_path: PathBuf,
    branch: Option<String>,
    branch_deleted: bool,
}

#[derive(Debug, Serialize)]
struct WsGcData {
    removed_workspaces: Vec<String>,
    skipped_dirty_workspaces: Vec<String>,
    removed_baselines: Vec<String>,
    reclaimed_baseline_bytes: u64,
    remaining_baseline_bytes: u64,
    remaining_baseline_count: usize,
    max_baseline_gb: f64,
    dry_run: bool,
}

#[derive(Debug, Serialize)]
struct DiffData {
    #[serde(flatten)]
    report: DiffReport,
    unified_diff: String,
}

#[derive(Debug, Serialize)]
struct UndoData {
    #[serde(flatten)]
    outcome: UndoOutcome,
    engine_elapsed_ms: f64,
}

#[derive(Debug, Serialize)]
struct RedoData {
    #[serde(flatten)]
    outcome: RedoOutcome,
    engine_elapsed_ms: f64,
}

#[derive(Debug, Serialize)]
struct SetupData {
    repo_root: PathBuf,
    git_common_dir: PathBuf,
    ozone_dir: PathBuf,
    recommended_backend: BackendKind,
    oz_vfsd_bin: PathBuf,
    systemd_unit_path: Option<PathBuf>,
}

fn actor_from_mode(mode: InteractionMode, repo: &OzoneRepo) -> ActorInfo {
    ActorInfo {
        kind: if mode.agent {
            ActorKind::Agent
        } else {
            ActorKind::Human
        },
        client: Some("oz-cli".to_string()),
        workspace: repo.workspace.clone(),
    }
}

fn baseline_source_label(source: BaselineSource) -> &'static str {
    match source {
        BaselineSource::Existing => "existing",
        BaselineSource::Captured => "captured",
        BaselineSource::FallbackPrior => "fallback_prior",
    }
}

pub(crate) fn print_json_envelope<T: Serialize>(env: &Envelope<T>) {
    match env.to_json_pretty() {
        Ok(s) => println!("{s}"),
        Err(err) => eprintln!("error: failed to serialize oz.v1 envelope: {err}"),
    }
}

fn render_command_result<T: Serialize, F>(
    mode: InteractionMode,
    res: Result<Envelope<T>>,
    human_render: F,
) -> ExitCode
where
    F: FnOnce(&Envelope<T>),
{
    match res {
        Ok(env) => {
            if mode.json {
                print_json_envelope(&env);
            } else {
                human_render(&env);
            }
            ExitCode::Success
        }
        Err(err) => {
            if mode.json {
                let env = Envelope::from_error(&err);
                print_json_envelope(&env);
            } else {
                eprintln!("error: {err}");
                for cmd in err.recovery_commands() {
                    eprintln!("hint: {cmd}");
                }
            }
            err.exit_code()
        }
    }
}

pub(crate) fn handle_ws(
    cmd: WsCommand,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    match cmd {
        WsCommand::Spawn(args) => render_command_result(
            mode,
            execute_ws_spawn(&args, directory, mode),
            |env| {
                let d = &env.data;
                println!(
                    "spawned workspace '{}' on branch '{}' ({}, {:.1} ms) -> {}",
                    d.workspace,
                    d.branch,
                    d.backend,
                    d.spawn_elapsed_ms,
                    d.worktree_path.display()
                );
            },
        ),
        WsCommand::List => render_command_result(
            mode,
            execute_ws_list(directory),
            |env| {
                if env.data.workspaces.is_empty() {
                    println!("no workspaces");
                } else {
                    for ws in &env.data.workspaces {
                        println!(
                            "{}  branch={}  backend={}  health={}  path={}",
                            ws.name,
                            ws.branch,
                            ws.backend,
                            ws.mount_health,
                            ws.worktree_path.display()
                        );
                    }
                }
            },
        ),
        WsCommand::Rm(args) => render_command_result(
            mode,
            execute_ws_rm(&args, directory, mode),
            |env| {
                let d = &env.data;
                println!(
                    "removed workspace '{}' ({})",
                    d.name,
                    d.worktree_path.display()
                );
            },
        ),
        WsCommand::Gc(args) => render_command_result(
            mode,
            execute_ws_gc(&args, directory, mode),
            |env| {
                let d = &env.data;
                println!(
                    "gc: removed {} workspace(s) ({} skipped dirty), {} baseline(s) ({} bytes reclaimed)",
                    d.removed_workspaces.len(),
                    d.skipped_dirty_workspaces.len(),
                    d.removed_baselines.len(),
                    d.reclaimed_baseline_bytes
                );
            },
        ),
    }
}

fn execute_ws_spawn(
    args: &WsSpawnArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> Result<Envelope<WsSpawnData>> {
    let repo = discover_and_recover_repo(directory, true)?;
    let pre_status = compute_status(&repo)?;
    let cmd_str = format!("oz ws spawn {}", args.name);
    let actor = actor_from_mode(mode, &repo);
    let tx = MutationTransaction::begin(&repo, cmd_str, actor, Some(&pre_status.changes))?;

    let opts = SpawnWorkspaceOptions {
        name: args.name.clone(),
        branch: args.branch.clone(),
        from_ref: args.from_ref.clone(),
        backend: None,
        scope: args.scope.clone(),
        deny: args.deny.clone(),
        warm: !args.no_warm,
    };

    let started = Instant::now();
    let info = spawn_workspace(&repo, &opts)?;
    let spawn_elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let entry = tx.finish(&repo, Some(&pre_status.changes))?;

    let data = WsSpawnData {
        workspace: info.state.name.clone(),
        branch: info.state.branch.clone(),
        worktree_path: info.workspace.workspace_path,
        git_worktree_dir: info.git_worktree_dir,
        backend: info.state.backend,
        base_commit: info.state.base_commit.clone(),
        base_tree: info.state.base_tree.clone(),
        baseline_id: info.state.baseline_id.clone(),
        baseline_source: info.baseline_source.map(baseline_source_label),
        scope: info.state.scope.clone(),
        deny: info.state.deny.clone(),
        mounted: info
            .workspace
            .mount_record
            .as_ref()
            .is_some_and(|m| m.mounted),
        spawn_elapsed_ms,
    };

    Ok(Envelope::success(data)
        .with_op_id(entry.op_id)
        .with_workspace(Some(info.state.name.clone()))
        .with_branch(Some(info.state.branch))
        .with_head_oid(info.state.base_commit)
        .with_recovery_command(format!("oz ws rm {}", info.state.name)))
}

fn execute_ws_list(directory: Option<&Path>) -> Result<Envelope<WsListData>> {
    let repo = discover_and_recover_repo(directory, true)?;
    let summaries = list_workspaces(&repo)?;
    let workspaces = summaries
        .into_iter()
        .map(|s| WsListItem {
            name: s.state.name,
            branch: s.state.branch,
            backend: s.state.backend,
            worktree_path: s.state.worktree_path,
            base_commit: s.state.base_commit,
            branch_head_oid: s.branch_head_oid,
            mount_health: s.mount_health.as_str(),
            healthy: s.healthy,
            mounted: s.mount_record.as_ref().map(|m| m.mounted),
            daemon_pid: s.mount_record.and_then(|m| m.daemon_pid),
            dirty_count: s.state.dirty_paths.len(),
            pinned_count: s.state.pinned_paths.len(),
            created_at: s.state.created_at,
            updated_at: s.state.updated_at,
        })
        .collect();

    Ok(Envelope::success(WsListData { workspaces })
        .with_workspace(repo.workspace)
        .with_branch(repo.branch)
        .with_head_oid(repo.head_oid))
}

fn execute_ws_rm(
    args: &WsRmArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> Result<Envelope<WsRmData>> {
    let repo = discover_and_recover_repo(directory, false)?;
    // Anchor OpLog mutation transaction on main repo if removing the current workspace.
    let tx_repo = if repo.workspace.as_deref() == Some(args.name.as_str()) {
        OzoneRepo::discover(Some(&repo.repo_root))?
    } else {
        repo.clone()
    };
    let pre_status = compute_status(&tx_repo)?;
    let cmd_str = format!("oz ws rm {}", args.name);
    let actor = actor_from_mode(mode, &tx_repo);
    let tx = MutationTransaction::begin(&tx_repo, cmd_str, actor, Some(&pre_status.changes))?;

    let removed = remove_workspace(
        &repo,
        &args.name,
        &RemoveWorkspaceOptions {
            force: args.force,
            delete_branch: args.delete_branch,
        },
    )?;

    let entry = tx.finish(&tx_repo, Some(&pre_status.changes))?;

    Ok(Envelope::success(WsRmData {
        name: removed.name,
        worktree_path: removed.worktree_path,
        branch: removed.branch,
        branch_deleted: removed.branch_deleted,
    })
    .with_op_id(entry.op_id)
    .with_workspace(tx_repo.workspace)
    .with_branch(tx_repo.branch)
    .with_head_oid(tx_repo.head_oid)
    .with_recovery_command("oz ws list --json"))
}

fn execute_ws_gc(
    args: &WsGcArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> Result<Envelope<WsGcData>> {
    if let Some(gb) = args.max_gb
        && (!gb.is_finite() || gb <= 0.0)
    {
        return Err(OzError::InvalidUsage {
            message: "--max-gb must be a positive finite number".to_string(),
            recovery: vec!["Pass e.g. `oz ws gc --max-gb 10`.".to_string()],
        });
    }

    let repo = discover_and_recover_repo(directory, true)?;
    let main_repo = if repo.workspace.is_some() {
        OzoneRepo::discover(Some(&repo.repo_root))?
    } else {
        repo.clone()
    };
    let pre_status = compute_status(&main_repo)?;
    let actor = actor_from_mode(mode, &main_repo);
    let tx = if args.dry_run {
        None
    } else {
        Some(MutationTransaction::begin(
            &main_repo,
            "oz ws gc",
            actor,
            Some(&pre_status.changes),
        )?)
    };

    let ws_gc = gc_workspaces(
        &repo,
        &GcWorkspacesOptions {
            target_branch: args.target_branch.clone(),
            prune_merged: true,
            prune_missing: true,
            dry_run: args.dry_run,
        },
    )?;

    let max_baseline_gb = args.max_gb.unwrap_or(repo.config.max_baseline_gb);
    let baseline_gc = if args.dry_run {
        Default::default()
    } else {
        gc_baselines(&repo.layout.ozone_dir, max_baseline_gb)?
    };

    let op_id = match tx {
        Some(t) => Some(t.finish(&main_repo, Some(&pre_status.changes))?.op_id),
        None => None,
    };

    let mut env = Envelope::success(WsGcData {
        removed_workspaces: ws_gc.removed,
        skipped_dirty_workspaces: ws_gc.skipped_dirty,
        removed_baselines: baseline_gc.removed_baselines,
        reclaimed_baseline_bytes: baseline_gc.reclaimed_bytes,
        remaining_baseline_bytes: baseline_gc.remaining_bytes,
        remaining_baseline_count: baseline_gc.remaining_count,
        max_baseline_gb,
        dry_run: args.dry_run,
    })
    .with_workspace(main_repo.workspace)
    .with_branch(main_repo.branch)
    .with_head_oid(main_repo.head_oid)
    .with_recovery_command("oz ws list --json");

    if let Some(id) = op_id {
        env = env.with_op_id(id);
    }
    Ok(env)
}

pub(crate) fn handle_status(directory: Option<&Path>, mode: InteractionMode) -> ExitCode {
    let res = (|| -> Result<Envelope<_>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let report = compute_status(&repo)?;
        let workspace = report.workspace.clone();
        let branch = report.branch.clone();
        let head_oid = report.head_oid.clone();
        Ok(Envelope::success(report)
            .with_workspace(workspace)
            .with_branch(branch)
            .with_head_oid(head_oid))
    })();

    render_command_result(mode, res, |env| {
        let r = &env.data;
        let branch_label = r.branch.as_deref().unwrap_or("detached");
        if let Some(ref ws) = r.workspace {
            println!("On branch {branch_label} (workspace: {ws})");
        } else {
            println!("On branch {branch_label}");
        }
        if r.changes.is_empty() {
            println!("nothing to commit, working tree clean");
            return;
        }
        for change in &r.changes {
            print_status_change_line(change);
        }
    })
}

fn print_status_change_line(change: &FileChange) {
    let hunk_ids: Vec<&str> = change.hunks.iter().map(|h| h.id.as_str()).collect();
    let mut tags = Vec::new();
    if change.is_binary {
        tags.push("binary".to_string());
    } else if !hunk_ids.is_empty() {
        tags.push(format!("hunks: {}", hunk_ids.join(", ")));
    }
    if change.out_of_scope {
        tags.push("out-of-scope".to_string());
    }
    if change.pinned {
        tags.push("pinned".to_string());
    }
    let suffix = if tags.is_empty() {
        String::new()
    } else {
        format!(" ({})", tags.join("; "))
    };
    println!(
        "  [{}] {}  {} (+{}/-{}){suffix}",
        change.id,
        change.kind.short_code(),
        change.path,
        change.additions,
        change.deletions
    );
}

pub(crate) fn handle_diff(
    args: &DiffArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<DiffData>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let report = compute_diff(&repo, &args.targets)?;
        let unified_diff = render_unified_diff(&report);
        let workspace = report.workspace.clone();
        let branch = report.branch.clone();
        let head_oid = report.head_oid.clone();
        Ok(Envelope::success(DiffData {
            report,
            unified_diff,
        })
        .with_workspace(workspace)
        .with_branch(branch)
        .with_head_oid(head_oid))
    })();

    render_command_result(mode, res, |env| {
        if !env.data.unified_diff.is_empty() {
            print!("{}", env.data.unified_diff);
        }
    })
}

pub(crate) fn handle_commit(
    args: &CommitArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<CommitOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let pre_status = compute_status(&repo)?;
        let actor = actor_from_mode(mode, &repo);
        let tx = MutationTransaction::begin(
            &repo,
            "oz commit",
            actor,
            Some(&pre_status.changes),
        )?;

        let opts = CommitOptions {
            message: args.message.clone(),
            pick: args.pick.clone(),
            exclude: args.exclude.clone(),
            force: args.force,
            allow_out_of_scope: args.allow_out_of_scope,
            allow_empty: args.allow_empty,
            author_name: None,
            author_email: None,
        };
        let outcome = oz_core::commit_changes(&repo, &opts)?;

        let entry = if outcome.remaining_dirty_count == 0 {
            tx.finish_with_message(&repo, Some(&outcome.message), Some(&[]))?
        } else {
            let post_status = compute_status(&repo)?;
            tx.finish_with_message(&repo, Some(&outcome.message), Some(&post_status.changes))?
        };

        let branch = outcome.branch.clone();
        let head_oid = Some(outcome.commit_oid.clone());
        let op_id = entry.op_id;
        Ok(Envelope::success(outcome)
            .with_op_id(op_id.clone())
            .with_workspace(repo.workspace)
            .with_branch(branch)
            .with_head_oid(head_oid)
            .with_recovery_command(format!("oz undo {op_id}")))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        let short = &d.commit_oid[..d.commit_oid.len().min(12)];
        println!(
            "[{} {short}] {} file(s) committed ({} dirty remaining, Change-Id: {})",
            d.branch.as_deref().unwrap_or("detached"),
            d.committed_paths.len(),
            d.remaining_dirty_count,
            d.change_id
        );
    })
}

pub(crate) fn handle_checkpoint(
    args: &CheckpointArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<CheckpointOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let status = compute_status(&repo)?;
        let actor = actor_from_mode(mode, &repo);
        let outcome = checkpoint(
            &repo,
            "oz checkpoint",
            args.message.as_deref(),
            actor,
            Some(&status.changes),
        )?;
        let op_id = outcome.op_id.clone();
        Ok(Envelope::success(outcome)
            .with_op_id(op_id.clone())
            .with_workspace(repo.workspace)
            .with_branch(status.branch)
            .with_head_oid(status.head_oid)
            .with_recovery_command(format!("oz undo {op_id}")))
    })();

    render_command_result(mode, res, |env| {
        if !args.quiet {
            let d = &env.data;
            println!(
                "checkpoint {} saved ({} dirty file(s), tree {})",
                d.op_id,
                d.dirty_file_count,
                &d.dirty_tree[..d.dirty_tree.len().min(12)]
            );
        }
    })
}

pub(crate) fn handle_undo(
    args: &UndoArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<UndoData>> {
        if args.op_id.is_some() && args.steps.is_some() {
            return Err(OzError::InvalidUsage {
                message: "cannot pass both `<op_id>` and `--steps` to `oz undo`".to_string(),
                recovery: vec!["Run `oz undo <op_id>` or `oz undo --steps <n>`.".to_string()],
            });
        }

        let repo = discover_and_recover_repo(directory, false)?;
        let actor = actor_from_mode(mode, &repo);
        let target = match (&args.op_id, args.steps) {
            (Some(id), None) => UndoTarget::OpId(id.clone()),
            (None, Some(n)) => UndoTarget::Steps(n),
            (None, None) => UndoTarget::Steps(1),
            (Some(_), Some(_)) => unreachable!(),
        };

        let t0 = Instant::now();
        let outcome = undo(&repo, target, actor)?;
        let engine_elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let post_repo = OzoneRepo::discover(directory).unwrap_or(repo);
        let op_id = outcome.op_id.clone();
        Ok(Envelope::success(UndoData {
            outcome,
            engine_elapsed_ms,
        })
        .with_op_id(op_id)
        .with_workspace(post_repo.workspace)
        .with_branch(post_repo.branch)
        .with_head_oid(post_repo.head_oid)
        .with_recovery_command("oz redo"))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        println!(
            "undid {} op(s) -> {} ({} file(s) restored, {:.2} ms)",
            d.outcome.undone_op_ids.len(),
            d.outcome.op_id,
            d.outcome.restored_files,
            d.engine_elapsed_ms
        );
    })
}

pub(crate) fn handle_redo(directory: Option<&Path>, mode: InteractionMode) -> ExitCode {
    let res = (|| -> Result<Envelope<RedoData>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let actor = actor_from_mode(mode, &repo);

        let t0 = Instant::now();
        let outcome = redo(&repo, actor)?;
        let engine_elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let post_repo = OzoneRepo::discover(directory).unwrap_or(repo);
        let op_id = outcome.op_id.clone();
        Ok(Envelope::success(RedoData {
            outcome,
            engine_elapsed_ms,
        })
        .with_op_id(op_id.clone())
        .with_workspace(post_repo.workspace)
        .with_branch(post_repo.branch)
        .with_head_oid(post_repo.head_oid)
        .with_recovery_command(format!("oz undo {op_id}")))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        println!(
            "redid {} -> {} ({} file(s) restored, {:.2} ms)",
            d.outcome.redone_undo_op_id,
            d.outcome.op_id,
            d.outcome.restored_files,
            d.engine_elapsed_ms
        );
    })
}

pub(crate) fn handle_setup(
    args: &SetupArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<SetupData>> {
        let repo = discover_and_recover_repo(directory, false)?;
        repo.layout.ensure_initialized()?;

        let probe = detect_filesystem(&repo.repo_root)?;
        let recommended_backend = select_backend(&probe, repo.config.backend);
        let vfsd_bin = oz_vfsd::find_oz_vfsd_bin()?;

        let systemd_unit_path = if args.systemd_user {
            #[cfg(target_os = "linux")]
            {
                Some(oz_vfsd::install_systemd_user_unit(
                    &vfsd_bin,
                    &repo.layout.git_common_dir,
                )?)
            }
            #[cfg(not(target_os = "linux"))]
            {
                return Err(OzError::InvalidUsage {
                    message: "--systemd-user is only supported on Linux".to_string(),
                    recovery: vec!["Run `oz setup` without `--systemd-user`.".to_string()],
                });
            }
        } else {
            None
        };

        let mut env = Envelope::success(SetupData {
            repo_root: repo.repo_root,
            git_common_dir: repo.layout.git_common_dir,
            ozone_dir: repo.layout.ozone_dir,
            recommended_backend,
            oz_vfsd_bin: vfsd_bin,
            systemd_unit_path,
        })
        .with_workspace(repo.workspace)
        .with_branch(repo.branch)
        .with_head_oid(repo.head_oid);

        if cfg!(target_os = "linux") && !args.systemd_user {
            env = env.with_recovery_command("oz setup --systemd-user");
        }
        Ok(env)
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        println!(
            "Ozone initialized at {} (backend: {}, oz-vfsd: {})",
            d.ozone_dir.display(),
            d.recommended_backend,
            d.oz_vfsd_bin.display()
        );
        if let Some(ref unit) = d.systemd_unit_path {
            println!("Installed systemd --user unit: {}", unit.display());
        }
    })
}
