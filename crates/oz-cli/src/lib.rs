// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ozone CLI (`oz` and `git-ozone`) argument parsing, `oz.v1` JSON rendering, and subcommands.

#![allow(
    clippy::missing_errors_doc,
    clippy::too_many_lines,
    clippy::struct_excessive_bools,
    clippy::fn_params_excessive_bools
)]

pub(crate) mod aliases;
pub(crate) mod commands;
pub(crate) mod daily;
pub(crate) mod git_shim;
pub(crate) mod maintenance;
pub(crate) mod pager;
pub(crate) mod recovery;

use clap::{CommandFactory, Parser, Subcommand};
use commands::{
    AgentCommand, BlameArgs, CheckpointArgs, CiArgs, CloneArgs, CommitArgs, DiffArgs, FetchArgs,
    IntegrateArgs, IssueCommand, McpArgs, MergeArgs, OplogArgs, PrCommand, PullArgs, PushArgs,
    RadarArgs, ResolveArgs, SetupArgs, StackCommand, StatusArgs, SyncArgs, UndoArgs, WsCommand,
    handle_agent, handle_blame, handle_checkpoint, handle_ci, handle_clone, handle_commit,
    handle_diff, handle_fetch, handle_integrate, handle_issue, handle_mcp, handle_merge,
    handle_oplog, handle_pr, handle_pull, handle_push, handle_radar, handle_redo, handle_resolve,
    handle_setup, handle_stack, handle_status, handle_sync, handle_undo, handle_ws,
    print_json_envelope, write_stdout_line,
};
use daily::{
    BranchArgs, CherryPickArgs, CompletionsArgs, GrepArgs, LogArgs, RestoreArgs, RevParseArgs,
    RevertArgs, ShowArgs, StashArgs, SwitchArgs, TagArgs, handle_branch, handle_cherry_pick,
    handle_completions, handle_diff_paged, handle_grep, handle_log, handle_restore,
    handle_rev_parse, handle_revert, handle_show, handle_stash, handle_switch, handle_tag,
};
#[doc(hidden)]
pub use git_shim::resolve_real_git;
use oz_core::{
    BackendKind, CliInvocation, Envelope, ExitCode, FilesystemKind, InteractionMode, MountHealth,
    MountRecord, OzError, OzoneRepo, SshSigningReport, expand_tilde_path, find_git_ozone_on_path,
    inspect_ssh_signing_config, probe_mount_health_os, resolve_signing_private_key,
};
use oz_cow::{FilesystemProbe, LinuxFuseReadiness, detect_filesystem, select_backend};
use oz_github::{GitHubAuthProbe, probe_github_auth};
use oz_vfsd::{PROJFS_ENABLE_POWERSHELL_CMD, find_oz_vfsd_bin, ping_daemon, projected_fs_dll_path};
#[doc(hidden)]
pub use pager::{PagerChoice, PagerSelectionInput, resolve_pager_spec};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

const CLI_HELP_TEMPLATE: &str = "\
{before-help}{about-with-newline}
{usage-heading} {usage}

Options:
{options}{after-help}";

/// Domain headings used to organize top-level subcommands in `oz --help`.
const HELP_COMMAND_GROUPS: &[(&str, &[&str])] = &[
    (
        "Inspection",
        &[
            "status",
            "diff",
            "log",
            "show",
            "grep",
            "blame",
            "rev-parse",
        ],
    ),
    (
        "Editing & Undo",
        &[
            "commit",
            "checkpoint",
            "oplog",
            "undo",
            "redo",
            "restore",
            "stash",
        ],
    ),
    (
        "Branches & History",
        &["branch", "switch", "merge", "cherry-pick", "revert", "tag"],
    ),
    (
        "Workspaces & Stacks",
        &["ws", "radar", "integrate", "resolve", "stack", "sync"],
    ),
    (
        "Remotes & GitHub (PR / Issue / CI)",
        &["clone", "fetch", "pull", "push", "pr", "issue", "ci"],
    ),
    (
        "Maintenance, Agents & Setup",
        &[
            "agent",
            "mcp",
            "setup",
            "doctor",
            "gc",
            "fsck",
            "git",
            "completions",
            "version",
        ],
    ),
];

fn grouped_subcommands_help() -> &'static str {
    static RENDERED: OnceLock<String> = OnceLock::new();
    RENDERED.get_or_init(|| {
        let probe = Commands::augment_subcommands(clap::Command::new("oz"));
        let mut descriptions: HashMap<String, String> = HashMap::new();
        let mut all_names: Vec<String> = Vec::new();
        for sub in probe.get_subcommands() {
            if sub.is_hide_set() {
                continue;
            }
            let name = sub.get_name().to_string();
            let about = sub
                .get_about()
                .map(std::string::ToString::to_string)
                .unwrap_or_default();
            all_names.push(name.clone());
            descriptions.insert(name, about);
        }

        let name_width = all_names
            .iter()
            .map(String::len)
            .max()
            .unwrap_or(10)
            .max(11);

        let mut assigned: HashSet<&str> = HashSet::new();
        let mut out = String::new();
        for (heading, names) in HELP_COMMAND_GROUPS {
            let _ = writeln!(out, "{heading}:");
            for &name in *names {
                assigned.insert(name);
                if let Some(about) = descriptions.get(name) {
                    let _ = writeln!(out, "  {name:<name_width$}  {about}");
                }
            }
            out.push('\n');
        }

        let mut unassigned: Vec<&str> = all_names
            .iter()
            .map(String::as_str)
            .filter(|name| !assigned.contains(name) && *name != "help")
            .collect();
        if !unassigned.is_empty() {
            unassigned.sort_unstable();
            let _ = writeln!(out, "Other Commands:");
            for name in unassigned {
                if let Some(about) = descriptions.get(name) {
                    let _ = writeln!(out, "  {name:<name_width$}  {about}");
                }
            }
            out.push('\n');
        }

        let _ = write!(
            out,
            "Run `oz <command> --help` for detailed flags and usage of any subcommand."
        );
        out
    })
}

/// Top-level CLI options shared across `oz` and `git-ozone`.
#[derive(Debug, Parser)]
#[command(
    name = "oz",
    disable_version_flag = true,
    about = "Pure-Rust CoW Git client and multi-agent workspace engine",
    help_template = CLI_HELP_TEMPLATE,
    after_help = grouped_subcommands_help()
)]
pub struct Cli {
    /// Emit canonical `oz.v1` JSON output envelope.
    #[arg(long, global = true)]
    pub json: bool,

    /// Force strict non-interactive agent mode (also enabled via `OZ_AGENT=1`).
    #[arg(long, global = true)]
    pub agent: bool,

    /// Run as if `oz` was started in `<DIR>` instead of the current working directory.
    #[arg(short = 'C', long = "directory", value_name = "DIR", global = true)]
    pub directory: Option<PathBuf>,

