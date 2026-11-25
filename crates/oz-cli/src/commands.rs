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
    FileChange, InteractionMode, OzError, OzoneRepo, Result, RevisionDiffOptions, ScopePolicy,
    StatusReport, WarningEntry, compute_diff_filtered, compute_revision_diff, compute_status,
    format_numbered_blob_lines, normalize_lexical, parse_line_range, render_unified_diff,
    render_unified_diff_numbered,
};
use oz_cow::{
    GcWorkspacesOptions, RemoveWorkspaceOptions, SpawnWorkspaceOptions, detect_filesystem,
    gc_workspaces, list_workspaces, remove_workspace, select_backend, spawn_workspace,
};
use oz_github::{
    CiJobLogOptions, CiJobLogReport, CiQueryOptions, CiReport, CiRunsOptions, CiRunsReport,
    CiWatchEvent, CiWatchOptions, CiWatchOutcome, CiWorkflowOptions, CiWorkflowReport,
    CloneOptions, CloneOutcome, DEFAULT_MAX_EXCERPT_LINES, DEFAULT_WATCH_STARTUP_TIMEOUT_SECS,
    DEFAULT_WATCH_TIMEOUT_SECS, DEFAULT_WORKFLOW_RUN_LIMIT, FetchOptions, FetchOutcome,
    ForceLeaseSpec, HttpAuthScheme, IssueCommentOptions, IssueCommentOutcome, IssueListOptions,
    IssueListOutcome, IssueViewOptions, IssueViewOutcome, MAX_MCP_WAIT_SECONDS, MultiPrCiReport,
    PrCommentsOptions, PrCommentsOutcome, PrCreateOptions, PrCreateOutcome, PrDiffOptions,
    PrDiffOutcome, PrEditOptions, PrEditOutcome, PrInlineCommentInput, PrListOptions,
    PrListOutcome, PrReadOptions, PrReadOutcome, PrReplyOptions, PrReplyOutcome, PrResolveOptions,
    PrResolveOutcome, PrReviewEvent, PrReviewOptions, PrReviewOutcome, PrReviewThread,
    PrStackOptions, PrStackOutcome, PrStatusOptions, PrStatusOutcome, PrViewOptions, PrViewOutcome,
    PullOptions, PullOutcome, PullStrategy, PushOptions, PushOutcome, SyncOptions, SyncOutcome,
    clone_repo, fetch_remote, issue_comment, issue_list, issue_view, parse_inline_comment_spec,
    parse_issue_target, pr_comments, pr_create, pr_diff, pr_edit, pr_list, pr_read, pr_reply,
    pr_resolve, pr_review, pr_stack, pr_status, pr_view, pull_current_branch,
    push_refs_with_options, query_ci_job_log, query_ci_runs, query_ci_status,
    query_multi_pr_ci_status, query_workflow_ci_status, sync_repo, watch_ci_status,
    watch_workflow_ci_status,
};
use oz_integrate::{
    AbortOutcome, CollisionSeverity, ContinueOutcome, IntegrateOptions, IntegrateOutcome,
    IntegrationMode, MergeFastForwardMode, RadarOptions, RadarReport, ResolveOptions,
    ResolveOutcome, StackRebaseOptions, StackRebaseOutcome, StackStatusReport, TakeSide,
    abort_integration, continue_integration_with_verify, continue_stack_rebase, integrate,
    resolve_conflict, scan_radar, stack_rebase, stack_status,
};
use oz_oplog::{
    ActorInfo, ActorKind, CheckpointOutcome, MutationTransaction, RedoOutcome, UndoOutcome,
    UndoTarget, checkpoint, redo, undo,
};
use oz_warm::{BaselineSource, gc_baselines};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

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
    /// Explicit CoW backend override (`auto`, `cas_hardlink`, `linux_fuse3`, `linux_ficlone`, `apfs_clonefile`, `windows_refs_clone`, `windows_proj_fs`).
    #[arg(long = "backend", value_name = "BACKEND")]
    pub backend: Option<String>,
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

#[derive(Debug, Default, Args)]
pub struct StatusArgs {
    /// List every hunk ID inline even when a file has more than 5 hunks.
    #[arg(long = "hunks")]
    pub hunks: bool,
}

#[derive(Debug, Args)]
pub struct DiffArgs {
    /// Base revision (`--base <REV>`) to compare against `HEAD` or `--rev`.
    #[arg(long = "base", value_name = "REV")]
    pub base: Option<String>,
    /// Target revision (`--rev <REV>`) when comparing revisions (defaults to `HEAD` when `--base` is set).
    #[arg(long = "rev", value_name = "REV")]
    pub rev: Option<String>,
    /// Display diffstat summary instead of the full unified diff.
    #[arg(long = "stat")]
    pub stat: bool,
    /// Render explicit 1-based old/new line numbers alongside each diff line.
    #[arg(long = "line-numbers", alias = "numbered")]
    pub line_numbers: bool,
    /// Exclude file paths, directory prefixes, globs, file IDs (`f1`), or hunk IDs (`h1a`).
    #[arg(long = "exclude", num_args = 1.., action = clap::ArgAction::Append, value_name = "PATTERN")]
    pub exclude: Vec<String>,
    /// Revision range (`A..B` / `A...B`), file paths, directory prefixes, globs, `:!<path>` exclusions, file IDs (`f1`), or hunk IDs (`h1a`).
    pub targets: Vec<String>,
}

/// Create a stage-free commit (includes untracked files automatically; AI provenance is recorded
/// strictly in local `refs/notes/ai` git notes and is NEVER written into commit messages).
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
    /// Install and start the single user-wide `systemd --user` service (`oz-vfsd.service`) for all repos on Linux.
    #[arg(long)]
    pub systemd_user: bool,
    /// Install a repo-specific systemd user unit instead of the default user-wide `oz-vfsd.service` (used with `--systemd-user`).
    #[arg(long, requires = "systemd_user")]
    pub per_repo: bool,
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
pub struct MergeArgs {
    /// Revision(s), branch(es), or remote-tracking refs (e.g. `origin/main`) to merge into the current branch.
    pub sources: Vec<String>,
    /// Always create a merge commit even when the current branch can fast-forward (`--no-ff`).
    #[arg(long = "no-ff", conflicts_with = "ff_only")]
    pub no_ff: bool,
    /// Refuse to merge and exit with conflict if the current branch cannot fast-forward (`--ff-only`).
    #[arg(long = "ff-only", conflicts_with = "no_ff")]
    pub ff_only: bool,
    /// Optional custom merge commit message.
    #[arg(short = 'm', long = "message", value_name = "MSG")]
    pub message: Option<String>,
    /// Verification shell command executed inside `../<repo>.oz/_integrate` before advancing the branch.
    #[arg(long, value_name = "CMD")]
    pub verify: Option<String>,
    /// Override `--scope` and `--deny` path guardrails.
    #[arg(long)]
    pub allow_out_of_scope: bool,
    /// Override content guardrails (secrets, conflict markers, oversized blobs).
    #[arg(long)]
    pub force: bool,
    /// Resume an active merge conflict session after resolving `_integrate/`.
    #[arg(long = "continue")]
    pub continue_session: bool,
    /// Abort an active merge conflict session and remove `_integrate/`.
    #[arg(long)]
    pub abort: bool,
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
    #[arg(value_name = "DIRECTORY")]
    pub dest: Option<PathBuf>,
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
pub struct PullArgs {
    /// Remote name (defaults to branch upstream remote or `origin`).
    pub remote: Option<String>,
    /// Upstream branch name to pull from (defaults to branch upstream or current branch).
    pub branch: Option<String>,
    /// Rebase current branch commits onto the fetched upstream commit when diverged.
    #[arg(short = 'r', long = "rebase", conflicts_with = "ff_only")]
    pub rebase: bool,
    /// Refuse to merge or rebase when local and upstream branches have diverged.
    #[arg(long = "ff-only", conflicts_with = "rebase")]
    pub ff_only: bool,
    /// Prune remote-tracking branches that no longer exist on the remote.
    #[arg(short = 'p', long = "prune")]
    pub prune: bool,
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
    /// Watch GitHub CI checks for the pushed commit until pass, fail-fast, or timeout.
    #[arg(short = 'w', long = "watch")]
    pub watch: bool,
    /// Exit immediately on the first failing CI check when `--watch` is enabled (default).
    #[arg(long = "fail-fast")]
    pub fail_fast: bool,
    /// Wait for every watched CI check to complete even after a failure (requires `--watch`).
    #[arg(long = "wait-all")]
    pub wait_all: bool,
    /// Overall CI watch timeout in seconds (default: 1800).
    #[arg(
        long = "timeout",
        default_value_t = DEFAULT_WATCH_TIMEOUT_SECS,
        value_name = "SECS"
    )]
    pub timeout: u64,
    /// Grace period in seconds waiting for GitHub to register checks after push (default: 45).
    #[arg(
        long = "startup-timeout",
        default_value_t = DEFAULT_WATCH_STARTUP_TIMEOUT_SECS,
        value_name = "SECS"
    )]
    pub startup_timeout: u64,
    /// Optional fixed CI polling interval in seconds (defaults to adaptive 5s..30s backoff).
    #[arg(long = "interval", value_name = "SECS")]
    pub interval: Option<u64>,
    /// Only watch CI checks matching this name substring or glob pattern (repeatable).
    #[arg(long = "check", value_name = "PATTERN")]
    pub checks: Vec<String>,
    /// Ignore CI checks matching this name substring or glob pattern (repeatable).
    #[arg(long = "ignore-check", value_name = "PATTERN")]
    pub ignore_checks: Vec<String>,
    /// Stream compact CI state-transition updates to stderr while watching.
    #[arg(long = "progress")]
    pub progress: bool,
}

/// Pull request subcommands (`oz pr ...`).
#[derive(Debug, Subcommand)]
pub enum PrCommand {
    /// Create or idempotently update a GitHub Pull Request preserving `Change-Id:`.
    Create(PrCreateArgs),
    /// Edit an existing GitHub Pull Request's title, body, base branch, state, or draft flag.
    Edit(PrEditArgs),
    /// List or search GitHub Pull Requests (`--state`, `--search`, `--author`, `--base`, `--limit`).
    List(PrListArgs),
    /// Compact one-line status summary across one or more PRs (verdict, unresolved threads, mergeability, CI).
    Status(PrStatusArgs),
    /// Push stacked branches atomically and create/update cross-linked stacked PRs.
    Stack(PrStackArgs),
    /// Inspect PR metadata, full head/base SHAs, changed files, review states, and optionally comments.
    View(PrViewArgs),
    /// Fetch PR reviews, inline review threads grouped by `path:line` (with `[t1]` handles & resolved/outdated state), and conversation comments.
    Comments(PrCommentsArgs),
    /// Reply to an inline PR review thread by `[t1]` handle, numeric comment ID (`#12345`), or GraphQL thread ID.
    Reply(PrReplyArgs),
    /// Resolve (or `--unresolve`) an inline PR review thread by `[t1]` handle, GraphQL thread ID, or comment ID.
    Resolve(PrResolveArgs),
    /// Show a line-numbered PR diff with deterministic `[f1]` / `[h1a]` IDs (`--stat`, `--exclude`).
    Diff(PrDiffArgs),
    /// Read a file at the exact PR head commit without mutating the local working tree.
    Read(PrReadArgs),
    /// Submit a PR review with auto-filled `commit_id` and pre-validated inline diff comments.
    Review(PrReviewArgs),
}

