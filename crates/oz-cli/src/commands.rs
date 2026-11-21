// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Primary subcommand implementations for `oz` and `git-ozone`.

use crate::recovery::discover_and_recover_repo;
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand, ValueEnum};
use oz_agent::{
    AgentHarness, AgentSetupOptions, AgentSetupReport, BlameOptions, BlameReport, CiLogEntry,
    CiLogsReport, McpServer, ProvenanceOptions, blame_file, record_commit_provenance,
    setup_agent_harnesses,
};
use oz_core::{
    BackendKind, CommitOptions, CommitOutcome, DiffReport, Envelope, EnvelopeError, ExitCode,
    FileChange, InteractionMode, OzError, OzoneRepo, Result, ScopePolicy, StatusReport,
    WarningEntry, compute_diff, compute_status, normalize_lexical, render_unified_diff,
};
use oz_cow::{
    GcWorkspacesOptions, RemoveWorkspaceOptions, SpawnWorkspaceOptions, detect_filesystem,
    gc_workspaces, list_workspaces, remove_workspace, select_backend, spawn_workspace,
};
use oz_github::{
    CiQueryOptions, CiReport, CloneOptions, CloneOutcome, DEFAULT_MAX_EXCERPT_LINES, FetchOptions,
    FetchOutcome, ForceLeaseSpec, HttpAuthScheme, PrCreateOptions, PrCreateOutcome, PrStackOptions,
    PrStackOutcome, PushOptions, PushOutcome, SyncOptions, SyncOutcome, clone_repo, fetch_remote,
    pr_create, pr_stack, push_refs_with_options, query_ci_status, sync_repo,
};
use oz_integrate::{
    AbortOutcome, CollisionSeverity, ContinueOutcome, IntegrateOptions, IntegrateOutcome,
    IntegrationMode, RadarOptions, RadarReport, ResolveOptions, ResolveOutcome, StackRebaseOptions,
    StackRebaseOutcome, StackStatusReport, TakeSide, abort_integration,
    continue_integration_with_verify, continue_stack_rebase, integrate, resolve_conflict,
    scan_radar, stack_rebase, stack_status,
};
use oz_oplog::{
    ActorInfo, ActorKind, CheckpointOutcome, MutationTransaction, RedoOutcome, UndoOutcome,
    UndoTarget, checkpoint, redo, undo,
};
use oz_warm::{BaselineSource, gc_baselines};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::str::FromStr;
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
    /// Commit message (a `Change-Id:` trailer is appended automatically if missing; optional with `--amend`).
    #[arg(short = 'm', long = "message", value_name = "MSG")]
    pub message: Option<String>,
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
    /// Sign commit using pure-Rust OpenSSH Ed25519 `SSHSIG` v1 (`namespace = "git"`).
    #[arg(short = 'S', long = "sign", conflicts_with = "no_sign")]
    pub sign: bool,
    /// Disable SSH commit signing even when enabled in `.ozone.toml` or Git config.
    #[arg(long = "no-sign")]
    pub no_sign: bool,
    /// Path or inline OpenSSH Ed25519 signing key override.
    #[arg(long = "signing-key", value_name = "KEY")]
    pub signing_key: Option<String>,
    /// Amend the current `HEAD` commit while preserving its `Change-Id:` trailer.
    #[arg(long = "amend")]
    pub amend: bool,
    /// Reuse `HEAD`'s existing commit message without modification when amending.
    #[arg(long = "no-edit")]
    pub no_edit: bool,
    /// Bypass `pre-commit` and `commit-msg` hooks.
    #[arg(short = 'n', long = "no-verify")]
    pub no_verify: bool,
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
    /// Enable the Windows Projected File System (`Client-ProjFS`) feature on Windows.
    #[arg(long)]
    pub enable_projfs: bool,
}

#[derive(Debug, Args)]
pub struct RadarArgs {
    /// Base branch or revision to project workspace edits onto (defaults to active repo branch or `main`).
    #[arg(long, value_name = "REV")]
    pub base: Option<String>,
    /// Exclude branch-diverged commits and scan only uncommitted working-tree edits.
    #[arg(long)]
    pub no_committed: bool,
}

#[derive(Debug, Args)]
pub struct IntegrateArgs {
    /// Workspace or branch names to integrate in order.
    pub sources: Vec<String>,
    /// Target branch to integrate into (defaults to `main`).
    #[arg(long, value_name = "BRANCH")]
    pub into: Option<String>,
    /// Verification shell command executed inside `../<repo>.oz/_integrate` before advancing the target branch.
    #[arg(long, value_name = "CMD")]
    pub verify: Option<String>,
    /// Override `--scope` and `--deny` path guardrails.
    #[arg(long)]
    pub allow_out_of_scope: bool,
    /// Override content guardrails (secrets, conflict markers, oversized blobs).
    #[arg(long)]
    pub force: bool,
    /// Resume an active integration conflict session after resolving `_integrate/`.
    #[arg(long = "continue")]
    pub continue_session: bool,
    /// Abort an active integration or stack rebase conflict session and remove `_integrate/`.
    #[arg(long)]
    pub abort: bool,
    /// Optional custom merge commit message.
    #[arg(short = 'm', long = "message", value_name = "MSG")]
    pub message: Option<String>,
}

/// Resolution side accepted by `oz resolve --take`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TakeSideArg {
    Ours,
    Theirs,
    Base,
}

impl From<TakeSideArg> for TakeSide {
    fn from(value: TakeSideArg) -> Self {
        match value {
            TakeSideArg::Ours => Self::Ours,
            TakeSideArg::Theirs => Self::Theirs,
            TakeSideArg::Base => Self::Base,
        }
    }
}

#[derive(Debug, Args)]
pub struct ResolveArgs {
    /// Conflict ID (e.g., `c1`) to resolve. Omit when `--all` is passed.
    pub conflict_id: Option<String>,
    /// Resolve all unresolved conflicts in the active `_integrate/` session.
    #[arg(long)]
    pub all: bool,
    /// Version to apply (`ours`, `theirs`, or `base`).
    #[arg(long, value_enum, value_name = "SIDE")]
    pub take: TakeSideArg,
    /// Automatically continue the paused integration or stack rebase after resolving.
    #[arg(long = "continue")]
    pub continue_session: bool,
}

/// Stacked-branch subcommands (`oz stack ...`).
#[derive(Debug, Subcommand)]
pub enum StackCommand {
    /// Rebase stacked branches in memory onto a base branch, preserving `Change-Id:` trailers.
    Rebase(StackRebaseArgs),
    /// Inspect stacked branches, `Change-Id:` trailers, and rebase readiness.
    Status(StackStatusArgs),
}