    /// Print version information (supports `--json`).
    #[arg(short = 'V', long = "version", global = true)]
    pub version: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Supported Ozone subcommands.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Manage CoW/FUSE3 agent workspaces (`spawn`, `list`, `rm`, `gc`).
    #[command(subcommand)]
    Ws(WsCommand),
    /// Show indexless `O(dirty_files)` working-tree status with deterministic `[f1]` / `h1a` IDs.
    Status(StatusArgs),
    /// Show Histogram unified diffs with deterministic file/hunk IDs.
    Diff(DiffArgs),
    /// Stage-free commit (whole tree or `--pick` / `--exclude` partial) with guardrails & `Change-Id`.
    Commit(CommitArgs),
    /// Capture an `< 10 ms` MVCC working-tree checkpoint in the `OpLog`.
    Checkpoint(CheckpointArgs),
    /// List recent MVCC `OpLog` entries with `op_...` IDs and undo/redo cursor status (use with `oz undo [OP_ID]`).
    Oplog(OplogArgs),
    /// Sub-10 ms undo of recent `OpLog` mutations/checkpoints or un-checkpointed edits (`--list` or `oz oplog` to find `OP_ID`s).
    Undo(UndoArgs),
    /// Re-apply the most recently undone operation from the `OpLog` (inspect redo stack with `oz oplog`).
    Redo,
    /// Detect overlapping hunks, shared-file edits, and out-of-scope changes across workspaces.
    Radar(RadarArgs),
    /// Merge revision(s) (e.g. `origin/main`) into the current branch in memory (`OpLog`-reversible via `oz undo`).
    Merge(MergeArgs),
    /// Atomically integrate workspaces or branches in memory with `--verify` and conflict sandbox.
    Integrate(IntegrateArgs),
    /// Resolve conflicts inside `../<repo>.oz/_integrate` (`--take ours|theirs|base`).
    Resolve(ResolveArgs),
    /// Manage stacked branches (`rebase`, `status`) preserving `Change-Id:` trailers.
    #[command(subcommand)]
    Stack(StackCommand),
    /// Clone a remote repository over pure-Rust Git Wire Protocol v2 (supports `--filter=blob:none`).
    Clone(CloneArgs),
    /// Fetch remote updates over pure-Rust Git Wire Protocol v2 with `OpLog` `ref_deltas`.
    Fetch(FetchArgs),
    /// Fetch and incorporate upstream changes (`--ff-only`, `--rebase`, or merge) with `OpLog` undo.
    Pull(PullArgs),
    /// Push refs over pure-Rust smart `git-receive-pack` (`--atomic`, `--force-with-lease`).
    Push(PushArgs),
    /// Manage and review GitHub Pull Requests (`create`, `edit`, `list`, `status`, `view`, `comments`, `reply`, `resolve`, `diff`, `read`, `review`, `stack`).
    #[command(subcommand)]
    Pr(PrCommand),
    /// Inspect, search, and comment on GitHub Issues via REST (`view`, `list`, `comment`).
    #[command(subcommand)]
    Issue(IssueCommand),
    /// Sync with remote: fetch, fast-forward base, retarget stacked PRs, rebase, and GC merged workspaces.
    Sync(SyncArgs),
    /// Inspect and watch GitHub CI checks, workflow runs, and job logs (`--watch`, `--runs`, `--pr`, `--workflow`, `--job`).
    Ci(CiArgs),
    /// Configure coding agent harnesses (`setup` for Claude Code, Cursor, Codex CLI, Windsurf, Antigravity).
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Launch built-in JSON-RPC 2.0 Model Context Protocol (`oz mcp`) stdio server.
    Mcp(McpArgs),
    /// Show line-level human vs. AI (`refs/notes/ai`) authorship attribution for a file.
    Blame(BlameArgs),
    /// Show commit history graph with filtering (`--oneline`, `-n`, `--author`, `--grep`, `-S`, `-G`, `--since`).
    Log(LogArgs),
    /// Show commit metadata, `Change-Id`, `refs/notes/ai` provenance, diff, or `<rev>:<path>` blob (`--lines`, `--stat`, `--json`).
    Show(ShowArgs),
    /// Resolve revisions to full/short OIDs or query repo paths (`--show-toplevel`, `--is-inside-work-tree`, `--short`, `--verify`).
    #[command(name = "rev-parse")]
    RevParse(RevParseArgs),
    /// Save and restore dirty working-tree snapshots (`push`, `pop`, `list`, `drop`) backed by `OpLog`.
    Stash(StashArgs),
    /// Apply the changes introduced by an existing commit onto `HEAD` (recorded in `OpLog`).
    CherryPick(CherryPickArgs),
    /// Create a new commit reverting the changes introduced by an existing commit (recorded in `OpLog`).
    Revert(RevertArgs),
    /// Create, list, delete, and SSH-sign tags (recorded in `OpLog`).
    Tag(TagArgs),
    /// List, create, rename, and delete local or remote-tracking branches (`-a`, `-r`, recorded in `OpLog`).
    Branch(BranchArgs),
    /// Search working-tree files or any Git revision's tree (`oz grep <pat> [<rev>] [-- <paths...>]`).
    Grep(GrepArgs),
    /// Switch branches or detach `HEAD` via native `gix` tree-delta checkout (recorded in `OpLog`).
    Switch(SwitchArgs),
    /// Restore working-tree files from `HEAD` or `--source <REV>` (recorded in `OpLog` for `oz undo`).
    Restore(RestoreArgs),
    /// Generate shell completion scripts (`bash`, `zsh`, `fish`, `powershell`).
    Completions(CompletionsArgs),
    /// Initialize colocated `.git/ozone/` metadata, `systemd --user`, or Windows `ProjFS`.
    Setup(SetupArgs),
    /// Inspect OS, filesystem (`statfs`), FUSE3/reflink readiness, and colocated repo status.
    Doctor,
    /// Compact `.git/ozone/oplog/` and prune expired `OpLog` entries/snapshots safely.
    Gc(maintenance::GcArgs),
    /// Verify colocated `.git/`, `OpLog` chain, snapshot reachability, and orphaned mounts/baselines.
    Fsck(maintenance::FsckArgs),
    /// Print version information (equivalent to `--version`).
    Version,
    /// Git-compatible command shim (`alias git="oz git"`) backed by Ozone + MVCC `OpLog`.
    Git(git_shim::GitShimArgs),
}