#[derive(Debug, Args)]
pub struct PrViewArgs {
    /// Pull request number.
    pub number: u64,
    /// Include inline review comment threads and conversation comments.
    #[arg(long = "comments")]
    pub comments: bool,
    /// Compact summary omitting both PR body and changed file list.
    #[arg(long = "brief")]
    pub brief: bool,
    /// Omit the PR markdown description body from output.
    #[arg(long = "no-body")]
    pub no_body: bool,
    /// Omit the changed files list from output.
    #[arg(long = "no-files")]
    pub no_files: bool,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct PrListArgs {
    /// Filter PR state (`open`, `closed`, `merged`, `all`; default: `open`).
    #[arg(long = "state", value_name = "STATE")]
    pub state: Option<String>,
    /// Free-text GitHub search query (e.g. `--search "cleos"`).
    #[arg(long = "search", value_name = "QUERY")]
    pub search: Option<String>,
    /// Filter by author login.
    #[arg(long = "author", value_name = "USER")]
    pub author: Option<String>,
    /// Filter by base branch.
    #[arg(short = 'B', long = "base", value_name = "BRANCH")]
    pub base: Option<String>,
    /// Filter by head branch.
    #[arg(short = 'H', long = "head", value_name = "BRANCH")]
    pub head: Option<String>,
    /// Filter by label (repeatable).
    #[arg(long = "label", action = clap::ArgAction::Append, value_name = "LABEL")]
    pub label: Vec<String>,
    /// Maximum number of pull requests to return (default: 20).
    #[arg(short = 'L', long = "limit", default_value_t = 20, value_name = "N")]
    pub limit: usize,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct PrEditArgs {
    /// Pull request number to edit.
    pub number: u64,
    /// Updated pull request title.
    #[arg(short = 't', long = "title", value_name = "TITLE")]
    pub title: Option<String>,
    /// Updated pull request markdown body.
    #[arg(
        short = 'b',
        long = "body",
        value_name = "BODY",
        conflicts_with = "body_file"
    )]
    pub body: Option<String>,
    /// Read updated pull request body from a file path or `-` for stdin.
    #[arg(
        short = 'F',
        long = "body-file",
        value_name = "PATH",
        conflicts_with = "body"
    )]
    pub body_file: Option<String>,
    /// Updated base target branch.
    #[arg(short = 'B', long = "base", value_name = "BRANCH")]
    pub base: Option<String>,
    /// Updated PR state (`open` or `closed`).
    #[arg(long = "state", value_name = "STATE")]
    pub state: Option<String>,
    /// Convert pull request to draft.
    #[arg(long = "draft", conflicts_with = "ready")]
    pub draft: bool,
    /// Mark draft pull request ready for review.
    #[arg(long = "ready", conflicts_with = "draft")]
    pub ready: bool,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct PrReplyArgs {
    /// Pull request number.
    pub number: u64,
    /// Inline review thread handle (`t1` / `[t1]`), numeric comment ID (`#4694921942`), or GraphQL thread ID (`PRRT_...`).
    pub target: String,
    /// Reply body text.
    #[arg(
        short = 'm',
        long = "message",
        visible_short_alias = 'b',
        visible_alias = "body",
        value_name = "TEXT",
        conflicts_with = "body_file"
    )]
    pub body: Option<String>,
    /// Read reply body from a file path or `-` for stdin.
    #[arg(
        short = 'F',
        long = "body-file",
        value_name = "PATH",
        conflicts_with = "body"
    )]
    pub body_file: Option<String>,
    /// Also mark the review thread resolved after posting the reply.
    #[arg(long = "resolve")]
    pub resolve: bool,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct PrResolveArgs {
    /// Pull request number.
    pub number: u64,
    /// Inline review thread handle (`t1` / `[t1]`), GraphQL thread ID (`PRRT_...`), or numeric comment ID (`#4694921942`).
    pub target: String,
    /// Mark the thread unresolved instead of resolved.
    #[arg(long = "unresolve")]
    pub unresolve: bool,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct PrStatusArgs {
    /// Pull request number(s) to summarize (comma-separated or repeatable, e.g. `--pr 3338,3339,3340`). Defaults to current branch's open PR.
    #[arg(
        long = "pr",
        value_name = "NUMBER",
        value_delimiter = ',',
        action = clap::ArgAction::Append
    )]
    pub pr: Vec<u64>,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct PrCommentsArgs {
    /// Pull request number.
    pub number: u64,
    /// Only include inline review threads that are not marked resolved.
    #[arg(long = "unresolved")]
    pub unresolved: bool,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

/// GitHub Issue subcommands (`oz issue ...`).
#[derive(Debug, Subcommand)]
pub enum IssueCommand {
    /// Inspect a GitHub Issue via REST (`oz issue view <number|url>`) without deprecated GraphQL fields.
    View(IssueViewArgs),
    /// List or search GitHub Issues via REST (`--state`, `--search`, `--label`, `--assignee`, `--limit`).
    List(IssueListArgs),
    /// Post a conversation comment on a GitHub Issue (`oz issue comment <number|url> --body "..."`).
    Comment(IssueCommentArgs),
}

#[derive(Debug, Args)]
pub struct IssueViewArgs {
    /// Issue number (`3311`, `#3311`) or full GitHub issue URL.
    pub target: String,
    /// Include issue conversation comments.
    #[arg(long = "comments")]
    pub comments: bool,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct IssueListArgs {
    /// Filter issue state (`open`, `closed`, `all`; default: `open`).
    #[arg(long = "state", value_name = "STATE")]
    pub state: Option<String>,
    /// Free-text GitHub search query (e.g. `--search "cleos"`).
    #[arg(long = "search", value_name = "QUERY")]
    pub search: Option<String>,
    /// Filter by author login.
    #[arg(long = "author", value_name = "USER")]
    pub author: Option<String>,
    /// Filter by assignee login.
    #[arg(long = "assignee", value_name = "USER")]
    pub assignee: Option<String>,
    /// Filter by label (repeatable).
    #[arg(long = "label", action = clap::ArgAction::Append, value_name = "LABEL")]
    pub label: Vec<String>,
    /// Maximum number of issues to return (default: 20).
    #[arg(short = 'L', long = "limit", default_value_t = 20, value_name = "N")]
    pub limit: usize,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct IssueCommentArgs {
    /// Issue number (`3344`, `#3344`) or full GitHub issue URL.
    pub target: String,
    /// Comment markdown body.
    #[arg(
        short = 'b',
        long = "body",
        visible_short_alias = 'm',
        visible_alias = "message",
        value_name = "BODY",
        conflicts_with = "body_file"
    )]
    pub body: Option<String>,
    /// Read comment body from a file path or `-` for stdin.
    #[arg(
        short = 'F',
        long = "body-file",
        value_name = "PATH",
        conflicts_with = "body"
    )]
    pub body_file: Option<String>,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct PrDiffArgs {
    /// Pull request number.
    pub number: u64,
    /// Display diffstat summary instead of the unified diff.
    #[arg(long = "stat")]
    pub stat: bool,
    /// Include explicit 1-based old/new line numbers (enabled by default for `oz pr diff`).
    #[arg(long = "line-numbers", alias = "numbered")]
    pub line_numbers: bool,
    /// Omit explicit old/new line number columns and print standard unified diff lines.
    #[arg(long = "no-line-numbers", conflicts_with = "line_numbers")]
    pub no_line_numbers: bool,
    /// Exclude file paths, directory prefixes, globs, file IDs (`f1`), or hunk IDs (`h1a`).
    #[arg(long = "exclude", num_args = 1.., action = clap::ArgAction::Append, value_name = "PATTERN")]
    pub exclude: Vec<String>,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
    /// Optional file paths, directory prefixes, globs, `:!<path>` exclusions, file IDs, or hunk IDs.
    pub targets: Vec<String>,
}