#[derive(Debug, Args)]
pub struct StackRebaseArgs {
    /// Stacked branch names to rebase (auto-discovers stacked branches if omitted).
    pub branches: Vec<String>,
    /// Target base branch to rebase onto (defaults to `main`).
    #[arg(long, value_name = "BRANCH")]
    pub onto: Option<String>,
    /// Resume an active stack rebase conflict session after resolving `_integrate/`.
    #[arg(long = "continue")]
    pub continue_session: bool,
    /// Abort an active stack rebase conflict session and remove `_integrate/`.
    #[arg(long)]
    pub abort: bool,
}

#[derive(Debug, Args)]
pub struct StackStatusArgs {
    /// Base branch to compare stacked branches against (defaults to `main`).
    #[arg(long, value_name = "BRANCH")]
    pub onto: Option<String>,
}

#[derive(Debug, Args)]
pub struct CloneArgs {
    /// Remote repository URL or local path.
    pub url: String,
    /// Optional target directory (defaults to repository name derived from `<url>`).
    pub directory: Option<PathBuf>,
    /// Check out `<BRANCH>` instead of the remote's default branch.
    #[arg(short = 'b', long = "branch", value_name = "BRANCH")]
    pub branch: Option<String>,
    /// Partial-clone object filter specification (e.g., `blob:none`).
    #[arg(long = "filter", value_name = "SPEC")]
    pub filter: Option<String>,
    /// Remote name to configure (defaults to `origin`).
    #[arg(short = 'o', long = "origin", value_name = "NAME")]
    pub origin: Option<String>,
    /// Clone as a bare Git repository.
    #[arg(long = "bare")]
    pub bare: bool,
}

#[derive(Debug, Args)]
pub struct FetchArgs {
    /// Remote name or URL (defaults to `origin`).
    pub remote: Option<String>,
    /// Optional explicit refspecs to fetch.
    pub refspecs: Vec<String>,
    /// Prune remote-tracking branches that no longer exist on the remote.
    #[arg(short = 'p', long = "prune")]
    pub prune: bool,
    /// Partial-clone object filter specification (e.g., `blob:none`).
    #[arg(long = "filter", value_name = "SPEC")]
    pub filter: Option<String>,
    /// Fetch remote tags (`refs/tags/*`).
    #[arg(short = 't', long = "tags")]
    pub tags: bool,
}

#[derive(Debug, Args)]
pub struct PushArgs {
    /// Remote name or URL (defaults to `origin`).
    pub remote: Option<String>,
    /// Optional refspecs to push (defaults to active branch).
    pub refspecs: Vec<String>,
    /// Negotiate atomic multi-ref push (abort all updates if any ref fails).
    #[arg(long = "atomic")]
    pub atomic: bool,
    /// Safe force push verifying remote tips match expected OIDs (`--force-with-lease[=<ref>[:<oid>]]`).
    #[arg(
        long = "force-with-lease",
        value_name = "SPEC",
        num_args = 0..=1,
        default_missing_value = "",
        require_equals = true,
        action = clap::ArgAction::Append
    )]
    pub force_with_lease: Vec<String>,
    /// Force non-fast-forward ref updates unconditionally.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
    /// Configure upstream tracking reference (`branch.<name>.remote` and `merge`).
    #[arg(short = 'u', long = "set-upstream")]
    pub set_upstream: bool,
    /// Delete the specified remote references.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,
    /// Bypass `pre-push` verification hooks.
    #[arg(long = "no-verify")]
    pub no_verify: bool,
}

/// Pull request subcommands (`oz pr ...`).
#[derive(Debug, Subcommand)]
pub enum PrCommand {
    /// Create or idempotently update a GitHub Pull Request preserving `Change-Id:`.
    Create(PrCreateArgs),
    /// Push stacked branches atomically and create/update cross-linked stacked PRs.
    Stack(PrStackArgs),
}

#[derive(Debug, Args)]
pub struct PrCreateArgs {
    /// Pull request title (inferred from commit subject when omitted).
    #[arg(short = 't', long = "title", value_name = "TITLE")]
    pub title: Option<String>,
    /// Pull request markdown body (inferred from commit body + `Change-Id:` when omitted).
    #[arg(short = 'b', long = "body", value_name = "BODY")]
    pub body: Option<String>,
    /// Base target branch on GitHub (defaults to `main`).
    #[arg(short = 'B', long = "base", value_name = "BRANCH")]
    pub base: Option<String>,
    /// Head source branch (defaults to active workspace branch).
    #[arg(short = 'H', long = "head", value_name = "BRANCH")]
    pub head: Option<String>,
    /// Open pull request as a draft.
    #[arg(short = 'd', long = "draft")]
    pub draft: bool,
    /// Skip pushing head branch before creating/updating the pull request.
    #[arg(long = "no-push")]
    pub no_push: bool,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct PrStackArgs {
    /// Optional explicit stack branch names (auto-discovered from base when omitted).
    pub branches: Vec<String>,
    /// Root base branch of the stack (defaults to `main`).
    #[arg(short = 'B', long = "base", value_name = "BRANCH")]
    pub base: Option<String>,
    /// Open newly created stack pull requests as drafts.
    #[arg(short = 'd', long = "draft")]
    pub draft: bool,
    /// Skip atomic push of stack branches before creating/updating pull requests.
    #[arg(long = "no-push")]
    pub no_push: bool,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
    /// Base trunk branch (defaults to `main`).
    #[arg(long = "base", value_name = "BRANCH")]
    pub base: Option<String>,
    /// Skip fetching from remote before syncing.
    #[arg(long = "no-fetch")]
    pub no_fetch: bool,
    /// Skip rebasing downstream local stacked branches onto updated base.
    #[arg(long = "no-rebase")]
    pub no_rebase: bool,
    /// Skip merged CoW workspace & checkpoint garbage collection.
    #[arg(long = "no-gc")]
    pub no_gc: bool,
    /// Skip warm build baseline refresh.
    #[arg(long = "no-warm")]
    pub no_warm: bool,
    /// Push rebased downstream stacked branches back to remote with `--force-with-lease`.
    #[arg(long = "push")]
    pub push: bool,
}

#[derive(Debug, Args)]
pub struct CiArgs {
    /// Target Git revision, branch, or commit SHA to inspect.
    #[arg(long = "ref", value_name = "REV")]
    pub target_ref: Option<String>,
    /// Target branch name to inspect.
    #[arg(long = "branch", value_name = "BRANCH")]
    pub branch: Option<String>,
    /// Pull request number to inspect.
    #[arg(long = "pr", value_name = "NUMBER")]
    pub pr: Option<u64>,
    /// Maximum lines per failing check log excerpt (default: 40).
    #[arg(
        long = "max-excerpt-lines",
        default_value_t = DEFAULT_MAX_EXCERPT_LINES,
        value_name = "N"
    )]
    pub max_excerpt_lines: usize,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