#[derive(Debug, Serialize)]
struct VersionData {
    name: &'static str,
    invoked_as: &'static str,
    version: &'static str,
    os: &'static str,
    arch: &'static str,
    pure_rust: bool,
    non_interactive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DoctorCheckStatus {
    Ok,
    Warn,
    Fail,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DoctorCheck {
    pub(crate) id: &'static str,
    pub(crate) category: &'static str,
    pub(crate) status: DoctorCheckStatus,
    pub(crate) summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) remediation: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DoctorFilesystem {
    #[serde(rename = "type")]
    pub(crate) fs_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) magic: Option<String>,
    pub(crate) reflink_ready: bool,
    pub(crate) apfs_clonefile_ready: bool,
    pub(crate) dev_drive_refs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DoctorWindowsDiagnostics {
    pub(crate) projfs_feature: &'static str,
    pub(crate) projfs_dll_path: PathBuf,
    pub(crate) projfs_dll_exists: bool,
    pub(crate) projfs_ready: bool,
    pub(crate) dev_drive_detected: bool,
    pub(crate) refs_clone_ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DoctorMacOsDiagnostics {
    pub(crate) apfs_detected: bool,
    pub(crate) clonefile_ready: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct DoctorVfsd {
    pub(crate) found: bool,
    pub(crate) path: Option<PathBuf>,
    pub(crate) daemon_required: bool,
    pub(crate) daemon_running: bool,
    pub(crate) daemon_health: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) socket_path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DoctorGitOzoneAlias {
    pub(crate) invoked_as: &'static str,
    pub(crate) found_on_path: Option<PathBuf>,
    pub(crate) resolved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sibling_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DoctorActiveMount {
    pub(crate) workspace: String,
    pub(crate) branch: String,
    pub(crate) backend: BackendKind,
    pub(crate) mount_point: PathBuf,
    pub(crate) mounted: bool,
    pub(crate) health: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) upper_dir: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DoctorRepoStatus {
    pub(crate) in_repo: bool,
    pub(crate) colocated: bool,
    pub(crate) repo_root: Option<PathBuf>,
    pub(crate) git_dir: Option<PathBuf>,
    pub(crate) git_common_dir: Option<PathBuf>,
    pub(crate) ozone_dir: Option<PathBuf>,
    pub(crate) ozone_initialized: bool,
    pub(crate) is_linked_worktree: bool,
    pub(crate) workspace: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) head_oid: Option<String>,
    pub(crate) mount_health: Option<&'static str>,
    pub(crate) repo_config_present: bool,
    pub(crate) local_config_present: bool,
    pub(crate) active_mount_count: usize,
    pub(crate) unhealthy_mount_count: usize,
    pub(crate) active_mounts: Vec<DoctorActiveMount>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DoctorReport {
    pub(crate) invoked_as: &'static str,
    pub(crate) version: &'static str,
    pub(crate) non_interactive: bool,
    pub(crate) os: String,
    pub(crate) arch: String,
    pub(crate) target_path: PathBuf,
    pub(crate) filesystem: DoctorFilesystem,
    pub(crate) fuse: Option<LinuxFuseReadiness>,
    pub(crate) projfs_ready: Option<bool>,
    pub(crate) windows: Option<DoctorWindowsDiagnostics>,
    pub(crate) macos: Option<DoctorMacOsDiagnostics>,
    pub(crate) recommended_backend: BackendKind,
    pub(crate) oz_vfsd: DoctorVfsd,
    pub(crate) git_ozone_alias: DoctorGitOzoneAlias,
    pub(crate) repo: DoctorRepoStatus,
    pub(crate) ssh_signing: SshSigningReport,
    pub(crate) github_auth: GitHubAuthProbe,
    pub(crate) checks: Vec<DoctorCheck>,
    pub(crate) remediation_hints: Vec<String>,
}

/// Entrypoint shared by `oz` and `git-ozone` binaries.
#[must_use]
pub fn run_cli() -> std::process::ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    run_with_args(args).to_process_exit_code()
}

/// Executes the CLI against explicit argument vector and returns canonical [`ExitCode`].
pub fn run_with_args(args: Vec<OsString>) -> ExitCode {
    let invocation = CliInvocation::from_arg0(args.first().map(OsString::as_os_str));
    let raw_json_hint = args.iter().any(|a| a == "--json");
    let raw_agent_hint = args.iter().any(|a| a == "--agent");

    let args = match aliases::expand_aliases(args) {
        Ok(expanded) => expanded,
        Err(err) => {
            if raw_json_hint {
                let env = Envelope::from_error(&err);
                write_stdout_line(
                    &env.to_json_pretty()
                        .unwrap_or_else(|_| "{\"schema\":\"oz.v1\",\"ok\":false}".into()),
                );
            } else {
                eprintln!("error: {err}");
            }
            return err.exit_code();
        }
    };

    let (parse_slice, trailing_git_args) = match find_git_subcommand_index(&args) {
        Some(idx) => (&args[..=idx], Some(args[idx + 1..].to_vec())),
        None => (args.as_slice(), None),
    };

    let cli = match Cli::try_parse_from(parse_slice) {
        Ok(cli) => cli,
        Err(err) => {
            use clap::error::ErrorKind;
            if err.kind() == ErrorKind::DisplayHelp {
                let _ = err.print();
                return ExitCode::Success;
            }
            if raw_json_hint {
                let mode = InteractionMode::detect(true, raw_agent_hint);
                let _ = mode;
                let env = Envelope::failure(
                    "invalid_usage",
                    ExitCode::UsageOrGuardrail,
                    err.to_string().trim().to_string(),
                )
                .with_recovery_command("oz --help");
                write_stdout_line(
                    &env.to_json_pretty()
                        .unwrap_or_else(|_| "{\"schema\":\"oz.v1\",\"ok\":false}".into()),
                );
            } else {
                let _ = err.print();
            }
            return ExitCode::UsageOrGuardrail;
        }
    };

    let mode = InteractionMode::detect(cli.json, cli.agent);

    if cli.version || matches!(cli.command, Some(Commands::Version)) {
        return handle_version(invocation, mode);
    }

    if let Some(ref dir) = cli.directory
        && !matches!(
            cli.command,
            Some(Commands::Clone(_) | Commands::Git(_) | Commands::Version) | None
        )
        && !dir.exists()
        && probe_mount_health_os(dir) != MountHealth::DisconnectedEnotconn
    {
        let err = OzError::InvalidUsage {
            message: format!("cannot change to '{}': no such directory", dir.display()),
            recovery: vec![
                "Pass an existing directory path to `-C <DIR>` or `--directory <DIR>`.".to_string(),
            ],
        };
        if mode.json {
            let env = Envelope::from_error(&err);
            print_json_envelope(&env);
        } else {
            eprintln!("error: {err}");
        }
        return err.exit_code();
    }

    match cli.command {
        Some(Commands::Ws(ws_cmd)) => handle_ws(ws_cmd, cli.directory.as_deref(), mode),
        Some(Commands::Status(args)) => handle_status(&args, cli.directory.as_deref(), mode),
        Some(Commands::Diff(args)) => {
            if pager::pager_allowed_for_mode(mode) {
                handle_diff_paged(&args, cli.directory.as_deref(), mode)
            } else {
                handle_diff(&args, cli.directory.as_deref(), mode)
            }
        }
        Some(Commands::Commit(args)) => handle_commit(&args, cli.directory.as_deref(), mode),
        Some(Commands::Checkpoint(args)) => {
            handle_checkpoint(&args, cli.directory.as_deref(), mode)
        }
        Some(Commands::Oplog(args)) => handle_oplog(&args, cli.directory.as_deref(), mode),
        Some(Commands::Undo(args)) => handle_undo(&args, cli.directory.as_deref(), mode),
        Some(Commands::Redo) => handle_redo(cli.directory.as_deref(), mode),
        Some(Commands::Radar(args)) => handle_radar(&args, cli.directory.as_deref(), mode),
        Some(Commands::Merge(args)) => handle_merge(&args, cli.directory.as_deref(), mode),
        Some(Commands::Integrate(args)) => handle_integrate(&args, cli.directory.as_deref(), mode),
        Some(Commands::Resolve(args)) => handle_resolve(&args, cli.directory.as_deref(), mode),
        Some(Commands::Stack(stack_cmd)) => handle_stack(stack_cmd, cli.directory.as_deref(), mode),
        Some(Commands::Clone(args)) => handle_clone(&args, cli.directory.as_deref(), mode),
        Some(Commands::Fetch(args)) => handle_fetch(&args, cli.directory.as_deref(), mode),
        Some(Commands::Pull(args)) => handle_pull(&args, cli.directory.as_deref(), mode),
        Some(Commands::Push(args)) => handle_push(&args, cli.directory.as_deref(), mode),
        Some(Commands::Pr(pr_cmd)) => handle_pr(pr_cmd, cli.directory.as_deref(), mode),
        Some(Commands::Issue(issue_cmd)) => handle_issue(issue_cmd, cli.directory.as_deref(), mode),
        Some(Commands::Sync(args)) => handle_sync(&args, cli.directory.as_deref(), mode),
        Some(Commands::Ci(args)) => handle_ci(&args, cli.directory.as_deref(), mode),
        Some(Commands::Agent(agent_cmd)) => handle_agent(agent_cmd, cli.directory.as_deref(), mode),
        Some(Commands::Mcp(args)) => handle_mcp(&args, cli.directory.as_deref(), mode),
        Some(Commands::Blame(args)) => handle_blame(&args, cli.directory.as_deref(), mode),
        Some(Commands::Log(args)) => handle_log(&args, cli.directory.as_deref(), mode),
        Some(Commands::Show(args)) => handle_show(&args, cli.directory.as_deref(), mode),
        Some(Commands::RevParse(args)) => handle_rev_parse(&args, cli.directory.as_deref(), mode),
        Some(Commands::Stash(args)) => handle_stash(&args, cli.directory.as_deref(), mode),
        Some(Commands::CherryPick(args)) => {
            handle_cherry_pick(&args, cli.directory.as_deref(), mode)
        }
        Some(Commands::Revert(args)) => handle_revert(&args, cli.directory.as_deref(), mode),
        Some(Commands::Tag(args)) => handle_tag(&args, cli.directory.as_deref(), mode),
        Some(Commands::Branch(args)) => handle_branch(&args, cli.directory.as_deref(), mode),
        Some(Commands::Grep(args)) => handle_grep(&args, cli.directory.as_deref(), mode),
        Some(Commands::Switch(args)) => handle_switch(&args, cli.directory.as_deref(), mode),
        Some(Commands::Restore(args)) => handle_restore(&args, cli.directory.as_deref(), mode),
        Some(Commands::Completions(args)) => handle_completions(&args, Cli::command(), mode),
        Some(Commands::Setup(args)) => handle_setup(&args, cli.directory.as_deref(), mode),
        Some(Commands::Doctor) => handle_doctor(invocation, &cli, mode),
        Some(Commands::Gc(args)) => maintenance::handle_gc(&args, cli.directory.as_deref(), mode),
        Some(Commands::Fsck(args)) => {
            maintenance::handle_fsck(&args, cli.directory.as_deref(), mode)
        }
        Some(Commands::Version) => handle_version(invocation, mode),
        Some(Commands::Git(mut git_args)) => {
            if let Some(raw_tail) = trailing_git_args {
                git_args.args = raw_tail;
            }
            git_shim::handle_git(&git_args, cli.directory.as_deref(), mode)
        }
        None => {
            let msg = "no subcommand provided; run `oz --help` or `oz doctor --json`";
            if mode.json {
                let env = Envelope::failure("invalid_usage", ExitCode::UsageOrGuardrail, msg)
                    .with_recovery_command("oz --help")
                    .with_recovery_command("oz doctor --json");
                print_json_envelope(&env);
            } else {
                eprintln!("error: {msg}");
            }
            ExitCode::UsageOrGuardrail
        }
    }
}

fn find_git_subcommand_index(args: &[OsString]) -> Option<usize> {
    let mut i = 1usize;
    while i < args.len() {
        let s = args[i].to_string_lossy();
        if matches!(
            s.as_ref(),
            "--json" | "--agent" | "-V" | "--version" | "-h" | "--help"
        ) {
            i += 1;
            continue;
        }
        if s == "-C" || s == "--directory" {
            i += 2;
            continue;
        }
        if s.starts_with("-C") || s.starts_with("--directory=") {
            i += 1;
            continue;
        }
        if s == "--" || s.starts_with('-') {
            return None;
        }
        return (s == "git").then_some(i);
    }
    None
}

fn handle_version(invocation: CliInvocation, mode: InteractionMode) -> ExitCode {
    let data = VersionData {
        name: "oz",
        invoked_as: invocation.binary_name(),
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        pure_rust: true,
        non_interactive: mode.non_interactive,
    };

    if mode.json {
        let env = Envelope::success(data);
        print_json_envelope(&env);
    } else {
        write_stdout_line(&format!(
            "{} {}",
            invocation.binary_name(),
            env!("CARGO_PKG_VERSION")
        ));
    }
    ExitCode::Success
}

fn handle_doctor(invocation: CliInvocation, cli: &Cli, mode: InteractionMode) -> ExitCode {
    match build_doctor_envelope(invocation, cli.directory.as_deref(), mode) {
        Ok(env) => {
            if mode.json {
                print_json_envelope(&env);
            } else {
                print_doctor_human(&env);
            }
            ExitCode::Success
        }
        Err(err) => {
            if mode.json {
                let env = Envelope::from_error(&err);
                print_json_envelope(&env);
            } else {
                eprintln!("error: {err}");
            }
            err.exit_code()
        }
    }
}

/// Computes diagnostics and builds the `oz.v1` [`Envelope`] for `oz doctor`.
pub(crate) fn build_doctor_envelope(
    invocation: CliInvocation,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> oz_core::Result<Envelope<DoctorReport>> {
    let repo_result = OzoneRepo::discover(directory);

    let (repo_opt, repo_warnings) = match repo_result {
        Ok(repo) => (Some(repo), Vec::new()),
        Err(OzError::NotARepository { .. }) => (None, Vec::new()),
        Err(OzError::InvalidConfig { path, message }) => {
            return Err(OzError::InvalidConfig { path, message });
        }
        Err(err) => {
            let msg = format!("repository discovery warning: {err}");
            (None, vec![msg])
        }
    };

    let target_path = if let Some(dir) = directory {
        dir.to_path_buf()
    } else if let Some(ref repo) = repo_opt {
        repo.workdir_path.clone()
    } else {
        std::env::current_dir()
            .or_else(|_| {
                std::env::var_os("PWD")
                    .map(PathBuf::from)
                    .ok_or_else(|| std::io::Error::other("cwd unavailable"))
            })
            .unwrap_or_else(|_| PathBuf::from("."))
    };

    let probe_path = repo_opt
        .as_ref()
        .map_or(target_path.as_path(), |r| r.repo_root.as_path());
    let probe: FilesystemProbe = detect_filesystem(probe_path)?;

    let configured_backend = repo_opt.as_ref().and_then(|r| r.config.backend);
    let recommended_backend = select_backend(&probe, configured_backend);

    let vfsd_bin = find_oz_vfsd_bin().ok();
    let git_ozone_alias_path = find_git_ozone_on_path();
    let git_ozone_sibling = find_sibling_git_ozone_bin();

    let active_mounts = repo_opt
        .as_ref()
        .map(collect_active_mounts)
        .unwrap_or_default();
    let active_mount_count = active_mounts.iter().filter(|m| m.mounted).count();
    let unhealthy_mount_count = active_mounts
        .iter()
        .filter(|m| m.mounted && m.health != MountHealth::Healthy.as_str())
        .count();
    let has_active_daemon_mounts = active_mounts
        .iter()
        .any(|m| m.mounted && m.backend.requires_daemon());

    let oz_vfsd = probe_vfsd_status(
        vfsd_bin,
        recommended_backend.requires_daemon() || has_active_daemon_mounts,
        has_active_daemon_mounts,
        repo_opt.as_ref(),
    );

    let git_ozone_alias = DoctorGitOzoneAlias {
        invoked_as: invocation.binary_name(),
        resolved: git_ozone_alias_path.is_some() || invocation == CliInvocation::GitOzone,
        found_on_path: git_ozone_alias_path,
        sibling_path: git_ozone_sibling,
    };

    let mut ssh_signing = inspect_ssh_signing_for_doctor(repo_opt.as_ref());
    sanitize_ssh_signing_report(&mut ssh_signing);
    let github_auth = probe_github_auth(repo_opt.as_ref(), None, None);

    let (workspace, branch, head_oid, repo_status) = match repo_opt {
        Some(repo) => {
            let status = DoctorRepoStatus {
                in_repo: true,
                colocated: true,
                repo_root: Some(repo.repo_root.clone()),
                git_dir: Some(repo.git_dir.clone()),
                git_common_dir: Some(repo.git_common_dir.clone()),
                ozone_dir: Some(repo.layout.ozone_dir.clone()),
                ozone_initialized: repo.layout.is_initialized(),
                is_linked_worktree: repo.is_linked_worktree,
                workspace: repo.workspace.clone(),
                branch: repo.branch.clone(),
                head_oid: repo.head_oid.clone(),
                mount_health: Some(repo.mount_health.as_str()),
                repo_config_present: repo.repo_root.join(".ozone.toml").is_file(),
                local_config_present: repo.layout.local_config_path().is_file(),
                active_mount_count,
                unhealthy_mount_count,
                active_mounts,
            };
            (repo.workspace, repo.branch, repo.head_oid, status)
        }
        None => (
            None,
            None,
            None,
            DoctorRepoStatus {
                in_repo: false,
                colocated: false,
                repo_root: None,
                git_dir: None,
                git_common_dir: None,
                ozone_dir: None,
                ozone_initialized: false,
                is_linked_worktree: false,
                workspace: None,
                branch: None,
                head_oid: None,
                mount_health: None,
                repo_config_present: false,
                local_config_present: false,
                active_mount_count: 0,
                unhealthy_mount_count: 0,
                active_mounts: Vec::new(),
            },
        ),
    };

    let (windows, macos) = build_platform_diagnostics(&probe);
    let apfs_clonefile_ready = macos.as_ref().is_some_and(|m| m.clonefile_ready);
    let dev_drive_refs = windows.as_ref().is_some_and(|w| w.dev_drive_detected);

    let (checks, mut warnings, recovery_commands, remediation_hints) = evaluate_doctor_checks(
        &probe,
        recommended_backend,
        windows.as_ref(),
        macos.as_ref(),
        &oz_vfsd,
        &git_ozone_alias,
        &repo_status,
        &ssh_signing,
        &github_auth,
    );

    let mut all_warnings = repo_warnings;
    all_warnings.append(&mut warnings);

    let report = DoctorReport {
        invoked_as: invocation.binary_name(),
        version: env!("CARGO_PKG_VERSION"),
        non_interactive: mode.non_interactive,
        os: probe.os,
        arch: probe.arch,
        target_path,
        filesystem: DoctorFilesystem {
            fs_type: probe.fs_kind.as_str().to_string(),
            magic: probe.fs_magic,
            reflink_ready: probe.reflink_supported,
            apfs_clonefile_ready,
            dev_drive_refs,
        },
        fuse: probe.fuse,
        projfs_ready: probe.projfs_ready,
        windows,
        macos,
        recommended_backend,
        oz_vfsd,
        git_ozone_alias,
        repo: repo_status,
        ssh_signing,
        github_auth,
        checks,
        remediation_hints,
    };

    let mut env = Envelope::success(report)
        .with_workspace(workspace)
        .with_branch(branch)
        .with_head_oid(head_oid)
        .with_recovery_commands(recovery_commands);
    for w in all_warnings {
        env = env.with_warning(w);
    }
    Ok(env)
}

fn build_platform_diagnostics(
    probe: &FilesystemProbe,
) -> (
    Option<DoctorWindowsDiagnostics>,
    Option<DoctorMacOsDiagnostics>,
) {
    let windows = if probe.os == "windows" {
        let dll_path = projected_fs_dll_path();
        let projfs_ready = probe.projfs_ready.unwrap_or(false);
        let dev_drive_detected = probe.fs_kind == FilesystemKind::Refs;
        Some(DoctorWindowsDiagnostics {
            projfs_feature: "Client-ProjFS",
            projfs_dll_exists: projfs_ready || dll_path.is_file(),
            projfs_dll_path: dll_path,
            projfs_ready,
            dev_drive_detected,
            refs_clone_ready: dev_drive_detected && probe.reflink_supported,
        })
    } else {
        None
    };

    let macos = if probe.os == "macos" {
        let apfs_detected = probe.fs_kind == FilesystemKind::Apfs;
        Some(DoctorMacOsDiagnostics {
            apfs_detected,
            clonefile_ready: apfs_detected && probe.reflink_supported,
        })
    } else {
        None
    };

    (windows, macos)
}

fn find_sibling_git_ozone_bin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    #[cfg(windows)]
    let name = "git-ozone.exe";
    #[cfg(not(windows))]
    let name = "git-ozone";
    let cand = dir.join(name);
    cand.is_file().then_some(cand)
}

fn collect_active_mounts(repo: &OzoneRepo) -> Vec<DoctorActiveMount> {
    let ws_dir = repo.layout.ws_dir();
    let Ok(entries) = std::fs::read_dir(&ws_dir) else {
        return Vec::new();
    };

    let mut mounts = Vec::new();
    for entry in entries.flatten() {
        let mount_json = entry.path().join("mount.json");
        let Ok(raw) = std::fs::read_to_string(&mount_json) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<MountRecord>(&raw) else {
            continue;
        };
        let health = probe_mount_health_os(&record.mount_point).as_str();
        mounts.push(DoctorActiveMount {
            workspace: record.workspace,
            branch: record.branch,
            backend: record.backend,
            mount_point: record.mount_point,
            mounted: record.mounted,
            health,
            daemon_pid: record.daemon_pid,
            upper_dir: record.upper_dir,
        });
    }
    mounts.sort_by(|a, b| a.workspace.cmp(&b.workspace));
    mounts
}

fn probe_vfsd_status(
    vfsd_bin: Option<PathBuf>,
    daemon_required: bool,
    has_active_daemon_mounts: bool,
    repo_opt: Option<&OzoneRepo>,
) -> DoctorVfsd {
    let found = vfsd_bin.is_some();
    let socket_path = repo_opt
        .filter(|r| r.layout.is_initialized())
        .map(|r| r.layout.vfsd_socket_path());

    let pong = socket_path
        .as_deref()
        .filter(|p| p.exists())
        .and_then(|p| ping_daemon(p).ok());

    let (daemon_running, daemon_pid, daemon_version) = match pong {
        Some(p) => (true, Some(p.pid), Some(p.version)),
        None => (false, None, None),
    };

    let daemon_health = if !found && daemon_required {
        "binary_missing"
    } else if daemon_running {
        "running"
    } else if has_active_daemon_mounts
        || repo_opt.is_some_and(|r| r.mount_health == MountHealth::DisconnectedEnotconn)
    {
        "stopped_with_active_mounts"
    } else if daemon_required {
        "stopped_idle"
    } else {
        "not_required"
    };

    DoctorVfsd {
        found,
        path: vfsd_bin,
        daemon_required,
        daemon_running,
        daemon_health,
        daemon_pid,
        daemon_version,
        socket_path,
    }
}

fn push_unique(target: &mut Vec<String>, item: impl Into<String>) {
    let s = item.into();
    if !s.is_empty() && !target.contains(&s) {
        target.push(s);
    }
}

#[allow(clippy::too_many_arguments)]
fn evaluate_doctor_checks(
    probe: &FilesystemProbe,
    recommended_backend: BackendKind,
    windows: Option<&DoctorWindowsDiagnostics>,
    macos: Option<&DoctorMacOsDiagnostics>,
    oz_vfsd: &DoctorVfsd,
    git_ozone_alias: &DoctorGitOzoneAlias,
    repo: &DoctorRepoStatus,
    ssh_signing: &SshSigningReport,
    github_auth: &GitHubAuthProbe,
) -> (Vec<DoctorCheck>, Vec<String>, Vec<String>, Vec<String>) {
    let mut checks = Vec::with_capacity(8);
    let mut warnings = Vec::new();
    let mut recovery_commands = Vec::new();
    let mut remediation_hints = Vec::new();

    // 1. Linux `/dev/fuse` and `fusermount3`
    if let Some(ref fuse) = probe.fuse {
        if fuse.ready {
            let ver = fuse
                .fusermount3_version
                .as_deref()
                .unwrap_or("version unknown");
            checks.push(DoctorCheck {
                id: "linux_fuse",
                category: "platform",
                status: DoctorCheckStatus::Ok,
                summary: format!("/dev/fuse is read/write and fusermount3 is ready ({ver})"),
                remediation: Vec::new(),
            });
        } else {
            let mut rem = Vec::new();
            if !fuse.dev_fuse_exists {
                rem.push(
                    "sudo modprobe fuse  # or run container with --device /dev/fuse".to_string(),
                );
            } else if !fuse.dev_fuse_rw {
                rem.push(
                    "sudo usermod -aG fuse $USER  # then re-login for /dev/fuse rw".to_string(),
                );
            }
            if fuse.fusermount3_path.is_none() {
                rem.push("sudo apt-get install -y fuse3".to_string());
            }
            if fuse.no_new_privs && !fuse.daemon_running {
                rem.push(
                    "oz setup --systemd-user  # run once from a regular shell so oz-vfsd mounts FUSE3 natively even when agents run under NoNewPrivs".to_string(),
                );
            }
            if probe.reflink_supported {
                checks.push(DoctorCheck {
                    id: "linux_fuse",
                    category: "platform",
                    status: DoctorCheckStatus::Ok,
                    summary: format!(
                        "Native {} reflinks available; Linux FUSE3 is optional",
                        probe.fs_kind
                    ),
                    remediation: Vec::new(),
                });
            } else {
                let warn_msg = if fuse.no_new_privs
                    && !fuse.daemon_running
                    && fuse.dev_fuse_rw
                    && fuse.fusermount3_path.is_some()
                {
                    "Linux NoNewPrivs=1 blocks setuid fusermount3 in this process and oz-vfsd is not running; run `oz setup --systemd-user` once from a regular shell for native FUSE3, or use cas_hardlink"
                } else {
                    "Linux FUSE3 readiness incomplete (/dev/fuse rw or fusermount3 missing); will fall back to cas_hardlink"
                };
                warnings.push(warn_msg.to_string());
                for r in &rem {
                    push_unique(&mut recovery_commands, r.clone());
                    push_unique(&mut remediation_hints, r.clone());
                }
                checks.push(DoctorCheck {
                    id: "linux_fuse",
                    category: "platform",
                    status: DoctorCheckStatus::Warn,
                    summary: format!(
                        "Linux FUSE3 incomplete (dev_fuse_exists={}, dev_fuse_rw={}, fusermount3={}, no_new_privs={}, daemon_running={}); falling back to cas_hardlink",
                        fuse.dev_fuse_exists,
                        fuse.dev_fuse_rw,
                        fuse.fusermount3_path.is_some(),
                        fuse.no_new_privs,
                        fuse.daemon_running
                    ),
                    remediation: rem,
                });
            }
        }
    } else {
        checks.push(DoctorCheck {
            id: "linux_fuse",
            category: "platform",
            status: DoctorCheckStatus::NotApplicable,
            summary: format!("Linux FUSE3 check not applicable on {}", probe.os),
            remediation: Vec::new(),
        });
    }

    // 2. Windows `Client-ProjFS` / `ProjectedFSLib.dll` and Dev Drive / ReFS
    if let Some(win) = windows {
        if win.dev_drive_detected && win.refs_clone_ready {
            checks.push(DoctorCheck {
                id: "windows_projfs_dev_drive",
                category: "platform",
                status: DoctorCheckStatus::Ok,
                summary: format!(
                    "Windows Dev Drive (ReFS) detected with native block-clone support (Client-ProjFS ready={})",
                    win.projfs_ready
                ),
                remediation: Vec::new(),
            });
        } else if win.projfs_ready {
            checks.push(DoctorCheck {
                id: "windows_projfs_dev_drive",
                category: "platform",
                status: DoctorCheckStatus::Ok,
                summary: format!(
                    "Windows {} ({}) ready; Dev Drive (ReFS) not detected on target volume",
                    win.projfs_feature,
                    win.projfs_dll_path.display()
                ),
                remediation: Vec::new(),
            });
        } else {
            let rem = vec![
                "oz setup --enable-projfs".to_string(),
                PROJFS_ENABLE_POWERSHELL_CMD.to_string(),
                "Optional: move repositories onto a Windows Dev Drive (ReFS) for native zero-copy clones"
                    .to_string(),
            ];
            warnings.push(
                "Windows Client-ProjFS (ProjectedFSLib.dll) is not enabled and target volume is not Dev Drive (ReFS); will fall back to cas_hardlink"
                    .to_string(),
            );
            push_unique(&mut recovery_commands, "oz setup --enable-projfs");
            for r in &rem {
                push_unique(&mut remediation_hints, r.clone());
            }
            checks.push(DoctorCheck {
                id: "windows_projfs_dev_drive",
                category: "platform",
                status: DoctorCheckStatus::Warn,
                summary: format!(
                    "Windows {} ({}) missing and Dev Drive (ReFS) not detected",
                    win.projfs_feature,
                    win.projfs_dll_path.display()
                ),
                remediation: rem,
            });
        }
    } else {
        checks.push(DoctorCheck {
            id: "windows_projfs_dev_drive",
            category: "platform",
            status: DoctorCheckStatus::NotApplicable,
            summary: format!(
                "Windows Client-ProjFS and Dev Drive (ReFS) check not applicable on {}",
                probe.os
            ),
            remediation: Vec::new(),
        });
    }

    // 3. macOS APFS / `clonefile`
    if let Some(mac) = macos {
        if mac.apfs_detected && mac.clonefile_ready {
            checks.push(DoctorCheck {
                id: "macos_apfs_clonefile",
                category: "platform",
                status: DoctorCheckStatus::Ok,
                summary: "macOS APFS detected with native clonefile(2) CoW support".to_string(),
                remediation: Vec::new(),
            });
        } else {
            let rem = vec![
                "Store repositories on an APFS volume to enable zero-byte clonefile(2) workspaces"
                    .to_string(),
            ];
            warnings.push(format!(
                "macOS APFS clonefile(2) unavailable on {} filesystem; will fall back to cas_hardlink",
                probe.fs_kind
            ));
            for r in &rem {
                push_unique(&mut remediation_hints, r.clone());
            }
            checks.push(DoctorCheck {
                id: "macos_apfs_clonefile",
                category: "platform",
                status: DoctorCheckStatus::Warn,
                summary: format!(
                    "macOS APFS clonefile(2) not ready (apfs={}, clonefile_ready={})",
                    mac.apfs_detected, mac.clonefile_ready
                ),
                remediation: rem,
            });
        }
    } else {
        checks.push(DoctorCheck {
            id: "macos_apfs_clonefile",
            category: "platform",
            status: DoctorCheckStatus::NotApplicable,
            summary: format!(
                "macOS APFS clonefile(2) check not applicable on {}",
                probe.os
            ),
            remediation: Vec::new(),
        });
    }

    // 4. `oz-vfsd` binary discovery & daemon health
    match oz_vfsd.daemon_health {
        "binary_missing" => {
            let rem = vec![
                "Install `oz-vfsd` alongside `oz` or export OZ_VFSD_BIN=/path/to/oz-vfsd"
                    .to_string(),
            ];
            warnings.push(format!(
                "oz-vfsd binary not found on PATH or next to oz (required by recommended backend '{recommended_backend}')"
            ));
            for r in &rem {
                push_unique(&mut recovery_commands, r.clone());
                push_unique(&mut remediation_hints, r.clone());
            }
            checks.push(DoctorCheck {
                id: "oz_vfsd",
                category: "daemon",
                status: DoctorCheckStatus::Fail,
                summary: format!(
                    "oz-vfsd binary missing (required for {recommended_backend} backend)"
                ),
                remediation: rem,
            });
        }
        "stopped_with_active_mounts" => {
            let rem = vec![
                "oz ws list --json".to_string(),
                "oz status --json".to_string(),
            ];
            warnings.push(
                "oz-vfsd daemon is not running while VFS workspace mounts are registered; run any workspace command to auto-remount"
                    .to_string(),
            );
            for r in &rem {
                push_unique(&mut recovery_commands, r.clone());
                push_unique(&mut remediation_hints, r.clone());
            }
            checks.push(DoctorCheck {
                id: "oz_vfsd",
                category: "daemon",
                status: DoctorCheckStatus::Warn,
                summary: "oz-vfsd daemon stopped with active registered VFS mounts".to_string(),
                remediation: rem,
            });
        }
        "running" => {
            let version_mismatch = oz_vfsd
                .daemon_version
                .as_deref()
                .is_some_and(|v| v != env!("CARGO_PKG_VERSION"));
            if version_mismatch {
                let rem = vec![
                    "Restart oz-vfsd so the running daemon matches the installed oz CLI version"
                        .to_string(),
                ];
                warnings.push(format!(
                    "Running oz-vfsd daemon version ({}) differs from oz CLI version ({})",
                    oz_vfsd.daemon_version.as_deref().unwrap_or("unknown"),
                    env!("CARGO_PKG_VERSION")
                ));
                for r in &rem {
                    push_unique(&mut remediation_hints, r.clone());
                }
                checks.push(DoctorCheck {
                    id: "oz_vfsd",
                    category: "daemon",
                    status: DoctorCheckStatus::Warn,
                    summary: format!(
                        "oz-vfsd running (pid={}, version={}) but CLI is {}",
                        oz_vfsd.daemon_pid.unwrap_or(0),
                        oz_vfsd.daemon_version.as_deref().unwrap_or("unknown"),
                        env!("CARGO_PKG_VERSION")
                    ),
                    remediation: rem,
                });
            } else {
                checks.push(DoctorCheck {
                    id: "oz_vfsd",
                    category: "daemon",
                    status: DoctorCheckStatus::Ok,
                    summary: format!(
                        "oz-vfsd binary found and daemon healthy (pid={}, version={})",
                        oz_vfsd.daemon_pid.unwrap_or(0),
                        oz_vfsd
                            .daemon_version
                            .as_deref()
                            .unwrap_or(env!("CARGO_PKG_VERSION"))
                    ),
                    remediation: Vec::new(),
                });
            }
        }
        "stopped_idle" => {
            checks.push(DoctorCheck {
                id: "oz_vfsd",
                category: "daemon",
                status: DoctorCheckStatus::Ok,
                summary: format!(
                    "oz-vfsd binary found at {} (daemon idle; auto-spawns on workspace mount)",
                    oz_vfsd
                        .path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ),
                remediation: Vec::new(),
            });
        }
        _ => {
            checks.push(DoctorCheck {
                id: "oz_vfsd",
                category: "daemon",
                status: DoctorCheckStatus::Ok,
                summary: if oz_vfsd.found {
                    format!("oz-vfsd binary found (not required by {recommended_backend} backend)")
                } else {
                    format!("oz-vfsd daemon not required by {recommended_backend} backend")
                },
                remediation: Vec::new(),
            });
        }
    }

    // 5. `git-ozone` alias resolution
    if let Some(ref alias_path) = git_ozone_alias.found_on_path {
        checks.push(DoctorCheck {
            id: "git_ozone_alias",
            category: "cli",
            status: DoctorCheckStatus::Ok,
            summary: format!("`git ozone` alias resolves via {}", alias_path.display()),
            remediation: Vec::new(),
        });
    } else if git_ozone_alias.resolved {
        checks.push(DoctorCheck {
            id: "git_ozone_alias",
            category: "cli",
            status: DoctorCheckStatus::Ok,
            summary: "Invoked directly as `git-ozone`".to_string(),
            remediation: Vec::new(),
        });
    } else {
        let rem = if let Some(ref sibling) = git_ozone_alias.sibling_path {
            let parent = sibling
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            vec![format!(
                "export PATH=\"{parent}:$PATH\"  # enables `git ozone <cmd>`"
            )]
        } else {
            vec![
                "Install the `git-ozone` binary into a directory on PATH (e.g. ~/.local/bin or /usr/local/bin)"
                    .to_string(),
            ]
        };
        for r in &rem {
            push_unique(&mut remediation_hints, r.clone());
        }
        checks.push(DoctorCheck {
            id: "git_ozone_alias",
            category: "cli",
            status: DoctorCheckStatus::Warn,
            summary: "`git-ozone` binary not found on PATH; `git ozone <cmd>` alias unavailable"
                .to_string(),
            remediation: rem,
        });
    }

    // 6. SSH signing keys
    if ssh_signing.ready && ssh_signing.issues.is_empty() {
        let fp = ssh_signing.fingerprint.as_deref().unwrap_or("unknown");
        checks.push(DoctorCheck {
            id: "ssh_signing",
            category: "security",
            status: DoctorCheckStatus::Ok,
            summary: if ssh_signing.enabled_by_default {
                format!("SSH commit signing enabled by default and ready ({fp})")
            } else {
                format!(
                    "Ed25519 SSH signing key ready ({fp}); pass `oz commit -S` or enable by default"
                )
            },
            remediation: Vec::new(),
        });
    } else {
        let mut rem = ssh_signing.recovery_commands.clone();
        if rem.is_empty() {
            rem.push("ssh-keygen -t ed25519 -N \"\" -f ~/.ssh/id_ed25519".to_string());
            rem.push("git config --global gpg.format ssh".to_string());
            rem.push("git config --global user.signingkey ~/.ssh/id_ed25519".to_string());
        }
        if ssh_signing.enabled_by_default {
            warnings.push(format!(
                "SSH commit signing is enabled by default but not ready: {}",
                ssh_signing
                    .issues
                    .first()
                    .map_or("no valid unencrypted Ed25519 signing key found", String::as_str)
            ));
            for r in &rem {
                push_unique(&mut recovery_commands, r.clone());
            }
        }
        for r in &rem {
            push_unique(&mut remediation_hints, r.clone());
        }
        let summary = if ssh_signing.issues.is_empty() {
            "No valid unencrypted Ed25519 SSH signing key configured".to_string()
        } else {
            ssh_signing.issues.join("; ")
        };
        checks.push(DoctorCheck {
            id: "ssh_signing",
            category: "security",
            status: DoctorCheckStatus::Warn,
            summary,
            remediation: rem,
        });
    }

    // 7. GitHub auth availability
    if github_auth.authenticated {
        let source = github_auth
            .token_source
            .as_ref()
            .map_or("unknown", oz_github::TokenSource::label);
        let masked = github_auth.masked_token.as_deref().unwrap_or("****");
        checks.push(DoctorCheck {
            id: "github_auth",
            category: "auth",
            status: DoctorCheckStatus::Ok,
            summary: format!(
                "GitHub auth ready for {} via {source} ({masked})",
                github_auth.host
            ),
            remediation: Vec::new(),
        });
    } else {
        let host_clean = github_auth
            .host
            .split(':')
            .next()
            .unwrap_or(&github_auth.host);
        let rem = vec![
            "export GITHUB_TOKEN=<personal-access-token>".to_string(),
            format!("gh auth login --hostname {host_clean}"),
        ];
        for r in &rem {
            push_unique(&mut remediation_hints, r.clone());
        }
        checks.push(DoctorCheck {
            id: "github_auth",
            category: "auth",
            status: DoctorCheckStatus::Warn,
            summary: format!(
                "No GitHub token discovered for {} (checked OZ_GITHUB_TOKEN, GITHUB_TOKEN, GH_TOKEN, gh hosts.yml, git credential helper)",
                github_auth.host
            ),
            remediation: rem,
        });
    }

    // 8. Repository status & active mounts
    if !repo.in_repo {
        checks.push(DoctorCheck {
            id: "repository_mounts",
            category: "repository",
            status: DoctorCheckStatus::Ok,
            summary: "Not inside a Git repository; OS and toolchain diagnostics completed"
                .to_string(),
            remediation: Vec::new(),
        });
    } else if repo.mount_health == Some(MountHealth::DisconnectedEnotconn.as_str())
        || repo.unhealthy_mount_count > 0
    {
        if repo.mount_health == Some(MountHealth::DisconnectedEnotconn.as_str()) {
            warnings.push(format!(
                "Workspace '{}' mountpoint returned ENOTCONN; real host disk .git/ozone state remains intact",
                repo.workspace.as_deref().unwrap_or("unknown")
            ));
        }
        if repo.unhealthy_mount_count > 0 {
            warnings.push(format!(
                "{} registered workspace mount(s) reported unhealthy status",
                repo.unhealthy_mount_count
            ));
        }
        let rem = vec![
            "oz ws list --json".to_string(),
            "oz status --json".to_string(),
        ];
        for r in &rem {
            push_unique(&mut recovery_commands, r.clone());
            push_unique(&mut remediation_hints, r.clone());
        }
        checks.push(DoctorCheck {
            id: "repository_mounts",
            category: "repository",
            status: DoctorCheckStatus::Warn,
            summary: format!(
                "Colocated repo detected with {} unhealthy mount(s) out of {} active",
                repo.unhealthy_mount_count.max(usize::from(
                    repo.mount_health == Some(MountHealth::DisconnectedEnotconn.as_str())
                )),
                repo.active_mount_count
            ),
            remediation: rem,
        });
    } else {
        checks.push(DoctorCheck {
            id: "repository_mounts",
            category: "repository",
            status: DoctorCheckStatus::Ok,
            summary: format!(
                "Colocated repo ready (branch: {}, ozone_initialized: {}, active_mounts: {})",
                repo.branch.as_deref().unwrap_or("detached"),
                repo.ozone_initialized,
                repo.active_mount_count
            ),
            remediation: Vec::new(),
        });
    }

    (checks, warnings, recovery_commands, remediation_hints)
}

fn sanitize_ssh_signing_report(report: &mut SshSigningReport) {
    let redact = |opt: &mut Option<String>| {
        if let Some(val) = opt.as_ref()
            && (val.contains("PRIVATE KEY") || val.contains("-----BEGIN "))
        {
            *opt = Some("<redacted-inline-private-key>".to_string());
        }
    };
    redact(&mut report.configured_key);
    redact(&mut report.git_user_signingkey);
    redact(&mut report.ozone_signing_key);
}

fn read_global_git_config(key: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["config", "--global", "--get", key])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let val = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!val.is_empty()).then_some(val)
}

fn parse_git_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn inspect_ssh_signing_for_doctor(repo_opt: Option<&OzoneRepo>) -> SshSigningReport {
    if let Some(repo) = repo_opt {
        return match inspect_ssh_signing_config(repo) {
            Ok(report) => report,
            Err(err) => SshSigningReport {
                enabled_by_default: false,
                ready: false,
                key_source: None,
                configured_key: None,
                resolved_key_path: None,
                key_algorithm: None,
                key_encrypted: false,
                fingerprint: None,
                public_key: None,
                git_gpg_format: None,
                git_user_signingkey: None,
                git_commit_gpgsign: None,
                git_allowed_signers_file: None,
                ozone_signing_enabled: None,
                ozone_signing_key: None,
                issues: vec![err.to_string()],
                recovery_commands: err.recovery_commands(),
            },
        };
    }

    let git_gpg_format = read_global_git_config("gpg.format");
    let git_user_signingkey = read_global_git_config("user.signingkey");
    let git_commit_gpgsign = read_global_git_config("commit.gpgsign")
        .as_deref()
        .and_then(parse_git_bool);
    let git_allowed_signers_file =
        read_global_git_config("gpg.ssh.allowedSignersFile").map(|v| expand_tilde_path(&v));

    let enabled_by_default = git_commit_gpgsign == Some(true)
        && git_gpg_format
            .as_deref()
            .is_some_and(|f| f.eq_ignore_ascii_case("ssh"));

    let mut issues = Vec::new();
    let mut recovery_commands = Vec::new();

    if git_commit_gpgsign == Some(true)
        && !git_gpg_format
            .as_deref()
            .is_some_and(|f| f.eq_ignore_ascii_case("ssh"))
    {
        issues.push(format!(
            "git commit.gpgsign is true, but gpg.format is '{}' (set gpg.format=ssh for pure-Rust SSH signing)",
            git_gpg_format.as_deref().unwrap_or("openpgp")
        ));
        recovery_commands.push("git config --global gpg.format ssh".to_string());
    }

    let (key_source, configured_key) = if let Some(ref k) = git_user_signingkey {
        (Some("git_config".to_string()), Some(k.clone()))
    } else {
        let default_spec = "~/.ssh/id_ed25519";
        let expanded = expand_tilde_path(default_spec);
        if expanded.is_file() {
            (
                Some("default_ssh_dir".to_string()),
                Some(default_spec.to_string()),
            )
        } else {
            (None, None)
        }
    };

    let mut ready = false;
    let mut resolved_key_path = None;
    let mut key_algorithm = None;
    let mut key_encrypted = false;
    let mut fingerprint = None;
    let mut public_key = None;

    if let Some(ref spec) = configured_key {
        match resolve_signing_private_key(spec, None) {
            Ok((priv_key, path_opt)) => {
                let pub_key = priv_key.public_key();
                ready = true;
                resolved_key_path = path_opt;
                key_algorithm = Some("ssh-ed25519".to_string());
                fingerprint = Some(pub_key.fingerprint_sha256());
                public_key = Some(pub_key.to_openssh_line());
            }
            Err(err) => {
                let msg = err.to_string();
                if msg.contains("encrypted") {
                    key_encrypted = true;
                    key_algorithm = Some("ssh-ed25519".to_string());
                }
                let candidate = expand_tilde_path(spec);
                if candidate.exists() {
                    resolved_key_path = Some(candidate);
                }
                issues.push(msg);
                for cmd in err.recovery_commands() {
                    push_unique(&mut recovery_commands, cmd);
                }
            }
        }
    }

    SshSigningReport {
        enabled_by_default,
        ready,
        key_source,
        configured_key,
        resolved_key_path,
        key_algorithm,
        key_encrypted,
        fingerprint,
        public_key,
        git_gpg_format,
        git_user_signingkey,
        git_commit_gpgsign,
        git_allowed_signers_file,
        ozone_signing_enabled: None,
        ozone_signing_key: None,
        issues,
        recovery_commands,
    }
}

fn print_doctor_human(env: &Envelope<DoctorReport>) {
    let d = &env.data;
    println!("Ozone Doctor ({} {})", d.invoked_as, d.version);
    println!("  OS / Arch:           {} / {}", d.os, d.arch);
    println!(
        "  Filesystem:          {} (reflink_ready: {})",
        d.filesystem.fs_type, d.filesystem.reflink_ready
    );
    if let Some(ref fuse) = d.fuse {
        println!(
            "  Linux FUSE3:         ready={} (/dev/fuse rw={}, fusermount3={})",
            fuse.ready,
            fuse.dev_fuse_rw,
            fuse.fusermount3_path
                .as_ref()
                .map_or_else(|| "missing".to_string(), |p| p.display().to_string())
        );
    }
    if let Some(ref win) = d.windows {
        println!(
            "  Windows ProjFS:      ready={} ({}, dll={})",
            win.projfs_ready,
            win.projfs_feature,
            win.projfs_dll_path.display()
        );
        println!(
            "  Windows Dev Drive:   detected={} (refs_clone_ready={})",
            win.dev_drive_detected, win.refs_clone_ready
        );
    } else if let Some(projfs) = d.projfs_ready {
        println!("  Windows ProjFS:      ready={projfs}");
    }
    if let Some(ref mac) = d.macos {
        println!(
            "  macOS APFS:          apfs={} (clonefile_ready={})",
            mac.apfs_detected, mac.clonefile_ready
        );
    }
    println!("  Recommended backend: {}", d.recommended_backend);
    println!(
        "  oz-vfsd:             {} (health={}, running={})",
        d.oz_vfsd
            .path
            .as_ref()
            .map_or_else(|| "not found".to_string(), |p| p.display().to_string()),
        d.oz_vfsd.daemon_health,
        d.oz_vfsd.daemon_running
    );
    println!(
        "  git-ozone alias:     resolved={} ({})",
        d.git_ozone_alias.resolved,
        d.git_ozone_alias
            .found_on_path
            .as_ref()
            .map_or_else(|| "not on PATH".to_string(), |p| p.display().to_string())
    );
    println!(
        "  SSH signing:         ready={} (default={}, fingerprint={})",
        d.ssh_signing.ready,
        d.ssh_signing.enabled_by_default,
        d.ssh_signing.fingerprint.as_deref().unwrap_or("none")
    );
    println!(
        "  GitHub auth:         authenticated={} (host={}, source={})",
        d.github_auth.authenticated,
        d.github_auth.host,
        d.github_auth
            .token_source
            .as_ref()
            .map_or("none", oz_github::TokenSource::label)
    );
    if d.repo.in_repo {
        println!(
            "  Repository:          {} (branch: {}, ozone_initialized: {}, active_mounts: {})",
            d.repo
                .repo_root
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            d.repo.branch.as_deref().unwrap_or("detached"),
            d.repo.ozone_initialized,
            d.repo.active_mount_count
        );
        for m in &d.repo.active_mounts {
            println!(
                "    mount [{}]: {} ({}, health={})",
                m.workspace,
                m.mount_point.display(),
                m.backend,
                m.health
            );
        }
    } else {
        println!("  Repository:          not inside a git repository");
    }
    if !d.remediation_hints.is_empty() {
        println!("  Remediation hints:");
        for hint in &d.remediation_hints {
            println!("    - {hint}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oz_core::OZ_SCHEMA_V1;
    use tempfile::TempDir;

    #[test]
    fn version_json_and_doctor_json_produce_valid_oz_v1_envelope() {
        let mode = InteractionMode::from_input(oz_core::InteractionInput {
            json_flag: true,
            agent_flag: true,
            oz_agent_env: Some("1"),
            ci_env: None,
            stdout_is_tty: false,
            stdin_is_tty: false,
        });

        let dir = TempDir::new().expect("tempdir");
        let init = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir.path())
            .output()
            .expect("git init");
        assert!(init.status.success());

        let env = build_doctor_envelope(CliInvocation::GitOzone, Some(dir.path()), mode)
            .expect("doctor envelope");
        assert_eq!(env.schema, OZ_SCHEMA_V1);
        assert!(env.ok);
        assert_eq!(env.branch.as_deref(), Some("main"));
        assert!(env.data.repo.in_repo);
        assert!(env.data.repo.colocated);
        assert_eq!(env.data.invoked_as, "git-ozone");
        assert_eq!(env.data.checks.len(), 8);

        let json = env.to_json_pretty().expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema"], "oz.v1");
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"]["repo"]["in_repo"], true);
        assert!(parsed["data"]["ssh_signing"].is_object());
        assert!(parsed["data"]["github_auth"].is_object());
        assert!(parsed["data"]["checks"].is_array());
        assert!(parsed["data"]["remediation_hints"].is_array());
    }

    #[test]
    fn cli_parses_version_and_doctor_flags_and_returns_expected_exit_codes() {
        let dir = TempDir::new().expect("tempdir");
        assert_eq!(
            run_with_args(vec!["oz".into(), "--version".into(), "--json".into()]),
            ExitCode::Success
        );
        assert_eq!(
            run_with_args(vec![
                "git-ozone".into(),
                "-C".into(),
                dir.path().as_os_str().to_os_string(),
                "--agent".into(),
                "doctor".into(),
                "--json".into(),
            ]),
            ExitCode::Success
        );
        assert_eq!(
            run_with_args(vec!["oz".into(), "--unknown-flag".into(), "--json".into()]),
            ExitCode::UsageOrGuardrail
        );
        let missing = dir.path().join("does-not-exist");
        assert_eq!(
            run_with_args(vec![
                "oz".into(),
                "-C".into(),
                missing.as_os_str().to_os_string(),
                "doctor".into(),
                "--json".into(),
            ]),
            ExitCode::UsageOrGuardrail
        );
    }

    #[test]
    fn sanitize_ssh_signing_report_redacts_inline_private_keys() {
        let mut report = SshSigningReport {
            enabled_by_default: true,
            ready: true,
            key_source: Some("ozone_config".to_string()),
            configured_key: Some(
                "-----BEGIN OPENSSH PRIVATE KEY-----\nSECRET_MATERIAL\n-----END OPENSSH PRIVATE KEY-----"
                    .to_string(),
            ),
            resolved_key_path: None,
            key_algorithm: Some("ssh-ed25519".to_string()),
            key_encrypted: false,
            fingerprint: Some("SHA256:test".to_string()),
            public_key: Some("ssh-ed25519 AAAA".to_string()),
            git_gpg_format: Some("ssh".to_string()),
            git_user_signingkey: Some("-----BEGIN OPENSSH PRIVATE KEY-----\nTOPSECRET".to_string()),
            git_commit_gpgsign: Some(true),
            git_allowed_signers_file: None,
            ozone_signing_enabled: Some(true),
            ozone_signing_key: Some("-----BEGIN OPENSSH PRIVATE KEY-----\nTOPSECRET2".to_string()),
            issues: Vec::new(),
            recovery_commands: Vec::new(),
        };

        sanitize_ssh_signing_report(&mut report);
        let serialized = serde_json::to_string(&report).expect("serialize");
        assert!(!serialized.contains("SECRET_MATERIAL"));
        assert!(!serialized.contains("TOPSECRET"));
        assert_eq!(
            report.configured_key.as_deref(),
            Some("<redacted-inline-private-key>")
        );
    }

    #[test]
    fn evaluate_doctor_checks_emits_platform_remediation_across_linux_windows_and_macos() {
        let empty_repo = DoctorRepoStatus {
            in_repo: false,
            colocated: false,
            repo_root: None,
            git_dir: None,
            git_common_dir: None,
            ozone_dir: None,
            ozone_initialized: false,
            is_linked_worktree: false,
            workspace: None,
            branch: None,
            head_oid: None,
            mount_health: None,
            repo_config_present: false,
            local_config_present: false,
            active_mount_count: 0,
            unhealthy_mount_count: 0,
            active_mounts: Vec::new(),
        };
        let vfsd_missing = DoctorVfsd {
            found: false,
            path: None,
            daemon_required: true,
            daemon_running: false,
            daemon_health: "binary_missing",
            daemon_pid: None,
            daemon_version: None,
            socket_path: None,
        };
        let alias_missing = DoctorGitOzoneAlias {
            invoked_as: "oz",
            found_on_path: None,
            resolved: false,
            sibling_path: None,
        };
        let ssh_unconfigured = SshSigningReport {
            enabled_by_default: false,
            ready: false,
            key_source: None,
            configured_key: None,
            resolved_key_path: None,
            key_algorithm: None,
            key_encrypted: false,
            fingerprint: None,
            public_key: None,
            git_gpg_format: None,
            git_user_signingkey: None,
            git_commit_gpgsign: None,
            git_allowed_signers_file: None,
            ozone_signing_enabled: None,
            ozone_signing_key: None,
            issues: Vec::new(),
            recovery_commands: Vec::new(),
        };
        let gh_unauth = GitHubAuthProbe {
            remote_name: "origin".to_string(),
            remote_url: None,
            host: "github.com".to_string(),
            owner: None,
            repo: None,
            transport: None,
            api_base_url: "https://api.github.com".to_string(),
            authenticated: false,
            token_source: None,
            masked_token: None,
            hints: Vec::new(),
        };

        // 1. Linux missing FUSE3
        let linux_probe = FilesystemProbe {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            probed_path: PathBuf::from("/tmp"),
            fs_kind: FilesystemKind::Ext4,
            fs_magic: Some("0xef53".to_string()),
            reflink_supported: false,
            fuse: Some(LinuxFuseReadiness {
                dev_fuse_exists: true,
                dev_fuse_rw: false,
                fusermount3_path: None,
                fusermount3_version: None,
                user_allow_other_enabled: false,
                no_new_privs: false,
                daemon_running: false,
                ready: false,
            }),
            projfs_ready: None,
            recommended_backend: BackendKind::CasHardlink,
        };
        let (checks, warnings, recovery, hints) = evaluate_doctor_checks(
            &linux_probe,
            BackendKind::CasHardlink,
            None,
            None,
            &vfsd_missing,
            &alias_missing,
            &empty_repo,
            &ssh_unconfigured,
            &gh_unauth,
        );
        let fuse_chk = checks.iter().find(|c| c.id == "linux_fuse").unwrap();
        assert_eq!(fuse_chk.status, DoctorCheckStatus::Warn);
        assert!(fuse_chk.remediation.iter().any(|r| r.contains("fuse3")));
        assert!(fuse_chk.remediation.iter().any(|r| r.contains("usermod")));
        assert_ne!(warnings.len(), 0);
        assert_ne!(recovery.len(), 0);
        assert_ne!(hints.len(), 0);

        // 2. Windows NTFS without ProjFS / Dev Drive
        let win_probe = FilesystemProbe {
            os: "windows".to_string(),
            arch: "x86_64".to_string(),
            probed_path: PathBuf::from("C:\\repo"),
            fs_kind: FilesystemKind::Ntfs,
            fs_magic: None,
            reflink_supported: false,
            fuse: None,
            projfs_ready: Some(false),
            recommended_backend: BackendKind::CasHardlink,
        };
        let (win_diag, mac_diag) = build_platform_diagnostics(&win_probe);
        assert!(win_diag.is_some());
        assert!(mac_diag.is_none());
        let (win_checks, _, _, _) = evaluate_doctor_checks(
            &win_probe,
            BackendKind::CasHardlink,
            win_diag.as_ref(),
            None,
            &vfsd_missing,
            &alias_missing,
            &empty_repo,
            &ssh_unconfigured,
            &gh_unauth,
        );
        let win_chk = win_checks
            .iter()
            .find(|c| c.id == "windows_projfs_dev_drive")
            .unwrap();
        assert_eq!(win_chk.status, DoctorCheckStatus::Warn);
        assert!(
            win_chk
                .remediation
                .iter()
                .any(|r| r.contains("Client-ProjFS"))
        );

        // 3. macOS APFS clonefile ready
        let mac_probe = FilesystemProbe {
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            probed_path: PathBuf::from("/Users/dev/repo"),
            fs_kind: FilesystemKind::Apfs,
            fs_magic: None,
            reflink_supported: true,
            fuse: None,
            projfs_ready: None,
            recommended_backend: BackendKind::ApfsClonefile,
        };
        let (win_diag2, mac_diag2) = build_platform_diagnostics(&mac_probe);
        assert!(win_diag2.is_none());
        assert!(mac_diag2.as_ref().is_some_and(|m| m.clonefile_ready));
        let (mac_checks, _, _, _) = evaluate_doctor_checks(
            &mac_probe,
            BackendKind::ApfsClonefile,
            None,
            mac_diag2.as_ref(),
            &vfsd_missing,
            &alias_missing,
            &empty_repo,
            &ssh_unconfigured,
            &gh_unauth,
        );
        let mac_chk = mac_checks
            .iter()
            .find(|c| c.id == "macos_apfs_clonefile")
            .unwrap();
        assert_eq!(mac_chk.status, DoctorCheckStatus::Ok);
    }

    #[test]
    fn parses_short_and_long_directory_flags_and_locates_git_subcommand() {
        for argv in [
            vec!["oz", "-C", "/tmp/repo", "status"],
            vec!["oz", "--directory", "/tmp/repo", "status"],
            vec!["oz", "--directory=/tmp/repo", "status"],
            vec!["oz", "status", "--directory", "/tmp/repo"],
        ] {
            let cli = Cli::try_parse_from(&argv).expect("parse directory flag");
            assert_eq!(cli.directory.as_deref(), Some(Path::new("/tmp/repo")));
            assert!(matches!(cli.command, Some(Commands::Status(_))));
        }

        let to_os = |items: &[&str]| items.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            find_git_subcommand_index(&to_os(&["oz", "-C", "/tmp/repo", "git", "status"])),
            Some(3)
        );
        assert_eq!(
            find_git_subcommand_index(&to_os(&["oz", "--directory", "/tmp/repo", "git", "status"])),
            Some(3)
        );
        assert_eq!(
            find_git_subcommand_index(&to_os(&["oz", "--directory=/tmp/repo", "git", "status"])),
            Some(2)
        );
    }

    #[test]
    fn help_command_groups_cover_every_subcommand_without_duplicates() {
        let probe = Commands::augment_subcommands(clap::Command::new("oz"));
        let actual: HashSet<String> = probe
            .get_subcommands()
            .filter(|c| !c.is_hide_set() && c.get_name() != "help")
            .map(|c| c.get_name().to_string())
            .collect();

        let mut grouped: HashSet<String> = HashSet::new();
        for (heading, names) in HELP_COMMAND_GROUPS {
            assert!(!names.is_empty(), "group '{heading}' must not be empty");
            for &name in *names {
                assert!(
                    grouped.insert(name.to_string()),
                    "subcommand '{name}' listed more than once in HELP_COMMAND_GROUPS"
                );
                assert!(
                    actual.contains(name),
                    "subcommand '{name}' in HELP_COMMAND_GROUPS does not exist on Commands"
                );
            }
        }

        let missing: Vec<&String> = actual.difference(&grouped).collect();
        assert!(
            missing.is_empty(),
            "subcommands missing from HELP_COMMAND_GROUPS: {missing:?}"
        );
        assert!(
            !grouped_subcommands_help().contains("Other Commands:"),
            "grouped help should not contain fallback 'Other Commands:' heading"
        );
    }

    #[test]
    fn parses_oplog_and_undo_list_flags_and_enforces_conflicts() {
        let cli = Cli::try_parse_from(["oz", "oplog"]).expect("oz oplog");
        match cli.command {
            Some(Commands::Oplog(args)) => {
                assert_eq!(args.limit, oz_oplog::DEFAULT_OPLOG_LIST_LIMIT);
                assert!(!args.all);
            }
            other => panic!("expected Oplog, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["oz", "oplog", "-n", "7", "--all", "--json"])
            .expect("oz oplog -n 7 --all --json");
        assert!(cli.json);
        match cli.command {
            Some(Commands::Oplog(args)) => {
                assert_eq!(args.limit, 7);
                assert!(args.all);
            }
            other => panic!("expected Oplog, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["oz", "undo", "--list", "--limit", "5", "--all"])
            .expect("oz undo --list --limit 5 --all");
        match cli.command {
            Some(Commands::Undo(args)) => {
                assert!(args.list);
                assert_eq!(args.limit, Some(5));
                assert!(args.all);
            }
            other => panic!("expected Undo, got {other:?}"),
        }

        assert!(Cli::try_parse_from(["oz", "undo", "--list", "op_01TEST"]).is_err());
        assert!(Cli::try_parse_from(["oz", "undo", "--list", "--steps", "2"]).is_err());
        assert!(Cli::try_parse_from(["oz", "undo", "-n", "5"]).is_err());
        assert!(Cli::try_parse_from(["oz", "undo", "--all"]).is_err());
    }
}