#[derive(Debug, Args)]
pub struct PrReadArgs {
    /// Pull request number.
    pub number: u64,
    /// Repository-relative file path to read at the PR head commit.
    pub path: String,
    /// Optional 1-based inclusive line range (e.g. `300..380`, `10-40`, `50`).
    #[arg(long = "lines", value_name = "START..END")]
    pub lines: Option<String>,
    /// Prefix output lines with 1-based line numbers (enabled by default when `--lines` is set).
    #[arg(short = 'n', long = "line-numbers", alias = "numbered")]
    pub line_numbers: bool,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct PrReviewArgs {
    /// Pull request number.
    pub number: u64,
    /// Approve the pull request (`APPROVE`).
    #[arg(long = "approve", conflicts_with_all = ["request_changes", "comment"])]
    pub approve: bool,
    /// Request changes on the pull request (`REQUEST_CHANGES`).
    #[arg(long = "request-changes", conflicts_with_all = ["approve", "comment"])]
    pub request_changes: bool,
    /// Submit a general comment review (`COMMENT`, default when no decision flag is given).
    #[arg(long = "comment", conflicts_with_all = ["approve", "request_changes"])]
    pub comment: bool,
    /// Top-level review summary body.
    #[arg(
        short = 'b',
        long = "body",
        value_name = "TEXT",
        conflicts_with = "body_file"
    )]
    pub body: Option<String>,
    /// Read top-level review summary body from a file path or `-` for stdin.
    #[arg(
        short = 'F',
        long = "body-file",
        value_name = "PATH",
        conflicts_with = "body"
    )]
    pub body_file: Option<String>,
    /// Inline diff comment (`<path>:<line>[:LEFT|RIGHT]=<body>` or `<path>:<start>-<end>[:LEFT|RIGHT]=<body>`).
    #[arg(long = "inline", alias = "comment-inline", action = clap::ArgAction::Append, value_name = "SPEC")]
    pub inline: Vec<String>,
    /// Path to a JSON file (or `-` for stdin) containing an array of `{path, line, side?, start_line?, body}` objects.
    #[arg(long = "comments-file", value_name = "PATH")]
    pub comments_file: Option<String>,
    /// Validate all inline comment paths/lines against the PR diff without posting to GitHub.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct PrCreateArgs {
    /// Pull request title (inferred from commit subject when omitted).
    #[arg(short = 't', long = "title", value_name = "TITLE")]
    pub title: Option<String>,
    /// Pull request markdown body (inferred from commit body + `Change-Id:` when omitted).
    #[arg(
        short = 'b',
        long = "body",
        value_name = "BODY",
        conflicts_with = "body_file"
    )]
    pub body: Option<String>,
    /// Read pull request markdown body from a file path or `-` for stdin.
    #[arg(
        short = 'F',
        long = "body-file",
        value_name = "PATH",
        conflicts_with = "body"
    )]
    pub body_file: Option<String>,
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
    /// Pull request number(s) to inspect (comma-separated or repeatable, e.g. `--pr 3320,3321,3322`).
    #[arg(
        long = "pr",
        value_name = "NUMBER",
        value_delimiter = ',',
        action = clap::ArgAction::Append
    )]
    pub pr: Vec<u64>,
    /// GitHub Actions workflow filename, path, ID, or display name to inspect (e.g. `sentry-release.yaml`).
    #[arg(long = "workflow", value_name = "WORKFLOW")]
    pub workflow: Option<String>,
    /// List recent GitHub Actions runs across ALL workflows (equivalent to `gh run list`, filterable with `--branch` / `--limit`).
    #[arg(long = "runs", conflicts_with_all = ["workflow", "pr", "job", "watch"])]
    pub runs: bool,
    /// Include full per-check metadata for passing checks instead of collapsing passing checks into `passed_checks`.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
    /// Maximum recent workflow runs to return with `--workflow` or `--runs` (default: 5; `--runs` defaults to 10 if omitted).
    #[arg(long = "limit", value_name = "N")]
    pub limit: Option<usize>,
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
    /// Block and watch CI checks until pass, fail-fast, superseded, or timeout.
    #[arg(short = 'w', long = "watch")]
    pub watch: bool,
    /// Exit immediately on the first failing check when `--watch` is enabled (default).
    #[arg(long = "fail-fast")]
    pub fail_fast: bool,
    /// Wait for every watched check to finish even after a failure (requires `--watch`).
    #[arg(long = "wait-all")]
    pub wait_all: bool,
    /// Overall watch timeout in seconds (default: 1800).
    #[arg(
        long = "timeout",
        default_value_t = DEFAULT_WATCH_TIMEOUT_SECS,
        value_name = "SECS"
    )]
    pub timeout: u64,
    /// Startup grace period in seconds waiting for checks to appear (default: 45).
    #[arg(
        long = "startup-timeout",
        default_value_t = DEFAULT_WATCH_STARTUP_TIMEOUT_SECS,
        value_name = "SECS"
    )]
    pub startup_timeout: u64,
    /// Optional fixed polling interval in seconds (defaults to adaptive 5s..30s backoff).
    #[arg(long = "interval", value_name = "SECS")]
    pub interval: Option<u64>,
    /// Only include/watch checks matching this name substring or glob pattern (repeatable).
    #[arg(long = "check", value_name = "PATTERN")]
    pub checks: Vec<String>,
    /// Ignore checks matching this name substring or glob pattern (repeatable).
    #[arg(long = "ignore-check", value_name = "PATTERN")]
    pub ignore_checks: Vec<String>,
    /// Stream compact state-transition lines to stderr while watching (auto-enabled on interactive TTY).
    #[arg(long = "progress")]
    pub progress: bool,
    /// Fetch logs directly for a single GitHub Actions job ID (mutually exclusive with `--watch`).
    #[arg(long = "job", value_name = "JOB_ID")]
    pub job: Option<u64>,
    /// Optional step name filter when inspecting a job with `--job`.
    #[arg(long = "step", value_name = "STEP_NAME")]
    pub step: Option<String>,
    /// Return cleaned tail lines instead of error-anchor excerpts (used with `--job`).
    #[arg(long = "raw")]
    pub raw: bool,
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
    clean: bool,
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
    if let Err(err) = out
        .write_all(line.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush())
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
        WsCommand::Spawn(args) => {
            render_command_result(mode, execute_ws_spawn(&args, directory, mode), |env| {
                let d = &env.data;
                let commit_suffix = d
                    .base_commit
                    .as_deref()
                    .map(|oid| format!("clean @ {}", &oid[..oid.len().min(12)]))
                    .unwrap_or_else(|| "clean".to_string());
                println!(
                    "spawned workspace '{}' on branch '{}' ({}, {}, {:.1} ms) -> {}",
                    d.workspace,
                    d.branch,
                    commit_suffix,
                    d.backend,
                    d.spawn_elapsed_ms,
                    d.worktree_path.display()
                );
                for w in &env.warnings {
                    match w {
                        WarningEntry::Text(msg) => eprintln!("warning: {msg}"),
                        WarningEntry::Structured { message, .. } => eprintln!("warning: {message}"),
                    }
                }
            })
        }
        WsCommand::List => render_command_result(mode, execute_ws_list(directory), |env| {
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
        }),
        WsCommand::Rm(args) => {
            render_command_result(mode, execute_ws_rm(&args, directory, mode), |env| {
                let d = &env.data;
                println!(
                    "removed workspace '{}' ({})",
                    d.name,
                    d.worktree_path.display()
                );
            })
        }
        WsCommand::Gc(args) => {
            render_command_result(mode, execute_ws_gc(&args, directory, mode), |env| {
                let d = &env.data;
                println!(
                    "gc: removed {} workspace(s) ({} skipped dirty), {} baseline(s) ({} bytes reclaimed)",
                    d.removed_workspaces.len(),
                    d.skipped_dirty_workspaces.len(),
                    d.removed_baselines.len(),
                    d.reclaimed_baseline_bytes
                );
            })
        }
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

    let backend_override = match args.backend.as_deref().map(str::trim) {
        None | Some("") | Some("auto") => None,
        Some(raw) => {
            let parsed = raw
                .parse::<BackendKind>()
                .map_err(|message| OzError::InvalidUsage {
                    message,
                    recovery: vec![
                    "Pass `--backend cas_hardlink` (or omit `--backend` for automatic selection)."
                        .to_string(),
                ],
                })?;
            Some(parsed)
        }
    };

    let opts = SpawnWorkspaceOptions {
        name: args.name.clone(),
        branch: args.branch.clone(),
        from_ref: args.from_ref.clone(),
        backend: backend_override,
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
        clean: true,
        spawn_elapsed_ms,
    };

    let mut env = Envelope::success(data)
        .with_op_id(entry.op_id)
        .with_workspace(Some(info.state.name.clone()))
        .with_branch(Some(info.state.branch))
        .with_head_oid(info.state.base_commit)
        .with_recovery_command(format!("oz ws rm {}", info.state.name));
    for w in info.warnings {
        env = env.with_warning(w);
    }
    Ok(env)
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

pub(crate) fn handle_status(
    args: &StatusArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
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
            print_status_change_line(change, args.hunks);
        }
    })
}

const MAX_INLINE_STATUS_HUNKS: usize = 5;

fn format_status_hunk_tag(hunk_ids: &[&str], show_all_hunks: bool) -> Option<String> {
    if hunk_ids.is_empty() {
        return None;
    }
    if show_all_hunks || hunk_ids.len() <= MAX_INLINE_STATUS_HUNKS {
        Some(format!("hunks: {}", hunk_ids.join(", ")))
    } else {
        Some(format!(
            "{} hunks: {}..{}",
            hunk_ids.len(),
            hunk_ids[0],
            hunk_ids[hunk_ids.len() - 1]
        ))
    }
}