/// Coding agent ecosystem subcommands (`oz agent ...`).
#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Install idempotent Ozone skills/rules and `oz checkpoint --quiet` hooks for coding agents.
    Setup(AgentSetupArgs),
}

#[derive(Debug, Args)]
pub struct AgentSetupArgs {
    /// Target agent harness(es): `claude-code`, `cursor`, `codex-cli`, `windsurf`, `antigravity`.
    #[arg(
        long = "harness",
        value_name = "HARNESS",
        num_args = 1..,
        value_delimiter = ',',
        action = clap::ArgAction::Append
    )]
    pub harnesses: Vec<String>,
    /// Configure all 5 supported coding agent harnesses.
    #[arg(long = "all")]
    pub all: bool,
    /// Check / dry-run mode: compute planned file changes without writing to disk.
    #[arg(long = "check", visible_alias = "dry-run")]
    pub check: bool,
    /// Skip installing `PostToolUse` / edit checkpoint hook configurations.
    #[arg(long = "no-hooks")]
    pub no_hooks: bool,
}

#[derive(Debug, Default, Args)]
pub struct McpArgs {
    /// Optional explicit MCP session identifier recorded in `refs/notes/ai`.
    #[arg(long = "session-id", value_name = "ID")]
    pub session_id: Option<String>,
}

#[derive(Debug, Args)]
pub struct BlameArgs {
    /// Repository-relative or filesystem path to the file to blame.
    pub path: PathBuf,
    /// Target commit, branch, or revision to blame from (defaults to `HEAD`).
    #[arg(long = "rev", visible_alias = "revision", value_name = "REV")]
    pub rev: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    projfs_enabled: Option<bool>,
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

pub(crate) fn write_stdout_line(line: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    if let Err(err) = out.write_all(line.as_bytes()).and_then(|()| out.write_all(b"\n")).and_then(|()| out.flush())
        && err.kind() != std::io::ErrorKind::BrokenPipe
    {
        eprintln!("error: failed writing stdout: {err}");
    }
}

pub(crate) fn print_json_envelope<T: Serialize>(env: &Envelope<T>) {
    match env.to_json_pretty() {
        Ok(s) => write_stdout_line(&s),
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

pub(crate) fn attach_out_of_scope_warnings(
    repo: &OzoneRepo,
    mut env: Envelope<StatusReport>,
) -> Envelope<StatusReport> {
    let policy = ScopePolicy::from_repo(repo).ok();
    let warnings: Vec<WarningEntry> = env
        .data
        .changes
        .iter()
        .filter(|change| change.out_of_scope)
        .map(|change| {
            let message = policy
                .as_ref()
                .and_then(|p| p.violation_reason(&change.path))
                .unwrap_or_else(|| {
                    format!(
                        "'{}' is outside workspace allowed scope or matches deny pattern",
                        change.path
                    )
                });
            WarningEntry::Structured {
                code: "out_of_scope".to_string(),
                message,
                path: Some(change.path.clone()),
            }
        })
        .collect();
    for w in warnings {
        env = env.with_warning(w);
    }
    env
}

pub(crate) fn handle_status(directory: Option<&Path>, mode: InteractionMode) -> ExitCode {
    let res = (|| -> Result<Envelope<_>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let report = compute_status(&repo)?;
        let workspace = report.workspace.clone();
        let branch = report.branch.clone();
        let head_oid = report.head_oid.clone();
        let env = Envelope::success(report)
            .with_workspace(workspace)
            .with_branch(branch)
            .with_head_oid(head_oid);
        Ok(attach_out_of_scope_warnings(&repo, env))
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
        if args.sign && args.no_sign {
            return Err(OzError::InvalidUsage {
                message: "cannot pass both `--sign` and `--no-sign` to `oz commit`".to_string(),
                recovery: vec!["oz commit --sign -m \"...\"".to_string()],
            });
        }
        if !args.amend && args.no_edit {
            return Err(OzError::InvalidUsage {
                message: "`--no-edit` is only valid alongside `--amend`".to_string(),
                recovery: vec!["oz commit --amend --no-edit".to_string()],
            });
        }
        let message = match (&args.message, args.amend) {
            (Some(msg), _) => msg.clone(),
            (None, true) => String::new(),
            (None, false) => {
                return Err(OzError::InvalidUsage {
                    message: "missing commit message; pass `-m \"...\"`".to_string(),
                    recovery: vec!["oz commit -m \"feat: ...\"".to_string()],
                });
            }
        };

        let sign = if args.no_sign {
            Some(false)
        } else if args.sign || args.signing_key.is_some() {
            Some(true)
        } else {
            None
        };

        let repo = discover_and_recover_repo(directory, false)?;
        let pre_status = compute_status(&repo)?;
        let actor = actor_from_mode(mode, &repo);
        let cmd_label = if args.amend {
            "oz commit --amend"
        } else {
            "oz commit"
        };
        let tx = MutationTransaction::begin(
            &repo,
            cmd_label,
            actor,
            Some(&pre_status.changes),
        )?;

        let opts = CommitOptions {
            message,
            pick: args.pick.clone(),
            exclude: args.exclude.clone(),
            force: args.force,
            allow_out_of_scope: args.allow_out_of_scope,
            allow_empty: args.allow_empty,
            author_name: None,
            author_email: None,
            sign,
            signing_key: args.signing_key.clone(),
            amend: args.amend,
            no_verify: args.no_verify,
            config_overrides: Vec::new(),
        };
        let outcome = oz_core::commit_changes(&repo, &opts)?;

        let entry = if outcome.remaining_dirty_count == 0 {
            tx.finish_with_message(&repo, Some(&outcome.message), Some(&[]))?
        } else {
            let post_status = compute_status(&repo)?;
            tx.finish_with_message(&repo, Some(&outcome.message), Some(&post_status.changes))?
        };

        // Record structured AI provenance into `refs/notes/ai` when `--agent` is active or an
        // active coding agent session is detected in environment variables, keeping the commit
        // message itself completely clean.
        let explicit_agent_disabled = !mode.agent
            && std::env::var("OZ_AGENT").is_ok_and(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            });
        if mode.agent || !explicit_agent_disabled {
            let prov_opts = ProvenanceOptions {
                force_agent: mode.agent,
                inspect_env: !explicit_agent_disabled,
                ..ProvenanceOptions::default()
            };
            let _ = record_commit_provenance(&repo, &outcome.commit_oid, &prov_opts)?;
        }

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
        let mut extras = Vec::new();
        if d.amended {
            extras.push("amended".to_string());
        }
        if d.signed {
            if let Some(ref fp) = d.signature_fingerprint {
                extras.push(format!("signed {fp}"));
            } else {
                extras.push("signed".to_string());
            }
        }
        let extra_suffix = if extras.is_empty() {
            String::new()
        } else {
            format!(" [{}]", extras.join(", "))
        };
        println!(
            "[{} {short}] {} file(s) committed ({} dirty remaining, Change-Id: {}){extra_suffix}",
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

        let projfs_enabled = if args.enable_projfs {
            let newly_enabled = oz_vfsd::enable_windows_projfs()?;
            Some(newly_enabled || oz_vfsd::is_windows_projfs_enabled())
        } else {
            None
        };

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
            projfs_enabled,
        })
        .with_workspace(repo.workspace)
        .with_branch(repo.branch)
        .with_head_oid(repo.head_oid);

        if cfg!(target_os = "linux") && !args.systemd_user {
            env = env.with_recovery_command("oz setup --systemd-user");
        }
        if cfg!(target_os = "windows") && !args.enable_projfs && !oz_vfsd::is_windows_projfs_enabled() {
            env = env.with_recovery_command("oz setup --enable-projfs");
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
        if let Some(enabled) = d.projfs_enabled {
            println!("Windows ProjFS (Client-ProjFS) enabled: {enabled}");
        }
    })
}

pub(crate) fn wrap_integrate_envelope(
    outcome: IntegrateOutcome,
    workspace: Option<String>,
) -> Envelope<IntegrateOutcome> {
    let branch = outcome.into_branch.clone();
    let head_oid = outcome.commit_oid.clone();
    let op_id = outcome.op_id.clone();
    let mut env = Envelope::success(outcome)
        .with_workspace(workspace)
        .with_branch(Some(branch))
        .with_head_oid(Some(head_oid));
    if let Some(id) = op_id {
        env = env
            .with_op_id(id.clone())
            .with_recovery_command(format!("oz undo {id}"));
    }
    env
}

pub(crate) fn wrap_stack_rebase_envelope(
    outcome: StackRebaseOutcome,
    post_repo: &OzoneRepo,
) -> Envelope<StackRebaseOutcome> {
    let op_id = outcome.op_id.clone();
    let mut env = Envelope::success(outcome)
        .with_workspace(post_repo.workspace.clone())
        .with_branch(post_repo.branch.clone())
        .with_head_oid(post_repo.head_oid.clone());
    if let Some(id) = op_id {
        env = env
            .with_op_id(id.clone())
            .with_recovery_command(format!("oz undo {id}"));
    }
    env
}

pub(crate) fn print_integrate_human(outcome: &IntegrateOutcome) {
    if outcome.merged_sources.is_empty() {
        println!(
            "Already up to date on {} ({})",
            outcome.into_branch,
            &outcome.commit_oid[..outcome.commit_oid.len().min(12)]
        );
    } else {
        println!(
            "Integrated [{}] into {} ({})",
            outcome.merged_sources.join(", "),
            outcome.into_branch,
            &outcome.commit_oid[..outcome.commit_oid.len().min(12)]
        );
    }
}

pub(crate) fn print_stack_rebase_human(outcome: &StackRebaseOutcome) {
    println!(
        "Rebased {} branch(es) onto {} ({} commit(s) replayed)",
        outcome.branches.len(),
        outcome.onto_branch,
        outcome.rebased_commits.len()
    );
    for b in &outcome.branches {
        println!(
            "  {}  {} -> {} ({} commit(s))",
            b.branch,
            &b.previous_oid[..b.previous_oid.len().min(12)],
            &b.new_oid[..b.new_oid.len().min(12)],
            b.commit_count
        );
    }
}

pub(crate) fn print_abort_human(outcome: &AbortOutcome) {
    if outcome.aborted {
        println!("Aborted active conflict session and cleaned _integrate sandbox");
    } else {
        println!("No active conflict session to abort");
    }
}

pub(crate) fn handle_radar(
    args: &RadarArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<RadarReport>> {
        let repo = discover_and_recover_repo(directory, true)?;
        let opts = RadarOptions {
            base_branch: args.base.clone(),
            include_committed: !args.no_committed,
            workspaces: Vec::new(),
        };
        let report = scan_radar(&repo, &opts)?;
        Ok(Envelope::success(report)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        println!(
            "Radar base={} ({} workspace(s) scanned)",
            d.base_branch,
            d.scanned_workspaces.len()
        );
        if d.collisions.is_empty() {
            println!("no collisions detected");
            return;
        }
        for item in &d.collisions {
            let sev = match item.severity {
                CollisionSeverity::OverlappingHunks => "overlapping_hunks",
                CollisionSeverity::SameFileDifferentLines => "same_file_different_lines",
                CollisionSeverity::OutOfScope => "out_of_scope",
            };
            let spans: Vec<String> = item
                .workspaces
                .iter()
                .map(|w| format!("{}:{}", w.ws, w.lines))
                .collect();
            println!(
                "  [{sev}] {}  ({}) -> {}",
                item.path,
                spans.join(", "),
                item.suggested_action
            );
        }
    })
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum IntegrateCliData {
    Integrated(IntegrateOutcome),
    Aborted(AbortOutcome),
}

pub(crate) fn handle_integrate(
    args: &IntegrateArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<IntegrateCliData>> {
        if args.continue_session && args.abort {
            return Err(OzError::InvalidUsage {
                message: "cannot pass both `--continue` and `--abort` to `oz integrate`"
                    .to_string(),
                recovery: vec![
                    "oz integrate --continue".to_string(),
                    "oz integrate --abort".to_string(),
                ],
            });
        }
        if (args.continue_session || args.abort) && !args.sources.is_empty() {
            return Err(OzError::InvalidUsage {
                message: "cannot pass source branches alongside `--continue` or `--abort`"
                    .to_string(),
                recovery: vec![
                    "oz integrate --continue".to_string(),
                    "oz integrate --abort".to_string(),
                ],
            });
        }

        let repo = discover_and_recover_repo(directory, true)?;
        let actor = actor_from_mode(mode, &repo);

        if args.abort {
            let outcome = abort_integration(&repo)?;
            return Ok(Envelope::success(IntegrateCliData::Aborted(outcome))
                .with_workspace(repo.workspace)
                .with_branch(repo.branch)
                .with_head_oid(repo.head_oid));
        }

        if args.continue_session {
            let outcome =
                continue_integration_with_verify(&repo, args.verify.as_deref(), actor)?;
            let wrapped = wrap_integrate_envelope(outcome, repo.workspace);
            return Ok(map_envelope_data(wrapped, IntegrateCliData::Integrated));
        }

        if args.sources.is_empty() {
            return Err(OzError::InvalidUsage {
                message: "missing source workspaces or branches for `oz integrate`".to_string(),
                recovery: vec![
                    "oz integrate <ws-or-branch>... --into main".to_string(),
                    "oz integrate --continue".to_string(),
                    "oz integrate --abort".to_string(),
                ],
            });
        }

        let opts = IntegrateOptions {
            sources: args.sources.clone(),
            into: args.into.clone(),
            verify_cmd: args.verify.clone(),
            allow_out_of_scope: args.allow_out_of_scope,
            force: args.force,
            message: args.message.clone(),
        };
        let outcome = integrate(&repo, &opts, actor)?;
        let wrapped = wrap_integrate_envelope(outcome, repo.workspace);
        Ok(map_envelope_data(wrapped, IntegrateCliData::Integrated))
    })();

    render_command_result(mode, res, |env| match &env.data {
        IntegrateCliData::Integrated(out) => print_integrate_human(out),
        IntegrateCliData::Aborted(out) => print_abort_human(out),
    })
}

pub(crate) fn handle_resolve(
    args: &ResolveArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<ResolveOutcome>> {
        if args.all && args.conflict_id.is_some() {
            return Err(OzError::InvalidUsage {
                message: "cannot pass both `<conflict_id>` and `--all` to `oz resolve`".to_string(),
                recovery: vec![
                    "oz resolve c1 --take ours".to_string(),
                    "oz resolve --all --take ours".to_string(),
                ],
            });
        }

        let repo = discover_and_recover_repo(directory, false)?;
        let actor = actor_from_mode(mode, &repo);
        let opts = ResolveOptions {
            conflict_id: args.conflict_id.clone(),
            all: args.all,
            take: args.take.into(),
            auto_continue: args.continue_session,
        };
        let outcome = resolve_conflict(&repo, &opts, actor)?;
        let post_repo = OzoneRepo::discover(directory).unwrap_or(repo);

        let mut env = Envelope::success(outcome.clone())
            .with_workspace(post_repo.workspace)
            .with_branch(post_repo.branch)
            .with_head_oid(post_repo.head_oid);

        match &outcome.continued {
            Some(ContinueOutcome::Integrate(int_out)) => {
                env = env
                    .with_branch(Some(int_out.into_branch.clone()))
                    .with_head_oid(Some(int_out.commit_oid.clone()));
                if let Some(ref op_id) = int_out.op_id {
                    env = env
                        .with_op_id(op_id.clone())
                        .with_recovery_command(format!("oz undo {op_id}"));
                }
            }
            Some(ContinueOutcome::StackRebase(sr_out)) => {
                if let Some(ref op_id) = sr_out.op_id {
                    env = env
                        .with_op_id(op_id.clone())
                        .with_recovery_command(format!("oz undo {op_id}"));
                }
            }
            None => {
                if outcome.remaining_unresolved.is_empty() {
                    let cont_cmd = match outcome.mode {
                        IntegrationMode::Integrate => "oz integrate --continue",
                        IntegrationMode::StackRebase => "oz stack rebase --continue",
                    };
                    env = env.with_recovery_command(cont_cmd);
                } else {
                    env = env.with_recovery_command(format!(
                        "oz resolve {} --take ours",
                        outcome.remaining_unresolved[0]
                    ));
                }
            }
        }

        Ok(env)
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        println!(
            "Resolved [{}] ({} remaining unresolved)",
            d.resolved_ids.join(", "),
            d.remaining_unresolved.len()
        );
        match &d.continued {
            Some(ContinueOutcome::Integrate(out)) => print_integrate_human(out),
            Some(ContinueOutcome::StackRebase(out)) => print_stack_rebase_human(out),
            None => {}
        }
    })
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum StackRebaseCliData {
    Rebased(StackRebaseOutcome),
    Aborted(AbortOutcome),
}

pub(crate) fn handle_stack(
    cmd: StackCommand,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    match cmd {
        StackCommand::Rebase(args) => handle_stack_rebase(&args, directory, mode),
        StackCommand::Status(args) => handle_stack_status(&args, directory, mode),
    }
}

fn handle_stack_rebase(
    args: &StackRebaseArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<StackRebaseCliData>> {
        if args.continue_session && args.abort {
            return Err(OzError::InvalidUsage {
                message: "cannot pass both `--continue` and `--abort` to `oz stack rebase`"
                    .to_string(),
                recovery: vec![
                    "oz stack rebase --continue".to_string(),
                    "oz stack rebase --abort".to_string(),
                ],
            });
        }
        if (args.continue_session || args.abort)
            && (!args.branches.is_empty() || args.onto.is_some())
        {
            return Err(OzError::InvalidUsage {
                message: "cannot pass branches or `--onto` alongside `--continue` or `--abort`"
                    .to_string(),
                recovery: vec![
                    "oz stack rebase --continue".to_string(),
                    "oz stack rebase --abort".to_string(),
                ],
            });
        }

        let repo = discover_and_recover_repo(directory, true)?;
        let actor = actor_from_mode(mode, &repo);

        if args.abort {
            let outcome = abort_integration(&repo)?;
            return Ok(Envelope::success(StackRebaseCliData::Aborted(outcome))
                .with_workspace(repo.workspace)
                .with_branch(repo.branch)
                .with_head_oid(repo.head_oid));
        }

        let outcome = if args.continue_session {
            continue_stack_rebase(&repo, actor)?
        } else {
            stack_rebase(
                &repo,
                &StackRebaseOptions {
                    branches: args.branches.clone(),
                    onto: args.onto.clone(),
                },
                actor,
            )?
        };
        let post_repo = OzoneRepo::discover(directory).unwrap_or(repo);
        let wrapped = wrap_stack_rebase_envelope(outcome, &post_repo);
        Ok(map_envelope_data(wrapped, StackRebaseCliData::Rebased))
    })();