fn print_status_change_line(change: &FileChange, show_all_hunks: bool) {
    let hunk_ids: Vec<&str> = change.hunks.iter().map(|h| h.id.as_str()).collect();
    let mut tags = Vec::new();
    if change.is_binary {
        tags.push("binary".to_string());
    } else if let Some(hunk_tag) = format_status_hunk_tag(&hunk_ids, show_all_hunks) {
        tags.push(hunk_tag);
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

pub(crate) fn resolve_diff_report(repo: &OzoneRepo, args: &DiffArgs) -> Result<DiffReport> {
    if args.base.is_some() || args.rev.is_some() {
        return compute_revision_diff(
            repo,
            &RevisionDiffOptions {
                base: args.base.clone(),
                rev: args.rev.clone(),
                range: None,
                targets: args.targets.clone(),
                excludes: args.exclude.clone(),
            },
        );
    }

    if let Some(first) = args.targets.first()
        && first.contains("..")
        && !first.starts_with(':')
    {
        return compute_revision_diff(
            repo,
            &RevisionDiffOptions {
                base: None,
                rev: None,
                range: Some(first.clone()),
                targets: args.targets[1..].to_vec(),
                excludes: args.exclude.clone(),
            },
        );
    }

    compute_diff_filtered(repo, &args.targets, &args.exclude)
}

pub(crate) fn format_diff_stat(report: &DiffReport) -> String {
    let mut out = String::new();
    for f in &report.files {
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!(" {} | +{} -{}\n", f.path, f.additions, f.deletions),
        );
    }
    let _ = std::fmt::Write::write_fmt(
        &mut out,
        format_args!(
            " {} file(s) changed, {} insertion(+), {} deletion(-)\n",
            report.files.len(),
            report.total_additions,
            report.total_deletions
        ),
    );
    out
}

pub(crate) fn handle_diff(
    args: &DiffArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<DiffData>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let report = resolve_diff_report(&repo, args)?;
        let unified_diff = if args.stat {
            format_diff_stat(&report)
        } else if args.line_numbers {
            render_unified_diff_numbered(&report)
        } else {
            render_unified_diff(&report)
        };
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
        let tx = MutationTransaction::begin(&repo, cmd_label, actor, Some(&pre_status.changes))?;

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
        let target = match (&args.op_id, args.steps) {
            (Some(_), Some(_)) => {
                return Err(OzError::InvalidUsage {
                    message: "cannot pass both `<op_id>` and `--steps` to `oz undo`".to_string(),
                    recovery: vec!["Run `oz undo <op_id>` or `oz undo --steps <n>`.".to_string()],
                });
            }
            (Some(id), None) => UndoTarget::OpId(id.clone()),
            (None, Some(n)) => UndoTarget::Steps(n),
            (None, None) => UndoTarget::Steps(1),
        };

        let repo = discover_and_recover_repo(directory, false)?;
        let actor = actor_from_mode(mode, &repo);

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
        let repo_opt = if !args.systemd_user || args.per_repo {
            // Repo-scoped setup requires being inside a Git repository.
            let repo = discover_and_recover_repo(directory, false)?;
            repo.layout.ensure_initialized()?;
            Some(repo)
        } else {
            let repo_opt = discover_and_recover_repo(directory, false).ok();
            if let Some(ref repo) = repo_opt {
                repo.layout.ensure_initialized()?;
            }
            repo_opt
        };

        let projfs_enabled = if args.enable_projfs {
            let newly_enabled = oz_vfsd::enable_windows_projfs()?;
            Some(newly_enabled || oz_vfsd::is_windows_projfs_enabled())
        } else {
            None
        };

        let probe_path = repo_opt
            .as_ref()
            .map(|r| r.repo_root.clone())
            .or_else(|| directory.map(Path::to_path_buf))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let probe = detect_filesystem(&probe_path)?;
        let configured_backend = repo_opt.as_ref().and_then(|r| r.config.backend);
        let recommended_backend = select_backend(&probe, configured_backend);
        let vfsd_bin = oz_vfsd::find_oz_vfsd_bin()?;

        let systemd_unit_path = if args.systemd_user {
            #[cfg(target_os = "linux")]
            {
                if args.per_repo {
                    let Some(ref repo) = repo_opt else {
                        return Err(OzError::InvalidUsage {
                            message: "`--per-repo` requires running inside a Git repository"
                                .to_string(),
                            recovery: vec![
                                "Run `oz setup --systemd-user --per-repo` inside a Git repository."
                                    .to_string(),
                            ],
                        });
                    };
                    Some(oz_vfsd::install_systemd_user_unit(
                        &vfsd_bin,
                        &repo.layout.git_common_dir,
                    )?)
                } else {
                    Some(oz_vfsd::install_global_systemd_user_unit(&vfsd_bin)?)
                }
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

        let (repo_root, git_common_dir, ozone_dir, ws, br, head_oid) = match repo_opt {
            Some(repo) => (
                repo.repo_root,
                repo.layout.git_common_dir,
                repo.layout.ozone_dir,
                repo.workspace,
                repo.branch,
                repo.head_oid,
            ),
            None => (probe_path, PathBuf::new(), PathBuf::new(), None, None, None),
        };

        let mut env = Envelope::success(SetupData {
            repo_root,
            git_common_dir,
            ozone_dir,
            recommended_backend,
            oz_vfsd_bin: vfsd_bin,
            systemd_unit_path,
            projfs_enabled,
        })
        .with_workspace(ws)
        .with_branch(br)
        .with_head_oid(head_oid);

        if cfg!(target_os = "linux") && !args.systemd_user {
            env = env.with_recovery_command("oz setup --systemd-user");
        }
        if cfg!(target_os = "windows")
            && !args.enable_projfs
            && !oz_vfsd::is_windows_projfs_enabled()
        {
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
            let outcome = continue_integration_with_verify(&repo, args.verify.as_deref(), actor)?;
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
            fast_forward: MergeFastForwardMode::NoFf,
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

pub(crate) fn handle_merge(
    args: &MergeArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<IntegrateCliData>> {
        if args.continue_session && args.abort {
            return Err(OzError::InvalidUsage {
                message: "cannot pass both `--continue` and `--abort` to `oz merge`".to_string(),
                recovery: vec![
                    "oz merge --continue".to_string(),
                    "oz merge --abort".to_string(),
                ],
            });
        }
        if (args.continue_session || args.abort) && !args.sources.is_empty() {
            return Err(OzError::InvalidUsage {
                message: "cannot pass revisions alongside `--continue` or `--abort`".to_string(),
                recovery: vec![
                    "oz merge --continue".to_string(),
                    "oz merge --abort".to_string(),
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
            let outcome = continue_integration_with_verify(&repo, args.verify.as_deref(), actor)?;
            let wrapped = wrap_integrate_envelope(outcome, repo.workspace);
            return Ok(map_envelope_data(wrapped, IntegrateCliData::Integrated));
        }

        if args.sources.is_empty() {
            return Err(OzError::InvalidUsage {
                message: "missing revision(s) to merge (e.g. `oz merge origin/main`)".to_string(),
                recovery: vec![
                    "oz merge origin/main".to_string(),
                    "oz merge --no-ff origin/main".to_string(),
                    "oz merge --continue".to_string(),
                    "oz merge --abort".to_string(),
                ],
            });
        }

        let current_branch = repo.branch.clone().ok_or_else(|| OzError::InvalidUsage {
            message: "cannot run `oz merge` on detached HEAD; switch to a branch first".to_string(),
            recovery: vec!["oz switch <branch>".to_string()],
        })?;

        let fast_forward = if args.no_ff {
            MergeFastForwardMode::NoFf
        } else if args.ff_only {
            MergeFastForwardMode::FfOnly
        } else {
            MergeFastForwardMode::AllowFf
        };

        let opts = IntegrateOptions {
            sources: args.sources.clone(),
            into: Some(current_branch),
            verify_cmd: args.verify.clone(),
            allow_out_of_scope: args.allow_out_of_scope,
            force: args.force,
            message: args.message.clone(),
            fast_forward,
        };
        let outcome = integrate(&repo, &opts, actor)?;
        let wrapped = wrap_integrate_envelope(outcome, repo.workspace);
        Ok(map_envelope_data(wrapped, IntegrateCliData::Integrated))
    })();

    render_command_result(mode, res, |env| match &env.data {
        IntegrateCliData::Integrated(out) => {
            if out.merged_sources.is_empty() {
                println!(
                    "Already up to date on {} ({})",
                    out.into_branch,
                    &out.commit_oid[..out.commit_oid.len().min(12)]
                );
            } else if out.fast_forwarded {
                println!(
                    "Fast-forwarded {} to [{}] ({})",
                    out.into_branch,
                    out.merged_sources.join(", "),
                    &out.commit_oid[..out.commit_oid.len().min(12)]
                );
            } else {
                println!(
                    "Merged [{}] into {} ({})",
                    out.merged_sources.join(", "),
                    out.into_branch,
                    &out.commit_oid[..out.commit_oid.len().min(12)]
                );
            }
        }
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
        println!(
            "Stack onto {} ({} branch(es))",
            d.onto_branch,
            d.branches.len()
        );
        if d.branches.is_empty() {
            println!("no stacked branches ahead of {}", d.onto_branch);
            return;
        }
        for b in &d.branches {
            let parent = b.parent_branch.as_deref().unwrap_or(&d.onto_branch);
            let status_flag = if b.needs_rebase {
                "needs-rebase"
            } else {
                "up-to-date"
            };
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
            .dest
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
            println!(
                "Fetched '{}' ({}): already up to date",
                d.remote, d.remote_url
            );
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

pub(crate) fn handle_pull(
    args: &PullArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<PullOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let actor = actor_from_mode(mode, &repo);
        let opts = PullOptions {
            remote: args.remote.clone(),
            branch: args.branch.clone(),
            rebase: args.rebase,
            ff_only: args.ff_only,
            prune: args.prune,
            tags: args.tags,
            auth_token: None,
            git_auth_scheme: HttpAuthScheme::BasicToken,
        };

        let outcome = pull_current_branch(&repo, &opts, actor)?;
        let op_id = outcome.op_id.clone();
        let warnings = outcome.warnings.clone();
        let branch = Some(outcome.branch.clone());
        let head_oid = Some(outcome.head_oid.clone());

        let mut env = Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(branch)
            .with_head_oid(head_oid);
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
        let short_head = &d.head_oid[..d.head_oid.len().min(7)];
        match d.strategy {
            PullStrategy::UpToDate => println!("Already up to date."),
            PullStrategy::FastForward => println!(
                "Fast-forwarded '{}' to '{}' ({short_head})",
                d.branch, d.upstream_ref
            ),
            PullStrategy::Merge => println!(
                "Merged '{}' into '{}' ({short_head})",
                d.upstream_ref, d.branch
            ),
            PullStrategy::Rebase => println!(
                "Rebased '{}' onto '{}' ({short_head})",
                d.branch, d.upstream_ref
            ),
        }
    })
}

pub(crate) fn handle_push(
    args: &PushArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    if args.watch && args.delete {
        let err = OzError::InvalidUsage {
            message: "cannot combine `--watch` with `--delete` on `oz push`".to_string(),
            recovery: vec![
                "oz push --delete <branch>".to_string(),
                "oz push --watch --json".to_string(),
            ],
        };
        return render_command_result::<PushOutcome, _>(mode, Err(err), |_| {});
    }
    if args.fail_fast && args.wait_all {
        let err = OzError::InvalidUsage {
            message: "cannot pass both `--fail-fast` and `--wait-all` to `oz push`".to_string(),
            recovery: vec![
                "oz push --watch --json".to_string(),
                "oz push --watch --wait-all --json".to_string(),
            ],
        };
        return render_command_result::<PushOutcome, _>(mode, Err(err), |_| {});
    }

    let push_res = (|| -> Result<(OzoneRepo, PushOutcome)> {
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

        let mut outcome = push_refs_with_options(&repo, &opts, args.no_verify, &[], actor)?;

        if args.watch {
            let target_ref = select_pushed_ci_target(&repo, &outcome);
            let emit_progress =
                args.progress || (mode.stdout_is_tty && !mode.json && !mode.non_interactive);
            let watch_opts = CiWatchOptions {
                query: CiQueryOptions {
                    target_ref,
                    pr_number: None,
                    remote: args.remote.clone(),
                    max_excerpt_lines: DEFAULT_MAX_EXCERPT_LINES,
                    include_checks: args.checks.clone(),
                    ignore_checks: args.ignore_checks.clone(),
                    auth_token: None,
                    api_url: None,
                },
                fail_fast: !args.wait_all,
                timeout: Duration::from_secs(args.timeout),
                startup_timeout: Duration::from_secs(args.startup_timeout),
                poll_interval: resolve_ci_poll_interval(args.interval),
            };
            let ci_report = watch_ci_status(&repo, &watch_opts, |ev| {
                if emit_progress {
                    render_watch_progress_event(&ev);
                }
            })?;
            outcome.ci = Some(ci_report);
        }

        Ok((repo, outcome))
    })();

    let (repo, outcome) = match push_res {
        Ok(pair) => pair,
        Err(err) => return render_command_result::<PushOutcome, _>(mode, Err(err), |_| {}),
    };

    let op_id = outcome.op_id.clone();
    let ci_snapshot = outcome.ci.clone();
    let mut env = Envelope::success(outcome)
        .with_workspace(repo.workspace)
        .with_branch(repo.branch)
        .with_head_oid(repo.head_oid);
    if let Some(id) = op_id {
        env = env
            .with_op_id(id.clone())
            .with_recovery_command(format!("oz undo {id}"));
    }

    let exit_code = if let Some(ref ci_report) = ci_snapshot {
        apply_ci_report_to_envelope(&mut env, ci_report)
    } else {
        ExitCode::Success
    };

    if mode.json {
        print_json_envelope(&env);
    } else {
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
        if let Some(ref ci_report) = d.ci {
            render_ci_report_human(ci_report);
        }
    }

    exit_code
}

fn select_pushed_ci_target(repo: &OzoneRepo, outcome: &PushOutcome) -> Option<String> {
    if let Some(ref active_branch) = repo.branch {
        let expected_remote = format!("refs/heads/{active_branch}");
        if let Some(pushed) = outcome
            .pushed_refs
            .iter()
            .find(|r| !r.deleted && r.remote_ref == expected_remote)
        {
            return pushed
                .new_oid
                .clone()
                .or_else(|| Some(active_branch.clone()));
        }
    }
    if let Some(pushed) = outcome
        .pushed_refs
        .iter()
        .find(|r| !r.deleted && r.new_oid.is_some())
    {
        return pushed.new_oid.clone();
    }
    repo.branch.clone().or_else(|| repo.head_oid.clone())
}

fn read_body_or_file(body: Option<&str>, body_file: Option<&str>) -> Result<Option<String>> {
    if let Some(file_spec) = body_file {
        let text = if file_spec == "-" {
            use std::io::Read as _;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|err| OzError::Other(format!("failed reading body from stdin: {err}")))?;
            buf
        } else {
            std::fs::read_to_string(file_spec).map_err(|err| {
                OzError::Other(format!("failed reading body file '{file_spec}': {err}"))
            })?
        };
        return Ok(Some(text));
    }
    Ok(body.map(str::to_string))
}

pub(crate) fn handle_pr(
    cmd: PrCommand,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    match cmd {
        PrCommand::Create(args) => handle_pr_create(&args, directory, mode),
        PrCommand::Edit(args) => handle_pr_edit(&args, directory, mode),
        PrCommand::List(args) => handle_pr_list(&args, directory, mode),
        PrCommand::Status(args) => handle_pr_status(&args, directory, mode),
        PrCommand::Stack(args) => handle_pr_stack(&args, directory, mode),
        PrCommand::View(args) => handle_pr_view(&args, directory, mode),
        PrCommand::Comments(args) => handle_pr_comments(&args, directory, mode),
        PrCommand::Reply(args) => handle_pr_reply(&args, directory, mode),
        PrCommand::Resolve(args) => handle_pr_resolve(&args, directory, mode),
        PrCommand::Diff(args) => handle_pr_diff(&args, directory, mode),
        PrCommand::Read(args) => handle_pr_read(&args, directory, mode),
        PrCommand::Review(args) => handle_pr_review(&args, directory, mode),
    }
}

pub(crate) fn handle_issue(
    cmd: IssueCommand,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    match cmd {
        IssueCommand::View(args) => handle_issue_view(&args, directory, mode),
        IssueCommand::List(args) => handle_issue_list(&args, directory, mode),
        IssueCommand::Comment(args) => handle_issue_comment(&args, directory, mode),
    }
}

fn handle_issue_list(
    args: &IssueListArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<IssueListOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let outcome = issue_list(
            &repo,
            &IssueListOptions {
                state: args.state.clone(),
                search: args.search.clone(),
                author: args.author.clone(),
                assignee: args.assignee.clone(),
                labels: args.label.clone(),
                limit: args.limit,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        if d.issues.is_empty() {
            println!(
                "No matching issues in {}/{} (state={})",
                d.owner, d.repo, d.state
            );
            return;
        }
        println!(
            "Issues in {}/{} (state={}, {} shown):",
            d.owner, d.repo, d.state, d.total_returned
        );
        for item in &d.issues {
            let author = item.author.as_deref().unwrap_or("unknown");
            let labels = if item.labels.is_empty() {
                String::new()
            } else {
                format!(" [{}]", item.labels.join(", "))
            };
            println!(
                "  #{} [{}] {}  @{}{}",
                item.number,
                item.state.to_ascii_uppercase(),
                item.title,
                author,
                labels
            );
        }
    })
}

fn handle_issue_comment(
    args: &IssueCommentArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<IssueCommentOutcome>> {
        let parsed = parse_issue_target(&args.target)?;
        let body =
            read_body_or_file(args.body.as_deref(), args.body_file.as_deref())?.unwrap_or_default();
        let repo = discover_and_recover_repo(directory, false)?;
        let outcome = issue_comment(
            &repo,
            &IssueCommentOptions {
                number: parsed.number,
                owner_override: parsed.owner,
                repo_override: parsed.repo,
                body,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        let url_suffix = d
            .comment
            .html_url
            .as_deref()
            .map(|u| format!(" -> {u}"))
            .unwrap_or_default();
        println!(
            "Commented on #{} (comment #{}){url_suffix}",
            d.number, d.comment.id
        );
    })
}

fn handle_pr_list(args: &PrListArgs, directory: Option<&Path>, mode: InteractionMode) -> ExitCode {
    let res = (|| -> Result<Envelope<PrListOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let outcome = pr_list(
            &repo,
            &PrListOptions {
                state: args.state.clone(),
                search: args.search.clone(),
                author: args.author.clone(),
                base: args.base.clone(),
                head: args.head.clone(),
                labels: args.label.clone(),
                limit: args.limit,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        if d.pull_requests.is_empty() {
            println!(
                "No matching PRs in {}/{} (state={})",
                d.owner, d.repo, d.state
            );
            return;
        }
        println!(
            "Pull requests in {}/{} (state={}, {} shown):",
            d.owner, d.repo, d.state, d.total_returned
        );
        for item in &d.pull_requests {
            let author = item.author.as_deref().unwrap_or("unknown");
            let branch_part = match (&item.head_branch, &item.base_branch) {
                (Some(h), Some(b)) => format!(" ({h} -> {b})"),
                _ => String::new(),
            };
            println!(
                "  #{} [{}] {}  @{}{}",
                item.number, item.state, item.title, author, branch_part
            );
        }
    })
}

fn handle_pr_edit(args: &PrEditArgs, directory: Option<&Path>, mode: InteractionMode) -> ExitCode {
    let res = (|| -> Result<Envelope<PrEditOutcome>> {
        let body = read_body_or_file(args.body.as_deref(), args.body_file.as_deref())?;
        let draft = if args.draft {
            Some(true)
        } else if args.ready {
            Some(false)
        } else {
            None
        };
        let repo = discover_and_recover_repo(directory, false)?;
        let outcome = pr_edit(
            &repo,
            &PrEditOptions {
                number: args.number,
                title: args.title.clone(),
                body,
                base: args.base.clone(),
                state: args.state.clone(),
                draft,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        println!(
            "Updated PR #{} ({}) -> {}",
            d.pull_request.number,
            d.updated_fields.join(", "),
            d.pull_request.html_url
        );
    })
}

fn handle_pr_reply(
    args: &PrReplyArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<PrReplyOutcome>> {
        let body =
            read_body_or_file(args.body.as_deref(), args.body_file.as_deref())?.unwrap_or_default();
        let repo = discover_and_recover_repo(directory, false)?;
        let outcome = pr_reply(
            &repo,
            &PrReplyOptions {
                number: args.number,
                target: args.target.clone(),
                body,
                resolve: args.resolve,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        let handle = d
            .thread_handle
            .as_deref()
            .map(|h| format!("[{h}] "))
            .unwrap_or_default();
        let resolved_suffix = match d.resolved {
            Some(true) => " [resolved]",
            _ => "",
        };
        let url_suffix = d
            .reply
            .html_url
            .as_deref()
            .map(|u| format!(" -> {u}"))
            .unwrap_or_default();
        println!(
            "Replied to PR #{} {handle}#{} (reply #{}){resolved_suffix}{url_suffix}",
            d.pr_number, d.in_reply_to_comment_id, d.reply.id
        );
    })
}

fn handle_pr_resolve(
    args: &PrResolveArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<PrResolveOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let outcome = pr_resolve(
            &repo,
            &PrResolveOptions {
                number: args.number,
                target: args.target.clone(),
                unresolve: args.unresolve,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        let verb = if d.is_resolved {
            "Resolved"
        } else {
            "Unresolved"
        };
        let handle = d
            .thread_handle
            .as_deref()
            .map(|h| format!("[{h}] "))
            .unwrap_or_default();
        println!("{verb} PR #{} {handle}thread {}", d.pr_number, d.thread_id);
    })
}

fn handle_pr_status(
    args: &PrStatusArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<PrStatusOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let outcome = pr_status(
            &repo,
            &PrStatusOptions {
                numbers: args.pr.clone(),
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        let (env_branch, env_head) = if outcome.prs.len() == 1 {
            (
                Some(outcome.prs[0].head_branch.clone()),
                Some(outcome.prs[0].head_sha.clone()),
            )
        } else {
            (None, None)
        };
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(env_branch)
            .with_head_oid(env_head))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        for item in &d.prs {
            let verdict = item.review_decision.as_deref().unwrap_or("PENDING");
            let merge_str = match (item.mergeable, item.mergeable_state.as_deref()) {
                (Some(true), Some(st)) => format!("merge={st}"),
                (Some(true), None) => "merge=clean".to_string(),
                (Some(false), _) => "merge=CONFLICT".to_string(),
                (None, Some(st)) => format!("merge={st}"),
                (None, None) => "merge=unknown".to_string(),
            };
            println!(
                "PR #{} [{}] verdict={} unresolved={} {merge_str} ci={}({}ok/{}fail/{}pend)  {}",
                item.number,
                item.state,
                verdict,
                item.unresolved_threads,
                item.ci_status,
                item.ci_passing,
                item.ci_failing,
                item.ci_pending,
                item.title
            );
        }
    })
}

fn handle_issue_view(
    args: &IssueViewArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<IssueViewOutcome>> {
        let parsed = parse_issue_target(&args.target)?;
        let repo = discover_and_recover_repo(directory, false)?;
        let outcome = issue_view(
            &repo,
            &IssueViewOptions {
                number: parsed.number,
                owner_override: parsed.owner,
                repo_override: parsed.repo,
                include_comments: args.comments,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        let issue = &d.issue;
        let kind_label = if d.is_pull_request {
            "PR/Issue"
        } else {
            "Issue"
        };
        let author = issue
            .user
            .as_ref()
            .map(|u| u.login.as_str())
            .unwrap_or("unknown");
        println!(
            "{kind_label} #{} [{}] {}",
            issue.number,
            issue.state.to_ascii_uppercase(),
            issue.title
        );
        let mut meta = vec![format!("author={author}")];
        if !issue.labels.is_empty() {
            let lbls: Vec<&str> = issue.labels.iter().map(|l| l.name.as_str()).collect();
            meta.push(format!("labels={}", lbls.join(",")));
        }
        if !issue.assignees.is_empty() {
            let people: Vec<&str> = issue.assignees.iter().map(|u| u.login.as_str()).collect();
            meta.push(format!("assignees={}", people.join(",")));
        }
        if let Some(ref ms) = issue.milestone {
            meta.push(format!("milestone={}", ms.title));
        }
        meta.push(format!("comments={}", issue.comments));
        println!("  {}", meta.join("  "));
        if !issue.html_url.is_empty() {
            println!("  url={}", issue.html_url);
        }
        if let Some(ref body) = issue.body
            && !body.trim().is_empty()
        {
            println!("\n{}", body.trim());
        }
        if !d.comments.is_empty() {
            println!("\nComments ({}):", d.comments.len());
            for c in &d.comments {
                let who = c.author.as_deref().unwrap_or("unknown");
                let when = c.created_at.as_deref().unwrap_or("");
                if when.is_empty() {
                    println!("  @{who}:");
                } else {
                    println!("  @{who} ({when}):");
                }
                for line in c.body.trim().lines() {
                    println!("    {line}");
                }
            }
        }
    })
}

fn render_pr_review_threads_human(threads: &[PrReviewThread]) {
    if threads.is_empty() {
        return;
    }
    println!("\nInline Review Threads ({}):", threads.len());
    for t in threads {
        let loc = match (t.start_line, t.line.or(t.original_line)) {
            (Some(s), Some(e)) if s != e => format!("{}:{s}-{e}", t.path),
            (_, Some(l)) => format!("{}:{l}", t.path),
            _ => t.path.clone(),
        };
        let mut tags = Vec::new();
        match t.is_resolved {
            Some(true) => tags.push("resolved"),
            Some(false) => tags.push("unresolved"),
            None => {}
        }
        if t.is_outdated {
            tags.push("outdated");
        }
        let tag_suffix = if tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tags.join(", "))
        };
        let handle_prefix = if t.id.is_empty() {
            format!("#{} ", t.root_comment_id)
        } else {
            format!("[{}] #{} ", t.id, t.root_comment_id)
        };
        println!("  {handle_prefix}{loc} ({}){tag_suffix}", t.side);
        for c in &t.comments {
            let who = c.author.as_deref().unwrap_or("unknown");
            println!("    @{who}:");
            for line in c.body.trim().lines() {
                println!("      {line}");
            }
        }
    }
}

fn handle_pr_comments(
    args: &PrCommentsArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<PrCommentsOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let outcome = pr_comments(
            &repo,
            &PrCommentsOptions {
                number: args.number,
                unresolved_only: args.unresolved,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        let decision = d.review_decision.as_deref().unwrap_or("PENDING");
        println!(
            "PR #{} [{} | verdict: {}] {}",
            d.pr_number, d.state, decision, d.title
        );
        println!(
            "  reviews={}  threads={} (unresolved={}, outdated={})  conversation_comments={}",
            d.total_reviews,
            d.total_threads,
            d.unresolved_threads,
            d.outdated_threads,
            d.total_issue_comments
        );
        if !d.reviews.is_empty() {
            println!("\nReviews:");
            for r in &d.reviews {
                let who = r.author.as_deref().unwrap_or("unknown");
                println!("  @{} [{}]", who, r.state);
                if let Some(ref body) = r.body {
                    for line in body.trim().lines() {
                        println!("    {line}");
                    }
                }
            }
        }
        render_pr_review_threads_human(&d.threads);
        if !d.issue_comments.is_empty() {
            println!("\nConversation Comments ({}):", d.issue_comments.len());
            for c in &d.issue_comments {
                let who = c.author.as_deref().unwrap_or("unknown");
                println!("  @{who}:");
                for line in c.body.trim().lines() {
                    println!("    {line}");
                }
            }
        }
    })
}

fn handle_pr_view(args: &PrViewArgs, directory: Option<&Path>, mode: InteractionMode) -> ExitCode {
    let omit_body = args.brief || args.no_body;
    let omit_files = args.brief || args.no_files;

    let res = (|| -> Result<Envelope<PrViewOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let mut outcome = pr_view(
            &repo,
            &PrViewOptions {
                number: args.number,
                include_comments: args.comments,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        if omit_body {
            outcome.pull_request.body = None;
        }
        if omit_files {
            outcome.files.clear();
        }
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        let pr = &d.pull_request;
        let state = if pr.is_merged() {
            "MERGED"
        } else if pr.draft {
            "DRAFT"
        } else {
            pr.state.as_str()
        };
        let author = pr
            .user
            .as_ref()
            .map(|u| u.login.as_str())
            .unwrap_or("unknown");
        println!("PR #{} [{}] {}", pr.number, state, pr.title);
        println!(
            "  {} -> {}  author={}  (+{}/-{}, {} files)",
            pr.head.ref_name,
            pr.base.ref_name,
            author,
            d.total_additions,
            d.total_deletions,
            d.files.len()
        );
        println!("  head={}  base={}", pr.head.sha, pr.base.sha);
        if !pr.html_url.is_empty() {
            println!("  url={}", pr.html_url);
        }
        if !d.reviews.is_empty() {
            let summaries: Vec<String> = d
                .reviews
                .iter()
                .map(|r| format!("{}:{}", r.author.as_deref().unwrap_or("unknown"), r.state))
                .collect();
            println!("  reviews: {}", summaries.join(", "));
        }
        if let Some(ref body) = pr.body
            && !body.trim().is_empty()
        {
            println!("\n{}", body.trim());
        }
        if !d.files.is_empty() {
            println!("\nFiles:");
            for f in &d.files {
                println!(
                    "  [{}] {}  {} (+{}/-{})",
                    f.id, f.status, f.path, f.additions, f.deletions
                );
            }
        }
        render_pr_review_threads_human(&d.threads);
        if !d.issue_comments.is_empty() {
            println!("\nConversation Comments ({}):", d.issue_comments.len());
            for c in &d.issue_comments {
                let who = c.author.as_deref().unwrap_or("unknown");
                println!("  @{who}:");
                for line in c.body.trim().lines() {
                    println!("    {line}");
                }
            }
        }
    })
}

#[derive(Debug, Serialize)]
struct PrDiffCliData {
    #[serde(flatten)]
    outcome: PrDiffOutcome,
    unified_diff: String,
}

fn handle_pr_diff(args: &PrDiffArgs, directory: Option<&Path>, mode: InteractionMode) -> ExitCode {
    let res = (|| -> Result<Envelope<PrDiffCliData>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let outcome = pr_diff(
            &repo,
            &PrDiffOptions {
                number: args.number,
                targets: args.targets.clone(),
                excludes: args.exclude.clone(),
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        let numbered = !args.no_line_numbers;
        let unified_diff = if args.stat {
            format_diff_stat(&outcome.report)
        } else if numbered {
            render_unified_diff_numbered(&outcome.report)
        } else {
            render_unified_diff(&outcome.report)
        };
        Ok(Envelope::success(PrDiffCliData {
            outcome,
            unified_diff,
        })
        .with_workspace(repo.workspace)
        .with_branch(repo.branch)
        .with_head_oid(repo.head_oid))
    })();

    render_command_result(mode, res, |env| {
        if !env.data.unified_diff.is_empty() {
            print!("{}", env.data.unified_diff);
        }
    })
}

fn handle_pr_read(args: &PrReadArgs, directory: Option<&Path>, mode: InteractionMode) -> ExitCode {
    let res = (|| -> Result<Envelope<PrReadOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let (start_line, end_line) = match args.lines.as_deref() {
            Some(raw) => parse_line_range(raw)?,
            None => (None, None),
        };
        let outcome = pr_read(
            &repo,
            &PrReadOptions {
                number: args.number,
                path: args.path.clone(),
                start_line,
                end_line,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_command_result(mode, res, |env| {
        let b = &env.data.blob;
        if b.is_binary {
            println!(
                "Binary file PR #{} {}:{} ({} bytes, source={})",
                env.data.pr_number, b.short_oid, b.path, b.size_bytes, env.data.source
            );
            return;
        }
        let numbered = args.line_numbers || args.lines.is_some();
        if numbered {
            print!("{}", format_numbered_blob_lines(&b.content, b.start_line));
        } else {
            print!("{}", b.content);
        }
    })
}

fn handle_pr_review(
    args: &PrReviewArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<PrReviewOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let event = if args.approve {
            PrReviewEvent::Approve
        } else if args.request_changes {
            PrReviewEvent::RequestChanges
        } else {
            PrReviewEvent::Comment
        };

        let mut comments: Vec<PrInlineCommentInput> = Vec::new();
        for spec in &args.inline {
            comments.push(parse_inline_comment_spec(spec)?);
        }
        if let Some(ref file_spec) = args.comments_file {
            let raw_json = if file_spec == "-" {
                use std::io::Read as _;
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf).map_err(|err| {
                    OzError::Other(format!("failed reading comments from stdin: {err}"))
                })?;
                buf
            } else {
                std::fs::read_to_string(file_spec).map_err(|err| {
                    OzError::Other(format!("failed reading comments file '{file_spec}': {err}"))
                })?
            };
            let parsed: Vec<PrInlineCommentInput> =
                serde_json::from_str(&raw_json).map_err(|err| OzError::InvalidUsage {
                    message: format!("invalid comments JSON in '{file_spec}': {err}"),
                    recovery: vec![
                        "Expected a JSON array of objects: `[{\"path\":\"src/a.rs\",\"line\":10,\"side\":\"RIGHT\",\"body\":\"...\"}]`."
                            .to_string(),
                    ],
                })?;
            comments.extend(parsed);
        }

        let body = read_body_or_file(args.body.as_deref(), args.body_file.as_deref())?;
        let outcome = pr_review(
            &repo,
            &PrReviewOptions {
                number: args.number,
                event,
                body,
                comments,
                dry_run: args.dry_run,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_command_result(mode, res, |env| {
        let d = &env.data;
        if d.dry_run {
            println!(
                "Dry-run OK for PR #{} ({}) at commit {} ({} inline comment(s) validated)",
                d.pr_number,
                d.event,
                &d.commit_id[..d.commit_id.len().min(12)],
                d.validated_comments
            );
        } else {
            let url_suffix = d
                .review
                .as_ref()
                .and_then(|r| r.html_url.as_deref())
                .map(|u| format!(" -> {u}"))
                .unwrap_or_default();
            println!(
                "Submitted {} review on PR #{} at commit {} ({} inline comment(s)){url_suffix}",
                d.event,
                d.pr_number,
                &d.commit_id[..d.commit_id.len().min(12)],
                d.validated_comments
            );
        }
    })
}

fn handle_pr_create(
    args: &PrCreateArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<PrCreateOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let actor = actor_from_mode(mode, &repo);
        let body = read_body_or_file(args.body.as_deref(), args.body_file.as_deref())?;
        let opts = PrCreateOptions {
            title: args.title.clone(),
            body,
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
    if args.fail_fast && args.wait_all {
        let err = OzError::InvalidUsage {
            message: "cannot pass both `--fail-fast` and `--wait-all` to `oz ci`".to_string(),
            recovery: vec![
                "oz ci --watch --json".to_string(),
                "oz ci --watch --wait-all --json".to_string(),
            ],
        };
        return render_command_result::<CiReport, _>(mode, Err(err), |_| {});
    }
    if args.job.is_some() && args.watch {
        let err = OzError::InvalidUsage {
            message: "cannot combine `--job` with `--watch` on `oz ci`".to_string(),
            recovery: vec![
                "oz ci --job <id> --max-excerpt-lines 150 --json".to_string(),
                "oz ci --watch --json".to_string(),
            ],
        };
        return render_command_result::<CiReport, _>(mode, Err(err), |_| {});
    }
    if args.workflow.is_some() && !args.pr.is_empty() {
        let err = OzError::InvalidUsage {
            message: "cannot combine `--workflow` with `--pr` on `oz ci`".to_string(),
            recovery: vec![
                "oz ci --workflow <file> --json".to_string(),
                "oz ci --pr 3320,3321 --json".to_string(),
            ],
        };
        return render_command_result::<CiReport, _>(mode, Err(err), |_| {});
    }
    if args.pr.len() > 1 && args.watch {
        let err = OzError::InvalidUsage {
            message: "`--watch` supports a single `--pr` at a time; run snapshot mode without `--watch` for multiple PRs"
                .to_string(),
            recovery: vec![
                "oz ci --pr 3320,3321 --json".to_string(),
                "oz ci --pr 3320 --watch --json".to_string(),
            ],
        };
        return render_command_result::<CiReport, _>(mode, Err(err), |_| {});
    }

    // Direct single-job log drill-down (`oz ci --job <id>`).
    if let Some(job_id) = args.job {
        let job_res = (|| -> Result<Envelope<CiJobLogReport>> {
            let repo = discover_and_recover_repo(directory, false)?;
            let opts = CiJobLogOptions {
                job_id,
                step: args.step.clone(),
                max_excerpt_lines: args.max_excerpt_lines,
                raw: args.raw,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            };
            let report = query_ci_job_log(&repo, &opts)?;
            Ok(Envelope::success(report)
                .with_workspace(repo.workspace)
                .with_branch(repo.branch)
                .with_head_oid(repo.head_oid))
        })();

        return render_command_result(mode, job_res, |env| {
            let d = &env.data;
            let conc = d.conclusion.as_deref().unwrap_or(&d.status);
            println!("CI job {} ({}) [{}]", d.job_id, d.name, conc);
            if let Some(ref step) = d.selected_step.as_ref().or(d.failing_step.as_ref()) {
                println!("  step: {step}");
            }
            if let Some(ref excerpt) = d.log_excerpt {
                for line in excerpt.lines() {
                    println!("    {line}");
                }
            }
        });
    }

    // Cross-workflow run overview (`oz ci --runs [--branch main] [--limit N]`).
    if args.runs {
        let runs_res = (|| -> Result<(OzoneRepo, CiRunsReport)> {
            let repo = discover_and_recover_repo(directory, false)?;
            let opts = CiRunsOptions {
                branch: args.branch.clone().or_else(|| args.target_ref.clone()),
                limit: args.limit.unwrap_or(10),
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            };
            let report = query_ci_runs(&repo, &opts)?;
            Ok((repo, report))
        })();

        let (repo, report) = match runs_res {
            Ok(pair) => pair,
            Err(err) => return render_command_result::<CiRunsReport, _>(mode, Err(err), |_| {}),
        };

        let env_branch = report.branch_filter.clone();
        let env = Envelope::success(report)
            .with_workspace(repo.workspace)
            .with_branch(env_branch)
            .with_head_oid(None);
        if mode.json {
            print_json_envelope(&env);
        } else {
            let d = &env.data;
            let branch_suffix = d
                .branch_filter
                .as_deref()
                .map(|b| format!(" (branch: {b})"))
                .unwrap_or_default();
            println!(
                "Workflow runs{branch_suffix}: {} total ({} passing, {} failing, {} pending)",
                d.total_runs, d.passing_count, d.failing_count, d.pending_count
            );
            for run in &d.runs {
                let conc = run.conclusion.as_deref().unwrap_or(&run.status);
                let wf = run.workflow_name.as_deref().unwrap_or("workflow");
                let br = run.branch.as_deref().unwrap_or("-");
                let short_sha = &run.commit_sha[..run.commit_sha.len().min(10)];
                let ev = run.event.as_deref().unwrap_or("-");
                println!(
                    "  run {} [{}] {}  branch={} sha={} event={}",
                    run.run_id, conc, wf, br, short_sha, ev
                );
            }
        }
        return ExitCode::Success;
    }

    // Workflow-level CI lookup (`oz ci --workflow <file-or-name> [--watch]`).
    if let Some(ref workflow) = args.workflow {
        let wf_res = (|| -> Result<(OzoneRepo, CiWorkflowReport)> {
            let repo = discover_and_recover_repo(directory, false)?;
            let wf_opts = CiWorkflowOptions {
                workflow: workflow.clone(),
                branch: args.branch.clone().or_else(|| args.target_ref.clone()),
                limit: args.limit.unwrap_or(DEFAULT_WORKFLOW_RUN_LIMIT),
                max_excerpt_lines: args.max_excerpt_lines,
                include_checks: args.checks.clone(),
                ignore_checks: args.ignore_checks.clone(),
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            };
            let report = if args.watch {
                let emit_progress =
                    args.progress || (mode.stdout_is_tty && !mode.json && !mode.non_interactive);
                let watch_opts = CiWatchOptions {
                    query: CiQueryOptions::default(),
                    fail_fast: !args.wait_all,
                    timeout: Duration::from_secs(args.timeout),
                    startup_timeout: Duration::from_secs(args.startup_timeout),
                    poll_interval: resolve_ci_poll_interval(args.interval),
                };
                watch_workflow_ci_status(&repo, &wf_opts, &watch_opts, |ev| {
                    if emit_progress {
                        render_watch_progress_event(&ev);
                    }
                })?
            } else {
                query_workflow_ci_status(&repo, &wf_opts)?
            };
            Ok((repo, report))
        })();

        let (repo, mut report) = match wf_res {
            Ok(pair) => pair,
            Err(err) => {
                return render_command_result::<CiWorkflowReport, _>(mode, Err(err), |_| {});
            }
        };

        let report_snapshot = report.clone();
        if !args.verbose {
            report.compact_for_agent();
        }
        let target_branch = report
            .branch_filter
            .clone()
            .or_else(|| report.runs.first().and_then(|r| r.branch.clone()));
        let target_head = report
            .runs
            .first()
            .map(|r| r.commit_sha.clone())
            .filter(|s| !s.is_empty());
        let mut env = Envelope::success(report)
            .with_workspace(repo.workspace)
            .with_branch(target_branch)
            .with_head_oid(target_head);
        let exit_code = apply_workflow_report_to_envelope(&mut env, &report_snapshot);

        if mode.json {
            print_json_envelope(&env);
        } else {
            render_workflow_report_human(&env.data);
        }
        return exit_code;
    }

    // Multi-PR CI snapshot (`oz ci --pr 3320,3321,3322`).
    if args.pr.len() > 1 {
        let multi_res = (|| -> Result<(OzoneRepo, MultiPrCiReport)> {
            let repo = discover_and_recover_repo(directory, false)?;
            let base_opts = CiQueryOptions {
                target_ref: None,
                pr_number: None,
                remote: args.remote.clone(),
                max_excerpt_lines: args.max_excerpt_lines,
                include_checks: args.checks.clone(),
                ignore_checks: args.ignore_checks.clone(),
                auth_token: None,
                api_url: None,
            };
            let report = query_multi_pr_ci_status(&repo, &args.pr, &base_opts)?;
            Ok((repo, report))
        })();

        let (repo, mut report) = match multi_res {
            Ok(pair) => pair,
            Err(err) => return render_command_result::<MultiPrCiReport, _>(mode, Err(err), |_| {}),
        };

        if !args.verbose {
            report.compact_for_agent();
        }
        let failing_prs = report.failing_prs;
        let pr_count = report.pr_count;
        let mut env = Envelope::success(report)
            .with_workspace(repo.workspace)
            .with_branch(None)
            .with_head_oid(None);
        let exit_code = if failing_prs > 0 {
            let code = ExitCode::ConflictOrCheckFail;
            env.ok = false;
            env.error = Some(EnvelopeError {
                code: "check_failed".to_string(),
                exit_code: code.as_i32(),
                message: format!("{failing_prs} of {pr_count} PR(s) have failing CI checks"),
            });
            code
        } else {
            ExitCode::Success
        };

        if mode.json {
            print_json_envelope(&env);
        } else {
            let d = &env.data;
            println!(
                "Multi-PR CI: {} ({} PR(s): {} passing, {} failing, {} pending)",
                d.overall_status, d.pr_count, d.passing_prs, d.failing_prs, d.pending_prs
            );
            for r in &d.reports {
                let pr_tag = r
                    .pr_number
                    .map(|n| format!("PR #{n}"))
                    .unwrap_or_else(|| r.ref_name.clone());
                let short_sha = &r.commit_sha[..r.commit_sha.len().min(10)];
                println!(
                    "\n{pr_tag} ({} @ {short_sha}): {} ({} total, {} failing, {} pending)",
                    r.ref_name, r.overall_status, r.total_count, r.failing_count, r.pending_count
                );
                if r.passed_count > 0 {
                    println!(
                        "  [passed x{}] {}",
                        r.passed_count,
                        r.passed_checks.join(", ")
                    );
                }
                for check in &r.checks {
                    let conc = check.conclusion.as_deref().unwrap_or(&check.status);
                    if let Some(jid) = check.job_id {
                        println!("  [{}] {} (job {})", conc, check.name, jid);
                    } else {
                        println!("  [{}] {}", conc, check.name);
                    }
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
        }
        return exit_code;
    }

    let explicit_target = args.target_ref.is_some() || args.branch.is_some() || !args.pr.is_empty();
    let query_res = (|| -> Result<(OzoneRepo, CiReport)> {
        let repo = discover_and_recover_repo(directory, false)?;
        let query_opts = CiQueryOptions {
            target_ref: args.target_ref.clone().or_else(|| args.branch.clone()),
            pr_number: args.pr.first().copied(),
            remote: args.remote.clone(),
            max_excerpt_lines: args.max_excerpt_lines,
            include_checks: args.checks.clone(),
            ignore_checks: args.ignore_checks.clone(),
            auth_token: None,
            api_url: None,
        };
        let report = if args.watch {
            let emit_progress =
                args.progress || (mode.stdout_is_tty && !mode.json && !mode.non_interactive);
            let watch_opts = CiWatchOptions {
                query: query_opts,
                fail_fast: !args.wait_all,
                timeout: Duration::from_secs(args.timeout),
                startup_timeout: Duration::from_secs(args.startup_timeout),
                poll_interval: resolve_ci_poll_interval(args.interval),
            };
            watch_ci_status(&repo, &watch_opts, |ev| {
                if emit_progress {
                    render_watch_progress_event(&ev);
                }
            })?
        } else {
            query_ci_status(&repo, &query_opts)?
        };
        Ok((repo, report))
    })();

    let (repo, mut report) = match query_res {
        Ok(pair) => pair,
        Err(err) => return render_command_result::<CiReport, _>(mode, Err(err), |_| {}),
    };

    let branch = if !report.ref_name.is_empty() {
        Some(report.ref_name.clone())
    } else if explicit_target {
        None
    } else {
        repo.branch
    };
    let head_oid = if !report.commit_sha.is_empty() {
        Some(report.commit_sha.clone())
    } else if explicit_target {
        None
    } else {
        repo.head_oid
    };

    let report_snapshot = report.clone();
    if !args.verbose {
        report.compact_for_agent();
    }
    let mut env = Envelope::success(report)
        .with_workspace(repo.workspace)
        .with_branch(branch)
        .with_head_oid(head_oid);

    let exit_code = apply_ci_report_to_envelope(&mut env, &report_snapshot);

    if mode.json {
        print_json_envelope(&env);
    } else {
        render_ci_report_human(&env.data);
    }

    exit_code
}

fn resolve_ci_poll_interval(interval_secs: Option<u64>) -> Option<Duration> {
    if let Some(secs) = interval_secs {
        return Some(Duration::from_secs(secs.max(1)));
    }
    std::env::var("OZ_CI_POLL_INTERVAL_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis)
}

fn apply_ci_report_to_envelope<T>(env: &mut Envelope<T>, report: &CiReport) -> ExitCode {
    if let Some(ref watch) = report.watch {
        match watch.outcome {
            CiWatchOutcome::Passed => return ExitCode::Success,
            CiWatchOutcome::FailedFast | CiWatchOutcome::Failed => {
                let exit_code = ExitCode::ConflictOrCheckFail;
                let suffix = if watch.stopped_early {
                    format!(" (fail-fast; {} still pending)", report.pending_count)
                } else {
                    String::new()
                };
                let msg = format!(
                    "{} of {} CI check(s) failing on {}{suffix}",
                    report.failing_count, report.total_count, report.ref_name
                );
                env.ok = false;
                env.error = Some(EnvelopeError {
                    code: "check_failed".to_string(),
                    exit_code: exit_code.as_i32(),
                    message: msg,
                });
                append_ci_failure_recovery(env, report);
                return exit_code;
            }
            CiWatchOutcome::TimedOut => {
                let exit_code = ExitCode::ConflictOrCheckFail;
                let msg = format!(
                    "CI watch timed out after {}s on {} ({} pending, {} failing)",
                    watch.elapsed_secs, report.ref_name, report.pending_count, report.failing_count
                );
                env.ok = false;
                env.error = Some(EnvelopeError {
                    code: "watch_timed_out".to_string(),
                    exit_code: exit_code.as_i32(),
                    message: msg,
                });
                env.recovery_commands
                    .push("oz ci --watch --json".to_string());
                return exit_code;
            }
            CiWatchOutcome::NoChecksTimedOut => {
                let exit_code = ExitCode::ConflictOrCheckFail;
                let msg = format!(
                    "no CI checks registered for {} after {}s",
                    report.ref_name, watch.elapsed_secs
                );
                env.ok = false;
                env.error = Some(EnvelopeError {
                    code: "no_checks_timed_out".to_string(),
                    exit_code: exit_code.as_i32(),
                    message: msg,
                });
                env.recovery_commands.push("oz ci --json".to_string());
                return exit_code;
            }
            CiWatchOutcome::Superseded => {
                let exit_code = ExitCode::ConflictOrCheckFail;
                let newer = watch
                    .superseded_by_sha
                    .as_deref()
                    .map(|s| &s[..s.len().min(10)])
                    .unwrap_or("newer commit");
                let msg = format!("CI watch on {} was superseded by {newer}", report.ref_name);
                env.ok = false;
                env.error = Some(EnvelopeError {
                    code: "watch_superseded".to_string(),
                    exit_code: exit_code.as_i32(),
                    message: msg,
                });
                env.recovery_commands
                    .push("oz ci --watch --json".to_string());
                return exit_code;
            }
        }
    }

    if report.failing_count > 0 {
        let exit_code = ExitCode::ConflictOrCheckFail;
        let msg = format!(
            "{} of {} CI check(s) failing on {}",
            report.failing_count, report.total_count, report.ref_name
        );
        env.ok = false;
        env.error = Some(EnvelopeError {
            code: "check_failed".to_string(),
            exit_code: exit_code.as_i32(),
            message: msg,
        });
        append_ci_failure_recovery(env, report);
        exit_code
    } else {
        ExitCode::Success
    }
}

fn append_ci_failure_recovery<T>(env: &mut Envelope<T>, report: &CiReport) {
    if let Some(job_id) = report
        .checks
        .iter()
        .find(|c| c.log_excerpt.is_some() || c.failing_step.is_some())
        .and_then(|c| c.job_id)
    {
        env.recovery_commands.push(format!(
            "oz ci --job {job_id} --max-excerpt-lines 150 --json"
        ));
    }
    env.recovery_commands.push("oz status --json".to_string());
    env.recovery_commands.push("oz commit --amend".to_string());
    env.recovery_commands
        .push("oz push --force-with-lease --watch".to_string());
}

fn render_watch_progress_event(event: &CiWatchEvent) {
    match event {
        CiWatchEvent::WaitingForChecks {
            ref_name,
            commit_sha,
            ..
        } => {
            let short = &commit_sha[..commit_sha.len().min(10)];
            eprintln!("oz ci: waiting for checks on {ref_name} ({short})...");
        }
        CiWatchEvent::CheckUpdated {
            name,
            status,
            conclusion,
            failing_step,
        } => {
            let state = conclusion.as_deref().unwrap_or(status.as_str());
            if let Some(step) = failing_step {
                eprintln!("oz ci: [{state}] {name} (step: {step})");
            } else {
                eprintln!("oz ci: [{state}] {name}");
            }
        }
        CiWatchEvent::RateLimited { retry_after_secs } => {
            eprintln!("oz ci: GitHub API rate limited; backing off {retry_after_secs}s...");
        }
        CiWatchEvent::Superseded { old_sha, new_sha } => {
            let old_s = &old_sha[..old_sha.len().min(10)];
            let new_s = &new_sha[..new_sha.len().min(10)];
            eprintln!("oz ci: commit {old_s} superseded by {new_s}");
        }
        CiWatchEvent::Completed {
            outcome,
            elapsed_secs,
            total_count,
            failing_count,
            pending_count,
        } => {
            eprintln!(
                "oz ci: {} in {}s ({} total, {} failing, {} pending)",
                outcome.as_str(),
                elapsed_secs,
                total_count,
                failing_count,
                pending_count
            );
        }
    }
}

fn apply_workflow_report_to_envelope<T>(
    env: &mut Envelope<T>,
    report: &CiWorkflowReport,
) -> ExitCode {
    if let Some(ref watch) = report.watch {
        match watch.outcome {
            CiWatchOutcome::Passed => return ExitCode::Success,
            CiWatchOutcome::FailedFast | CiWatchOutcome::Failed => {
                let code = ExitCode::ConflictOrCheckFail;
                env.ok = false;
                env.error = Some(EnvelopeError {
                    code: "check_failed".to_string(),
                    exit_code: code.as_i32(),
                    message: format!("workflow '{}' failed", report.workflow),
                });
                return code;
            }
            CiWatchOutcome::TimedOut => {
                let code = ExitCode::ConflictOrCheckFail;
                env.ok = false;
                env.error = Some(EnvelopeError {
                    code: "watch_timed_out".to_string(),
                    exit_code: code.as_i32(),
                    message: format!(
                        "workflow '{}' watch timed out after {}s",
                        report.workflow, watch.elapsed_secs
                    ),
                });
                return code;
            }
            CiWatchOutcome::NoChecksTimedOut => {
                let code = ExitCode::ConflictOrCheckFail;
                env.ok = false;
                env.error = Some(EnvelopeError {
                    code: "no_checks_timed_out".to_string(),
                    exit_code: code.as_i32(),
                    message: format!(
                        "no runs found for workflow '{}' after {}s",
                        report.workflow, watch.elapsed_secs
                    ),
                });
                return code;
            }
            CiWatchOutcome::Superseded => return ExitCode::ConflictOrCheckFail,
        }
    }

    if report.failing_count > 0 {
        let code = ExitCode::ConflictOrCheckFail;
        env.ok = false;
        env.error = Some(EnvelopeError {
            code: "check_failed".to_string(),
            exit_code: code.as_i32(),
            message: format!("latest run of workflow '{}' failed", report.workflow),
        });
        if let Some(job_id) = report.runs.first().and_then(|r| {
            r.jobs
                .iter()
                .find(|j| j.log_excerpt.is_some() || j.failing_step.is_some())
                .and_then(|j| j.job_id)
        }) {
            env.recovery_commands.push(format!(
                "oz ci --job {job_id} --max-excerpt-lines 150 --json"
            ));
        }
        code
    } else {
        ExitCode::Success
    }
}

fn render_workflow_report_human(d: &CiWorkflowReport) {
    let branch_suffix = d
        .branch_filter
        .as_deref()
        .map(|b| format!(" (branch: {b})"))
        .unwrap_or_default();
    println!(
        "Workflow '{}'{} : {} ({} run(s))",
        d.workflow, branch_suffix, d.overall_status, d.total_runs
    );
    for run in &d.runs {
        let conc = run.conclusion.as_deref().unwrap_or(&run.status);
        let short_sha = &run.commit_sha[..run.commit_sha.len().min(10)];
        let br = run.branch.as_deref().unwrap_or("-");
        let ev = run.event.as_deref().unwrap_or("-");
        println!(
            "  run {} [{}] branch={} sha={} event={}",
            run.run_id, conc, br, short_sha, ev
        );
        if run.passed_count > 0 {
            println!(
                "    [passed x{}] {}",
                run.passed_count,
                run.passed_jobs.join(", ")
            );
        }
        for job in &run.jobs {
            let jconc = job.conclusion.as_deref().unwrap_or(&job.status);
            if let Some(jid) = job.job_id {
                println!("    [{}] {} (job {})", jconc, job.name, jid);
            } else {
                println!("    [{}] {}", jconc, job.name);
            }
            if let Some(ref step) = job.failing_step {
                println!("      failing step: {step}");
            }
            if let Some(ref excerpt) = job.log_excerpt {
                for line in excerpt.lines() {
                    println!("        {line}");
                }
            }
        }
    }
}

fn render_ci_report_human(d: &CiReport) {
    let short_sha = &d.commit_sha[..d.commit_sha.len().min(10)];
    if let Some(ref w) = d.watch {
        println!(
            "CI watch for {} ({short_sha}): {} in {}s ({} total, {} failing, {} pending)",
            d.ref_name,
            w.outcome.as_str(),
            w.elapsed_secs,
            d.total_count,
            d.failing_count,
            d.pending_count
        );
        if let Some(ref newer) = w.superseded_by_sha {
            println!("  superseded by: {}", &newer[..newer.len().min(10)]);
        }
    } else {
        println!(
            "CI status for {} ({short_sha}): {} ({} total, {} failing, {} pending)",
            d.ref_name, d.overall_status, d.total_count, d.failing_count, d.pending_count
        );
    }
    if d.passed_count > 0 {
        println!(
            "  [passed x{}] {}",
            d.passed_count,
            d.passed_checks.join(", ")
        );
    }
    for check in &d.checks {
        let conc = check.conclusion.as_deref().unwrap_or(&check.status);
        if let Some(jid) = check.job_id {
            println!("  [{}] {} (job {})", conc, check.name, jid);
        } else {
            println!("  [{}] {}", conc, check.name);
        }
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
                Ok(repo) => (repo.repo_root, repo.workspace, repo.branch, repo.head_oid),
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
        let max_lines = req
            .max_excerpt_lines
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_MAX_EXCERPT_LINES);

        // Single-job drill-down (`job_id` provided).
        if let Some(job_id) = req.job_id {
            let job_report = query_ci_job_log(
                repo,
                &CiJobLogOptions {
                    job_id,
                    step: req.step.clone(),
                    max_excerpt_lines: max_lines,
                    raw: req.raw,
                    remote: None,
                    auth_token: None,
                    api_url: None,
                },
            )?;
            let is_failing = matches!(
                job_report.conclusion.as_deref(),
                Some("failure" | "timed_out" | "cancelled" | "action_required" | "startup_failure")
            );
            let is_pending = job_report.status != "completed";
            let overall = if is_failing {
                "failing"
            } else if is_pending {
                "pending"
            } else {
                "passing"
            };
            return Ok(CiLogsReport {
                branch: req.branch.clone().or_else(|| repo.branch.clone()),
                commit_oid: req.commit_oid.clone().or_else(|| repo.head_oid.clone()),
                all_passed: !is_failing && !is_pending,
                overall_status: Some(overall.to_string()),
                failing_count: Some(usize::from(is_failing)),
                pending_count: Some(usize::from(is_pending)),
                watch_outcome: None,
                elapsed_secs: None,
                superseded_by_sha: None,
                recommended_poll_secs: None,
                checks: vec![CiLogEntry {
                    name: job_report.name,
                    status: job_report.status,
                    conclusion: job_report.conclusion,
                    failing_step: job_report.selected_step.or(job_report.failing_step),
                    log_excerpt: job_report.log_excerpt,
                    job_id: Some(job_report.job_id),
                    run_id: job_report.run_id,
                    html_url: job_report.html_url,
                }],
            });
        }

        let explicit_commit_override = req
            .commit_oid
            .as_deref()
            .filter(|oid| !oid.is_empty() && repo.head_oid.as_deref() != Some(*oid))
            .map(str::to_string);
        let target_ref = explicit_commit_override
            .or_else(|| req.branch.clone())
            .or_else(|| req.commit_oid.clone());

        let query_opts = CiQueryOptions {
            target_ref,
            pr_number: req.pr_number,
            remote: None,
            max_excerpt_lines: max_lines,
            include_checks: req.check_filter.clone(),
            ignore_checks: req.ignore_checks.clone(),
            auth_token: None,
            api_url: None,
        };

        let wait_secs = req.wait_seconds.unwrap_or(0).min(MAX_MCP_WAIT_SECONDS);
        let ci_report = if wait_secs > 0 {
            let startup_secs = req.startup_timeout_seconds.unwrap_or(30).min(wait_secs);
            let watch_opts = CiWatchOptions {
                query: query_opts,
                fail_fast: req.fail_fast.unwrap_or(true),
                timeout: Duration::from_secs(wait_secs),
                startup_timeout: Duration::from_secs(startup_secs),
                poll_interval: resolve_ci_poll_interval(None),
            };
            watch_ci_status(repo, &watch_opts, |_| {})?
        } else {
            query_ci_status(repo, &query_opts)?
        };

        let watch_meta = ci_report.watch;
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
                job_id: c.job_id,
                run_id: c.run_id,
                html_url: c.html_url,
            })
            .collect();

        Ok(CiLogsReport {
            branch: (!ci_report.ref_name.is_empty()).then_some(ci_report.ref_name),
            commit_oid: (!ci_report.commit_sha.is_empty()).then_some(ci_report.commit_sha),
            all_passed: ci_report.all_passed,
            overall_status: Some(ci_report.overall_status),
            failing_count: Some(ci_report.failing_count),
            pending_count: Some(ci_report.pending_count),
            watch_outcome: watch_meta.as_ref().map(|w| w.outcome.as_str().to_string()),
            elapsed_secs: watch_meta.as_ref().map(|w| w.elapsed_secs),
            superseded_by_sha: watch_meta
                .as_ref()
                .and_then(|w| w.superseded_by_sha.clone()),
            recommended_poll_secs: watch_meta.as_ref().and_then(|w| w.recommended_poll_secs),
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