    render_command_result(mode, res, |env| match &env.data {
        StackRebaseCliData::Rebased(out) => print_stack_rebase_human(out),
        StackRebaseCliData::Aborted(out) => print_abort_human(out),
    })
}

fn handle_stack_status(
    args: &StackStatusArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<StackStatusReport>> {
        let repo = discover_and_recover_repo(directory, true)?;
        let report = stack_status(&repo, args.onto.as_deref())?;
        Ok(Envelope::success(report)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        println!("Stack onto {} ({} branch(es))", d.onto_branch, d.branches.len());
        if d.branches.is_empty() {
            println!("no stacked branches ahead of {}", d.onto_branch);
            return;
        }
        for b in &d.branches {
            let parent = b.parent_branch.as_deref().unwrap_or(&d.onto_branch);
            let status_flag = if b.needs_rebase { "needs-rebase" } else { "up-to-date" };
            println!(
                "  {} (parent: {}, ahead: {}, behind: {}, {})",
                b.branch, parent, b.ahead_count, b.behind_onto_count, status_flag
            );
            for c in &b.commits {
                let cid = c.change_id.as_deref().unwrap_or("-");
                println!(
                    "    {} {} [Change-Id: {}]",
                    &c.oid[..c.oid.len().min(8)],
                    c.subject,
                    cid
                );
            }
        }
    })
}

fn map_envelope_data<A: Serialize, B: Serialize, F: FnOnce(A) -> B>(
    env: Envelope<A>,
    map_fn: F,
) -> Envelope<B> {
    Envelope {
        schema: env.schema,
        ok: env.ok,
        op_id: env.op_id,
        workspace: env.workspace,
        branch: env.branch,
        head_oid: env.head_oid,
        data: map_fn(env.data),
        warnings: env.warnings,
        conflicts: env.conflicts,
        recovery_commands: env.recovery_commands,
        error: env.error,
    }
}

fn derive_clone_destination_name(raw_url: &str, bare: bool) -> PathBuf {
    let trimmed = raw_url.trim().trim_end_matches(['/', '\\']);
    let without_git = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let last = without_git
        .rsplit(['/', '\\', ':'])
        .find(|seg| !seg.is_empty())
        .unwrap_or("repo");
    if bare && !last.ends_with(".git") {
        PathBuf::from(format!("{last}.git"))
    } else {
        PathBuf::from(last)
    }
}

pub(crate) fn handle_clone(
    args: &CloneArgs,
    base_directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<CloneOutcome>> {
        let anchor_dir = match base_directory {
            Some(dir) => dir.to_path_buf(),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };

        let raw_dest = args
            .directory
            .clone()
            .unwrap_or_else(|| derive_clone_destination_name(&args.url, args.bare));
        let destination = if raw_dest.is_absolute() {
            normalize_lexical(&raw_dest)
        } else {
            normalize_lexical(&anchor_dir.join(&raw_dest))
        };

        let effective_url = if base_directory.is_some()
            && !args.url.contains("://")
            && !args.url.contains('@')
            && !Path::new(&args.url).is_absolute()
        {
            let candidate = anchor_dir.join(&args.url);
            if candidate.exists() {
                candidate.to_string_lossy().into_owned()
            } else {
                args.url.clone()
            }
        } else {
            args.url.clone()
        };

        let opts = CloneOptions {
            url: effective_url,
            destination,
            branch: args.branch.clone(),
            filter: args.filter.clone(),
            bare: args.bare,
            remote_name: args.origin.clone(),
            checkout: !args.bare,
            auth_token: None,
            auth_scheme: HttpAuthScheme::BasicToken,
        };
        let actor = ActorInfo {
            kind: if mode.agent {
                ActorKind::Agent
            } else {
                ActorKind::Human
            },
            client: Some("oz-cli".to_string()),
            workspace: None,
        };

        let outcome = clone_repo(&opts, actor)?;
        let branch = outcome.branch.clone();
        let head_oid = outcome.head_oid.clone();
        let op_id = outcome.op_id.clone();
        let warnings = outcome.warnings.clone();
        let dest_display = outcome.destination.display().to_string();

        let mut env = Envelope::success(outcome)
            .with_branch(branch)
            .with_head_oid(head_oid);
        if !args.bare {
            env = env.with_recovery_command(format!("oz -C {dest_display} status"));
        }
        if let Some(id) = op_id {
            env = env.with_op_id(id);
        }
        for w in warnings {
            env = env.with_warning(w);
        }
        Ok(env)
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        let filter_note = d
            .filter
            .as_deref()
            .map(|f| format!(", filter={f}"))
            .unwrap_or_default();
        println!(
            "Cloned '{}' into {} (branch: {}, files: {}{filter_note})",
            d.remote_url,
            d.destination.display(),
            d.branch.as_deref().unwrap_or("detached"),
            d.checked_out_files
        );
    })
}

pub(crate) fn handle_fetch(
    args: &FetchArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<FetchOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let actor = actor_from_mode(mode, &repo);
        let opts = FetchOptions {
            remote: args.remote.clone(),
            refspecs: args.refspecs.clone(),
            prune: args.prune,
            filter: args.filter.clone(),
            tags: args.tags,
            auth_token: None,
            auth_scheme: HttpAuthScheme::BasicToken,
        };

        let outcome = fetch_remote(&repo, &opts, actor)?;
        let op_id = outcome.op_id.clone();
        let warnings = outcome.warnings.clone();

        let mut env = Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid);
        if let Some(id) = op_id {
            env = env
                .with_op_id(id.clone())
                .with_recovery_command(format!("oz undo {id}"));
        }
        for w in warnings {
            env = env.with_warning(w);
        }
        Ok(env)
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        if d.updated_refs.is_empty() && d.pruned_refs.is_empty() {
            println!("Fetched '{}' ({}): already up to date", d.remote, d.remote_url);
        } else {
            println!(
                "Fetched '{}' ({}): {} updated, {} pruned",
                d.remote,
                d.remote_url,
                d.updated_refs.len(),
                d.pruned_refs.len()
            );
            for u in &d.updated_refs {
                let short_new = &u.new_oid[..u.new_oid.len().min(10)];
                println!("  {} -> {} ({short_new})", u.remote_ref, u.local_ref);
            }
            for p in &d.pruned_refs {
                println!("  - [pruned] {p}");
            }
        }
    })
}

pub(crate) fn handle_push(
    args: &PushArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<PushOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let actor = actor_from_mode(mode, &repo);

        let mut use_default_lease = false;
        let mut lease_specs = Vec::new();
        for raw_spec in &args.force_with_lease {
            if raw_spec.trim().is_empty() {
                use_default_lease = true;
            } else {
                lease_specs.push(ForceLeaseSpec::parse(raw_spec));
            }
        }

        let opts = PushOptions {
            remote: args.remote.clone(),
            refspecs: args.refspecs.clone(),
            atomic: args.atomic,
            force: args.force,
            force_with_lease: use_default_lease,
            lease_specs,
            delete: args.delete,
            set_upstream: args.set_upstream,
            auth_token: None,
            auth_scheme: HttpAuthScheme::BasicToken,
        };

        let outcome = push_refs_with_options(&repo, &opts, args.no_verify, &[], actor)?;
        let op_id = outcome.op_id.clone();

        let mut env = Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid);
        if let Some(id) = op_id {
            env = env
                .with_op_id(id.clone())
                .with_recovery_command(format!("oz undo {id}"));
        }
        Ok(env)
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        println!(
            "Pushed {} ref(s) to '{}' ({} objects, {} bytes)",
            d.pushed_refs.len(),
            d.remote,
            d.pack_objects_sent,
            d.pack_bytes_sent
        );
        for r in &d.pushed_refs {
            if r.deleted {
                println!("  - [deleted] {}", r.remote_ref);
            } else {
                let new_short = r
                    .new_oid
                    .as_deref()
                    .map(|s| &s[..s.len().min(10)])
                    .unwrap_or("-");
                println!(
                    "  {} -> {} ({new_short}{})",
                    r.local_ref.as_deref().unwrap_or("HEAD"),
                    r.remote_ref,
                    if r.forced { ", forced" } else { "" }
                );
            }
        }
    })
}

pub(crate) fn handle_pr(
    cmd: PrCommand,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    match cmd {
        PrCommand::Create(args) => handle_pr_create(&args, directory, mode),
        PrCommand::Stack(args) => handle_pr_stack(&args, directory, mode),
    }
}

fn handle_pr_create(
    args: &PrCreateArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<PrCreateOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let actor = actor_from_mode(mode, &repo);
        let opts = PrCreateOptions {
            title: args.title.clone(),
            body: args.body.clone(),
            base: args.base.clone(),
            head: args.head.clone(),
            draft: args.draft,
            no_push: args.no_push,
            remote: args.remote.clone(),
            auth_token: None,
            api_url: None,
            git_auth_scheme: HttpAuthScheme::BasicToken,
        };

        let outcome = pr_create(&repo, &opts, actor)?;
        let head_branch = outcome.head_branch.clone();
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(Some(head_branch))
            .with_head_oid(repo.head_oid)
            .with_recovery_command("oz ci --json"))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        let action = if d.created { "Created" } else { "Updated" };
        println!(
            "{action} PR #{}: {} ({} -> {}) -> {}",
            d.pull_request.number,
            d.pull_request.title,
            d.head_branch,
            d.base_branch,
            d.pull_request.html_url
        );
    })
}

fn handle_pr_stack(
    args: &PrStackArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<PrStackOutcome>> {
        let repo = discover_and_recover_repo(directory, true)?;
        let actor = actor_from_mode(mode, &repo);
        let opts = PrStackOptions {
            base: args.base.clone(),
            branches: args.branches.clone(),
            draft: args.draft,
            no_push: args.no_push,
            remote: args.remote.clone(),
            auth_token: None,
            api_url: None,
            git_auth_scheme: HttpAuthScheme::BasicToken,
        };

        let outcome = pr_stack(&repo, &opts, actor)?;
        let op_id = outcome.atomic_push_op_id.clone();

        let mut env = Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid);
        if let Some(id) = op_id {
            env = env
                .with_op_id(id.clone())
                .with_recovery_command(format!("oz undo {id}"));
        }
        env = env.with_recovery_command("oz ci --json");
        Ok(env)
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        println!(
            "Synced PR stack onto '{}' ({} PR(s))",
            d.root_base,
            d.items.len()
        );
        for item in &d.items {
            let verb = if item.created { "created" } else { "updated" };
            println!(
                "  #{} [{}] {} ({} -> {}) -> {}",
                item.number, verb, item.title, item.head_branch, item.base_branch, item.html_url
            );
        }
    })
}

pub(crate) fn handle_sync(
    args: &SyncArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<SyncOutcome>> {
        let _ = (args.no_gc, args.no_warm);
        let repo = discover_and_recover_repo(directory, true)?;
        let main_root = repo.repo_root.clone();
        let actor = actor_from_mode(mode, &repo);
        let opts = SyncOptions {
            remote: args.remote.clone(),
            base_branch: args.base.clone(),
            no_fetch: args.no_fetch,
            no_rebase: args.no_rebase,
            push_rebased: args.push,
            auth_token: None,
            api_url: None,
            git_auth_scheme: HttpAuthScheme::BasicToken,
        };

        let outcome = sync_repo(&repo, &opts, actor)?;
        let post_repo = OzoneRepo::discover(directory)
            .or_else(|_| OzoneRepo::discover(Some(&main_root)))
            .unwrap_or(repo);

        let op_id = outcome.op_id.clone();
        let warnings = outcome.warnings.clone();
        let mut env = Envelope::success(outcome)
            .with_workspace(post_repo.workspace)
            .with_branch(post_repo.branch)
            .with_head_oid(post_repo.head_oid);
        if let Some(id) = op_id {
            env = env
                .with_op_id(id.clone())
                .with_recovery_command(format!("oz undo {id}"));
        }
        for w in warnings {
            env = env.with_warning(w);
        }
        Ok(env)
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        println!(
            "Synced '{}' against '{}': fast_forwarded={}, merged={}, retargeted_prs={}, rebased={}, removed_ws={}",
            d.base_branch,
            d.remote,
            d.fast_forwarded,
            d.merged_branches.len(),
            d.retargeted_prs.len(),
            d.rebased_branches.len(),
            d.removed_workspaces.len()
        );
    })
}

pub(crate) fn handle_ci(
    args: &CiArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    if args.target_ref.is_some() && args.branch.is_some() {
        let err = OzError::InvalidUsage {
            message: "cannot pass both `--ref` and `--branch` to `oz ci`".to_string(),
            recovery: vec![
                "oz ci --ref <rev> --json".to_string(),
                "oz ci --branch <branch> --json".to_string(),
            ],
        };
        return render_command_result::<CiReport, _>(mode, Err(err), |_| {});
    }

    let query_res = (|| -> Result<(OzoneRepo, CiReport)> {
        let repo = discover_and_recover_repo(directory, false)?;
        let opts = CiQueryOptions {
            target_ref: args.target_ref.clone().or_else(|| args.branch.clone()),
            pr_number: args.pr,
            remote: args.remote.clone(),
            max_excerpt_lines: args.max_excerpt_lines,
            auth_token: None,
            api_url: None,
        };
        let report = query_ci_status(&repo, &opts)?;
        Ok((repo, report))
    })();

    let (repo, report) = match query_res {
        Ok(pair) => pair,
        Err(err) => return render_command_result::<CiReport, _>(mode, Err(err), |_| {}),
    };

    let has_failures = report.failing_count > 0;
    let exit_code = if has_failures {
        ExitCode::ConflictOrCheckFail
    } else {
        ExitCode::Success
    };

    let branch = (!report.ref_name.is_empty())
        .then(|| report.ref_name.clone())
        .or(repo.branch);
    let head_oid = (!report.commit_sha.is_empty())
        .then(|| report.commit_sha.clone())
        .or(repo.head_oid);

    let mut env = Envelope::success(report)
        .with_workspace(repo.workspace)
        .with_branch(branch)
        .with_head_oid(head_oid);

    if has_failures {
        let msg = format!(
            "{} of {} CI check(s) failing on {}",
            env.data.failing_count, env.data.total_count, env.data.ref_name
        );
        env.ok = false;
        env.error = Some(EnvelopeError {
            code: "check_failed".to_string(),
            exit_code: exit_code.as_i32(),
            message: msg,
        });
        env = env
            .with_recovery_command("oz status --json")
            .with_recovery_command("oz commit --amend")
            .with_recovery_command("oz push --force-with-lease");
    }

    if mode.json {
        print_json_envelope(&env);
    } else {
        let d = &env.data;
        let short_sha = &d.commit_sha[..d.commit_sha.len().min(10)];
        println!(
            "CI status for {} ({short_sha}): {} ({} total, {} failing, {} pending)",
            d.ref_name, d.overall_status, d.total_count, d.failing_count, d.pending_count
        );
        for check in &d.checks {
            let conc = check.conclusion.as_deref().unwrap_or(&check.status);
            println!("  [{}] {}", conc, check.name);
            if let Some(ref step) = check.failing_step {
                println!("    failing step: {step}");
            }
            if let Some(ref excerpt) = check.log_excerpt {
                for line in excerpt.lines() {
                    println!("      {line}");
                }
            }
        }
    }

    exit_code
}

pub(crate) fn handle_agent(
    cmd: AgentCommand,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    match cmd {
        AgentCommand::Setup(args) => handle_agent_setup(&args, directory, mode),
    }
}

fn handle_agent_setup(
    args: &AgentSetupArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<AgentSetupReport>> {
        let mut parsed_harnesses = Vec::with_capacity(args.harnesses.len());
        for raw in &args.harnesses {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                parsed_harnesses.push(AgentHarness::from_str(trimmed)?);
            }
        }

        let (target_root, workspace, branch, head_oid) =
            match discover_and_recover_repo(directory, false) {
                Ok(repo) => (
                    repo.repo_root,
                    repo.workspace,
                    repo.branch,
                    repo.head_oid,
                ),
                Err(OzError::NotARepository { .. }) => {
                    let dir = match directory {
                        Some(d) => d.to_path_buf(),
                        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                    };
                    (dir, None, None, None)
                }
                Err(err) => return Err(err),
            };

        let opts = AgentSetupOptions {
            harnesses: parsed_harnesses,
            all: args.all,
            check_only: args.check,
            install_hooks: !args.no_hooks,
            ..AgentSetupOptions::default()
        };

        let report = setup_agent_harnesses(&target_root, &opts)?;
        let needs_apply = args.check && !report.is_up_to_date();
        let mut env = Envelope::success(report)
            .with_workspace(workspace)
            .with_branch(branch)
            .with_head_oid(head_oid);
        if needs_apply {
            env = env.with_recovery_command(if args.all {
                "oz agent setup --all"
            } else {
                "oz agent setup"
            });
        }
        Ok(env)
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        let names: Vec<&str> = d.harnesses.iter().map(|h| h.as_str()).collect();
        let mode_prefix = if d.check_only {
            "Agent setup check"
        } else {
            "Configured agent harnesses"
        };
        println!(
            "{mode_prefix} [{}] in {}: {} created, {} updated, {} unchanged",
            names.join(", "),
            d.repo_root.display(),
            d.created.len(),
            d.updated.len(),
            d.unchanged.len()
        );
    })
}

/// Builds an [`McpServer`] wired with a live `oz-github` [`CiLogProvider`](oz_agent::CiLogProvider)
/// so the `oz_ci_logs` MCP tool queries GitHub / `OZ_GITHUB_API_URL` directly.
#[must_use]
pub fn build_mcp_server(root_dir: Option<PathBuf>, session_id: Option<String>) -> McpServer {
    let mut server = McpServer::new(root_dir).with_ci_provider_fn(|repo, req| {
        let explicit_commit_override = req
            .commit_oid
            .as_deref()
            .filter(|oid| !oid.is_empty() && repo.head_oid.as_deref() != Some(*oid))
            .map(str::to_string);
        let target_ref = explicit_commit_override
            .or_else(|| req.branch.clone())
            .or_else(|| req.commit_oid.clone());

        let ci_report = query_ci_status(
            repo,
            &CiQueryOptions {
                target_ref,
                pr_number: req.pr_number,
                remote: None,
                max_excerpt_lines: DEFAULT_MAX_EXCERPT_LINES,
                auth_token: None,
                api_url: None,
            },
        )?;

        let checks = ci_report
            .checks
            .into_iter()
            .filter(|c| {
                if !req.failing_only {
                    return true;
                }
                matches!(
                    c.conclusion.as_deref(),
                    Some(
                        "failure"
                            | "timed_out"
                            | "cancelled"
                            | "action_required"
                            | "startup_failure"
                            | "stale"
                    )
                )
            })
            .map(|c| CiLogEntry {
                name: c.name,
                status: c.status,
                conclusion: c.conclusion,
                failing_step: c.failing_step,
                log_excerpt: c.log_excerpt,
            })
            .collect();

        Ok(CiLogsReport {
            branch: (!ci_report.ref_name.is_empty()).then_some(ci_report.ref_name),
            commit_oid: (!ci_report.commit_sha.is_empty()).then_some(ci_report.commit_sha),
            all_passed: ci_report.all_passed,
            checks,
        })
    });

    if let Some(sid) = session_id {
        server = server.with_session_id(sid);
    }
    server
}

pub(crate) fn handle_mcp(
    args: &McpArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let mut server = build_mcp_server(directory.map(Path::to_path_buf), args.session_id.clone());
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match server.serve_stdio(stdin.lock(), stdout.lock()) {
        Ok(()) => ExitCode::Success,
        Err(err) => {
            if mode.json {
                let env = Envelope::from_error(&err);
                if let Ok(s) = env.to_json_pretty() {
                    eprintln!("{s}");
                }
            } else {
                eprintln!("error: {err}");
            }
            err.exit_code()
        }
    }
}

pub(crate) fn handle_blame(
    args: &BlameArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<BlameReport>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let opts = BlameOptions {
            revision: args.rev.clone(),
        };
        let report = blame_file(&repo, &args.path, &opts)?;
        let head_oid = Some(report.resolved_commit_oid.clone());
        Ok(Envelope::success(report)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(head_oid))
    })();

    render_command_result(mode, res, |env| {
        print!("{}", env.data.render_human());
    })
}
