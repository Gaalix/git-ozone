// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! CLI handlers for Phase 5C Everyday Human Ergonomics:
//! `oz log`, `oz show`, `oz stash`, `oz cherry-pick`, `oz revert`, `oz tag`, `oz branch`,
//! and `oz completions`.

use crate::pager::emit_paged_output;
use crate::recovery::discover_and_recover_repo;
use chrono::{DateTime, Utc};
use clap::{Args, Command, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use oz_agent::{
    AiProvenanceNote, ProvenanceOptions, list_provenance_notes, read_provenance_note,
    record_commit_provenance,
};
use oz_core::{
    BranchCreateOutcome, BranchDeleteOutcome, BranchListOptions, BranchListReport,
    BranchRenameOutcome, CherryPickOptions, CherryPickOutcome, Envelope, ExitCode, GrepOptions,
    GrepReport, InteractionMode, LogCommitEntry, LogOptions, OZ_STASH_REF, OzError, OzoneRepo,
    Result, RestoreOptions, RestoreOutcome, RevParseOptions, RevParseReport, RevertOptions,
    RevertOutcome, ShowBlobOptions, ShowBlobReport, ShowOptions, ShowReport, StashDropOutcome,
    StashListReport, StashPopOutcome, StashPushOutcome, SwitchOptions, SwitchOutcome,
    TagCreateOptions, TagDeleteOutcome, TagInfo, TagListReport, TagVerifyOutcome, apply_stash_pop,
    branch_create, branch_delete, branch_list_with_options, branch_rename, cherry_pick_commit,
    compute_status, format_numbered_blob_lines, grep_repo, log_commits, parse_line_range,
    parse_rev_path_spec, prepare_stash_pop, restore_paths, rev_parse, revert_commit, show_blob,
    show_commit, stash_drop, stash_list, stash_push, switch_branch, tag_create, tag_delete,
    tag_list, tag_verify,
};
use oz_oplog::{ActorInfo, ActorKind, MutationTransaction};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

/// Color output preference for terminal rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorWhen {
    Auto,
    Always,
    Never,
}

impl ColorWhen {
    fn enabled(self, mode: InteractionMode) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => {
                !mode.json
                    && !mode.agent
                    && mode.stdout_is_tty
                    && std::env::var_os("NO_COLOR").is_none()
            }
        }
    }
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

fn print_json_envelope<T: Serialize>(env: &Envelope<T>) {
    match env.to_json_pretty() {
        Ok(s) => println!("{s}"),
        Err(err) => eprintln!("error: failed to serialize oz.v1 envelope: {err}"),
    }
}

fn render_daily_result<T: Serialize, F>(
    mode: InteractionMode,
    res: Result<Envelope<T>>,
    human_render: F,
) -> ExitCode
where
    F: FnOnce(&Envelope<T>) -> std::io::Result<()>,
{
    match res {
        Ok(env) => {
            if mode.json {
                print_json_envelope(&env);
            } else if let Err(err) = human_render(&env) {
                eprintln!("error: {err}");
                return ExitCode::UsageOrGuardrail;
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

// ============================================================================
// 1. `oz log`
// ============================================================================

#[derive(Debug, Args)]
pub struct LogArgs {
    /// Optional starting revision, branch, tag, or `A..B` range (defaults to `HEAD`).
    pub rev: Option<String>,
    /// Optional starting revision passed as a named flag (alias for positional `[REV]`).
    #[arg(long = "rev", value_name = "REV", conflicts_with = "rev")]
    pub rev_flag: Option<String>,
    /// Include commits reachable from all local branches and tags.
    #[arg(long)]
    pub all: bool,
    /// Show compact single-line format per commit.
    #[arg(long)]
    pub oneline: bool,
    /// Limit the number of commits output.
    #[arg(short = 'n', long = "max-count", alias = "limit", value_name = "N")]
    pub limit: Option<usize>,
    /// Filter commits whose author name or email contains `<PATTERN>`.
    #[arg(long, value_name = "PATTERN")]
    pub author: Option<String>,
    /// Filter commits whose message matches `<PATTERN>`.
    #[arg(long, value_name = "PATTERN")]
    pub grep: Option<String>,
    /// Pickaxe search: match commits that change occurrences of `<STRING>` in file diffs.
    #[arg(short = 'S', long = "pickaxe", value_name = "STRING")]
    pub search_string: Option<String>,
    /// Diff search: match commits whose changed lines match regex `<PATTERN>`.
    #[arg(short = 'G', long = "grep-diff", value_name = "PATTERN")]
    pub search_diff_regex: Option<String>,
    /// Show commits newer than date or duration (e.g. `2026-11-20`, `"2 days ago"`, `12h`).
    #[arg(long, value_name = "DATE")]
    pub since: Option<String>,
    /// Show compact ASCII commit graph (enabled by default).
    #[arg(long)]
    pub graph: bool,
    /// Disable ASCII commit graph prefixes.
    #[arg(long = "no-graph", conflicts_with = "graph")]
    pub no_graph: bool,
    /// When to emit ANSI color escapes (`auto`, `always`, `never`).
    #[arg(long, value_enum, default_value_t = ColorWhen::Auto)]
    pub color: ColorWhen,
    /// Optional file or directory path filters after `--` (e.g. `oz log -n 10 --oneline -- crates/oz-core/`).
    #[arg(last = true, value_name = "PATH")]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiBadgeSummary {
    pub client: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub session_id: String,
}

impl From<&AiProvenanceNote> for AiBadgeSummary {
    fn from(note: &AiProvenanceNote) -> Self {
        Self {
            client: note.client.clone(),
            model: note.model.clone(),
            session_id: note.session_id.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct LogCliCommit {
    #[serde(flatten)]
    pub entry: LogCommitEntry,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_provenance: Option<AiBadgeSummary>,
}

#[derive(Debug, Serialize)]
pub struct LogCliData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_rev: Option<String>,
    pub unborn: bool,
    pub total_returned: usize,
    pub commits: Vec<LogCliCommit>,
}

pub(crate) fn handle_log(
    args: &LogArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<LogCliData>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let opts = LogOptions {
            rev: args.rev_flag.clone().or_else(|| args.rev.clone()),
            all: args.all,
            limit: args.limit,
            author: args.author.clone(),
            grep: args.grep.clone(),
            search_string: args.search_string.clone(),
            search_diff_regex: args.search_diff_regex.clone(),
            since: args.since.clone(),
            graph: !args.no_graph,
            paths: args.paths.clone(),
        };
        let report = log_commits(&repo, &opts)?;
        let notes: BTreeMap<String, AiProvenanceNote> =
            list_provenance_notes(&repo).unwrap_or_default();

        let commits = report
            .commits
            .into_iter()
            .map(|entry| {
                let ai_provenance = notes.get(&entry.oid).map(AiBadgeSummary::from);
                LogCliCommit {
                    entry,
                    ai_provenance,
                }
            })
            .collect();

        Ok(Envelope::success(LogCliData {
            target_rev: report.target_rev,
            unborn: report.unborn,
            total_returned: report.total_returned,
            commits,
        })
        .with_workspace(repo.workspace)
        .with_branch(repo.branch)
        .with_head_oid(repo.head_oid))
    })();

    render_daily_result(mode, res, |env| {
        let use_color = args.color.enabled(mode);
        let rendered = format_log_human(&env.data, args.oneline, use_color);
        emit_paged_output(mode, &rendered)
    })
}

fn format_log_human(data: &LogCliData, oneline: bool, color: bool) -> String {
    if data.unborn {
        return "repository has no commits yet (unborn HEAD)\n".to_string();
    }
    if data.commits.is_empty() {
        return "no matching commits\n".to_string();
    }

    let mut out = String::new();
    let (c_yellow, c_green, c_cyan, c_magenta, c_dim, c_reset) = if color {
        ("\x1b[33m", "\x1b[32m", "\x1b[36m", "\x1b[35m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "", "", "", "")
    };

    for item in &data.commits {
        let c = &item.entry;
        let refs_str = if c.refs.is_empty() {
            String::new()
        } else {
            format!(" {c_green}({}){c_reset}", c.refs.join(", "))
        };
        let ai_badge = item
            .ai_provenance
            .as_ref()
            .map(|ai| format!(" {c_magenta}[AI:{}]{c_reset}", ai.client))
            .unwrap_or_default();

        if oneline {
            let _ = writeln!(
                out,
                "{}{}{c_yellow}{}{c_reset}{}{ai_badge} {}",
                c.graph_prefix, c_reset, c.short_oid, refs_str, c.subject
            );
        } else {
            let _ = writeln!(
                out,
                "{}{c_yellow}commit {}{c_reset}{}{ai_badge}",
                c.graph_prefix, c.oid, refs_str
            );
            let cont_pad = continuation_prefix(&c.graph_prefix);
            let _ = writeln!(
                out,
                "{cont_pad}Author: {} <{}>",
                c.author_name, c.author_email
            );
            let _ = writeln!(
                out,
                "{cont_pad}Date:   {}",
                c.author_time.format("%Y-%m-%d %H:%M:%S UTC")
            );
            if let Some(ref cid) = c.change_id {
                let _ = writeln!(out, "{cont_pad}{c_cyan}Change-Id: {cid}{c_reset}");
            }
            let _ = writeln!(out, "{cont_pad}");
            let _ = writeln!(out, "{cont_pad}    {}", c.subject);
            if !c.body.trim().is_empty() {
                for line in c.body.lines() {
                    if line.trim().is_empty() {
                        let _ = writeln!(out, "{cont_pad}");
                    } else {
                        let _ = writeln!(out, "{cont_pad}    {c_dim}{line}{c_reset}");
                    }
                }
            }
            let _ = writeln!(out, "{cont_pad}");
        }

        for conn in &c.graph_connectors {
            let _ = writeln!(out, "{conn}");
        }
    }
    out
}

fn continuation_prefix(graph_prefix: &str) -> String {
    if graph_prefix.is_empty() {
        return String::new();
    }
    graph_prefix
        .chars()
        .map(|ch| if ch == '*' { '|' } else { ch })
        .collect()
}

// ============================================================================
// 2. `oz show <rev>`
// ============================================================================

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Revision, commit OID, branch, tag, or `<rev>:<path>` blob spec to inspect (defaults to `HEAD`).
    pub rev: Option<String>,
    /// Display diffstat summary instead of the full unified diff (commit mode).
    #[arg(long)]
    pub stat: bool,
    /// Optional 1-based inclusive line range for `<rev>:<path>` blob reads (e.g. `300..380`, `10-40`, `50`).
    #[arg(long = "lines", value_name = "START..END")]
    pub lines: Option<String>,
    /// Prefix blob output lines with 1-based line numbers (also enabled automatically when `--lines` is set).
    #[arg(short = 'n', long = "line-numbers", alias = "numbered")]
    pub line_numbers: bool,
    /// When to emit ANSI color escapes (`auto`, `always`, `never`).
    #[arg(long, value_enum, default_value_t = ColorWhen::Auto)]
    pub color: ColorWhen,
}

#[derive(Debug, Serialize)]
pub struct ShowCliData {
    #[serde(flatten)]
    pub report: ShowReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_provenance: Option<AiProvenanceNote>,
}

pub(crate) fn handle_show(
    args: &ShowArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    if let Some(ref raw_spec) = args.rev
        && let Some((rev_part, path_part)) = parse_rev_path_spec(raw_spec)
    {
        return handle_show_blob(args, &rev_part, &path_part, directory, mode);
    }

    let res = (|| -> Result<Envelope<ShowCliData>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let report = show_commit(
            &repo,
            &ShowOptions {
                rev: args.rev.clone(),
                stat_only: args.stat,
            },
        )?;
        let ai_provenance = read_provenance_note(&repo, &report.commit_oid)
            .ok()
            .flatten();
        let head_oid = Some(report.commit_oid.clone());

        Ok(Envelope::success(ShowCliData {
            report,
            ai_provenance,
        })
        .with_workspace(repo.workspace)
        .with_branch(repo.branch)
        .with_head_oid(head_oid))
    })();

    render_daily_result(mode, res, |env| {
        let use_color = args.color.enabled(mode);
        let rendered = format_show_human(&env.data, args.stat, use_color);
        emit_paged_output(mode, &rendered)
    })
}

fn handle_show_blob(
    args: &ShowArgs,
    rev_part: &str,
    path_part: &str,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<ShowBlobReport>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let (start_line, end_line) = match args.lines.as_deref() {
            Some(raw) => parse_line_range(raw)?,
            None => (None, None),
        };
        let report = show_blob(
            &repo,
            &ShowBlobOptions {
                rev: rev_part.to_string(),
                path: path_part.to_string(),
                start_line,
                end_line,
            },
        )?;
        let head_oid = repo.head_oid.clone();
        Ok(Envelope::success(report)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(head_oid))
    })();

    render_daily_result(mode, res, |env| {
        let b = &env.data;
        if b.is_binary {
            println!(
                "Binary file {}:{} ({} bytes, blob {})",
                b.short_oid,
                b.path,
                b.size_bytes,
                &b.blob_oid[..b.blob_oid.len().min(12)]
            );
            return Ok(());
        }
        let numbered = args.line_numbers || args.lines.is_some();
        let rendered = if numbered {
            format_numbered_blob_lines(&b.content, b.start_line)
        } else {
            b.content.clone()
        };
        emit_paged_output(mode, &rendered)
    })
}

// ============================================================================
// 2b. `oz rev-parse`
// ============================================================================

#[derive(Debug, Args)]
pub struct RevParseArgs {
    /// Revisions, branches, tags, or refs to resolve to object IDs.
    pub revs: Vec<String>,
    /// Shorten object IDs to `<LEN>` hex characters (`--short` defaults to 7, or `--short=<LEN>`).
    #[arg(
        long = "short",
        value_name = "LEN",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "7"
    )]
    pub short: Option<usize>,
    /// Verify that exactly one argument resolves to a valid object ID.
    #[arg(long = "verify")]
    pub verify: bool,
    /// Output the non-ambiguous short ref name (e.g. active branch name for `HEAD`).
    #[arg(long = "abbrev-ref")]
    pub abbrev_ref: bool,
    /// Show the absolute path of the repository working-tree top-level directory.
    #[arg(long = "show-toplevel")]
    pub show_toplevel: bool,
    /// Show the `.git` directory path for the current workspace.
    #[arg(long = "git-dir")]
    pub git_dir: bool,
    /// Show the shared common `.git` directory path.
    #[arg(long = "git-common-dir")]
    pub git_common_dir: bool,
    /// Print `true` if inside a Git/Ozone working tree (exits non-zero otherwise).
    #[arg(long = "is-inside-work-tree")]
    pub is_inside_work_tree: bool,
}

pub(crate) fn handle_rev_parse(
    args: &RevParseArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<RevParseReport>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let report = rev_parse(
            &repo,
            &RevParseOptions {
                revs: args.revs.clone(),
                short: args.short.map(Some),
                verify: args.verify,
                abbrev_ref: args.abbrev_ref,
                show_toplevel: args.show_toplevel,
                git_dir: args.git_dir,
                git_common_dir: args.git_common_dir,
                is_inside_work_tree: args.is_inside_work_tree,
            },
        )?;
        let head_oid = repo.head_oid.clone();
        Ok(Envelope::success(report)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(head_oid))
    })();

    render_daily_result(mode, res, |env| {
        for v in &env.data.values {
            println!("{v}");
        }
        Ok(())
    })
}

/// Paged `oz diff` dispatch used when stdout is an interactive TTY (`!json && !agent`).
pub(crate) fn handle_diff_paged(
    args: &crate::commands::DiffArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<String>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let report = crate::commands::resolve_diff_report(&repo, args)?;
        let rendered = if args.stat {
            crate::commands::format_diff_stat(&report)
        } else if args.line_numbers {
            oz_core::render_unified_diff_numbered(&report)
        } else {
            oz_core::render_unified_diff(&report)
        };
        Ok(Envelope::success(rendered)
            .with_workspace(report.workspace)
            .with_branch(report.branch)
            .with_head_oid(report.head_oid))
    })();

    render_daily_result(mode, res, |env| {
        if env.data.is_empty() {
            return Ok(());
        }
        emit_paged_output(mode, &env.data)
    })
}

fn format_show_human(data: &ShowCliData, stat_only: bool, color: bool) -> String {
    let r = &data.report;
    let (c_yellow, c_cyan, c_magenta, c_green, c_red, c_reset) = if color {
        (
            "\x1b[33m",
            "\x1b[36m",
            "\x1b[35m",
            "\x1b[32m",
            "\x1b[31m",
            "\x1b[0m",
        )
    } else {
        ("", "", "", "", "", "")
    };

    let mut out = String::new();
    if let Some(ref tag) = r.tag_name {
        let _ = writeln!(out, "{c_cyan}tag {tag}{c_reset}");
        if let Some(ref tmsg) = r.tag_message
            && !tmsg.is_empty()
        {
            let _ = writeln!(out, "{tmsg}\n");
        }
    }
    let _ = writeln!(out, "{c_yellow}commit {}{c_reset}", r.commit_oid);
    if !r.parents.is_empty() {
        let _ = writeln!(out, "Parents: {}", r.parents.join(" "));
    }
    let _ = writeln!(out, "Author: {} <{}>", r.author_name, r.author_email);
    let _ = writeln!(
        out,
        "Date:   {}",
        r.author_time.format("%Y-%m-%d %H:%M:%S UTC")
    );
    if let Some(ref cid) = r.change_id {
        let _ = writeln!(out, "{c_cyan}Change-Id: {cid}{c_reset}");
    }
    if r.signed {
        let valid_label = match r.signature_valid {
            Some(true) => format!("{c_green}valid{c_reset}"),
            Some(false) => format!("{c_red}invalid{c_reset}"),
            None => "present".to_string(),
        };
        let fp = r.signature_fingerprint.as_deref().unwrap_or("unknown");
        let _ = writeln!(out, "Signature: SSH {valid_label} ({fp})");
    }
    if let Some(ref ai) = data.ai_provenance {
        let mut parts = vec![format!("AI:{}", ai.client)];
        if let Some(ref model) = ai.model {
            parts.push(format!("model:{model}"));
        }
        parts.push(format!("session:{}", ai.session_id));
        let _ = writeln!(
            out,
            "{c_magenta}AI-Provenance: [{}]{c_reset}",
            parts.join(" | ")
        );
        if let Some(ref prompt) = ai.prompt_summary {
            let _ = writeln!(out, "Prompt: {prompt}");
        }
    }

    let _ = writeln!(out);
    for line in r.message.lines() {
        let _ = writeln!(out, "    {line}");
    }
    let _ = writeln!(out);

    if stat_only {
        for entry in &r.stat.entries {
            if entry.is_binary {
                let _ = writeln!(out, " {} | Bin", entry.path);
            } else {
                let _ = writeln!(
                    out,
                    " {} | +{} -{}",
                    entry.path, entry.additions, entry.deletions
                );
            }
        }
        let _ = writeln!(
            out,
            " {} file(s) changed, {} insertion(s)(+), {} deletion(s)(-)",
            r.stat.files_changed, r.stat.total_additions, r.stat.total_deletions
        );
    } else if let Some(ref diff) = r.unified_diff {
        out.push_str(diff);
    }

    out
}

// ============================================================================
// 3. `oz stash` (`push` / `pop` / `list` / `drop`)
// ============================================================================

#[derive(Debug, Args)]
pub struct StashArgs {
    #[command(subcommand)]
    pub command: Option<StashCommand>,
    /// Optional stash message when invoking `oz stash -m "..."` without `push`.
    #[arg(short = 'm', long = "message", value_name = "MSG")]
    pub message: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum StashCommand {
    /// Save local dirty working-tree edits into an OpLog-backed stash snapshot.
    Push(StashPushArgs),
    /// Apply and remove a stashed snapshot (`stash@{0}` by default).
    Pop(StashTargetArgs),
    /// List recorded stash snapshots.
    List,
    /// Remove a stashed snapshot (`stash@{0}` by default) without applying it.
    Drop(StashTargetArgs),
}

#[derive(Debug, Args)]
pub struct StashPushArgs {
    /// Optional descriptive message for the stash entry.
    #[arg(short = 'm', long = "message", value_name = "MSG")]
    pub message: Option<String>,
}

#[derive(Debug, Args)]
pub struct StashTargetArgs {
    /// Target stash reference (e.g. `stash@{0}` or `0`; defaults to `stash@{0}`).
    pub stash: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StashPushCliData {
    #[serde(flatten)]
    pub outcome: StashPushOutcome,
}

#[derive(Debug, Serialize)]
pub struct StashPopCliData {
    #[serde(flatten)]
    pub outcome: StashPopOutcome,
}

#[derive(Debug, Serialize)]
pub struct StashDropCliData {
    #[serde(flatten)]
    pub outcome: StashDropOutcome,
}

pub(crate) fn handle_stash(
    args: &StashArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    match &args.command {
        None => handle_stash_push(args.message.as_deref(), directory, mode),
        Some(StashCommand::Push(push_args)) => {
            let msg = push_args.message.as_deref().or(args.message.as_deref());
            handle_stash_push(msg, directory, mode)
        }
        Some(StashCommand::Pop(target)) => {
            handle_stash_pop(target.stash.as_deref(), directory, mode)
        }
        Some(StashCommand::List) => handle_stash_list(directory, mode),
        Some(StashCommand::Drop(target)) => {
            handle_stash_drop(target.stash.as_deref(), directory, mode)
        }
    }
}

fn handle_stash_push(
    message: Option<&str>,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<StashPushCliData>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let pre_status = compute_status(&repo)?;
        if pre_status.changes.is_empty() {
            return Err(OzError::InvalidUsage {
                message: "no local changes to save".to_string(),
                recovery: vec!["Run `oz status` to check dirty files.".to_string()],
            });
        }
        let actor = actor_from_mode(mode, &repo);
        let tx = MutationTransaction::begin_with_refs(
            &repo,
            "oz stash push",
            actor,
            &[OZ_STASH_REF],
            Some(&pre_status.changes),
        )?;
        let outcome = stash_push(&repo, message)?;
        let entry = tx.finish_with_message(&repo, Some(&outcome.stash.message), Some(&[]))?;
        let op_id = entry.op_id;
        Ok(Envelope::success(StashPushCliData { outcome })
            .with_op_id(op_id.clone())
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid)
            .with_recovery_command(format!("oz undo {op_id}")))
    })();

    render_daily_result(mode, res, |env| {
        let d = &env.data.outcome;
        println!(
            "Saved {} ({} file(s)): {}",
            d.stash.ref_label, d.stash.files_count, d.stash.message
        );
        Ok(())
    })
}

fn handle_stash_pop(
    selector: Option<&str>,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<StashPopCliData>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let prepared = prepare_stash_pop(&repo, selector)?;
        let pre_status = compute_status(&repo)?;
        let actor = actor_from_mode(mode, &repo);
        let tx = MutationTransaction::begin_with_refs(
            &repo,
            "oz stash pop",
            actor,
            &[OZ_STASH_REF],
            Some(&pre_status.changes),
        )?;
        let outcome = apply_stash_pop(&repo, prepared)?;
        let entry = tx.finish_with_message(&repo, Some(&outcome.stash.message), None)?;
        let op_id = entry.op_id;
        Ok(Envelope::success(StashPopCliData { outcome })
            .with_op_id(op_id.clone())
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid)
            .with_recovery_command(format!("oz undo {op_id}")))
    })();

    render_daily_result(mode, res, |env| {
        let d = &env.data.outcome;
        println!(
            "Popped {} ({} file(s) applied): {}",
            d.stash.ref_label,
            d.applied_paths.len(),
            d.stash.message
        );
        Ok(())
    })
}

fn handle_stash_list(directory: Option<&Path>, mode: InteractionMode) -> ExitCode {
    let res = (|| -> Result<Envelope<StashListReport>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let report = stash_list(&repo)?;
        Ok(Envelope::success(report)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_daily_result(mode, res, |env| {
        if env.data.stashes.is_empty() {
            println!("no stashes");
            return Ok(());
        }
        let mut out = String::new();
        for item in &env.data.stashes {
            let _ = writeln!(
                out,
                "{}: {} ({} file(s))",
                item.ref_label, item.message, item.files_count
            );
        }
        emit_paged_output(mode, &out)
    })
}

fn handle_stash_drop(
    selector: Option<&str>,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<StashDropCliData>> {
        let repo = discover_and_recover_repo(directory, false)?;
        // Validate selector and presence before opening an OpLog transaction.
        let list = stash_list(&repo)?;
        let idx = oz_core::parse_stash_selector(selector)?;
        if idx >= list.stashes.len() {
            return Err(OzError::InvalidUsage {
                message: format!("stash@{{{idx}}} does not exist"),
                recovery: vec!["Run `oz stash list` to inspect available stashes.".to_string()],
            });
        }

        let pre_status = compute_status(&repo)?;
        let actor = actor_from_mode(mode, &repo);
        let tx = MutationTransaction::begin_with_refs(
            &repo,
            "oz stash drop",
            actor,
            &[OZ_STASH_REF],
            Some(&pre_status.changes),
        )?;
        let outcome = stash_drop(&repo, selector)?;
        let entry = tx.finish_with_message(
            &repo,
            Some(&outcome.dropped.message),
            Some(&pre_status.changes),
        )?;
        let op_id = entry.op_id;
        Ok(Envelope::success(StashDropCliData { outcome })
            .with_op_id(op_id.clone())
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid)
            .with_recovery_command(format!("oz undo {op_id}")))
    })();

    render_daily_result(mode, res, |env| {
        let d = &env.data.outcome;
        println!(
            "Dropped {} ({} remaining): {}",
            d.dropped.ref_label, d.remaining_count, d.dropped.message
        );
        Ok(())
    })
}

// ============================================================================
// 4. `oz cherry-pick <rev>` & `oz revert <rev>`
// ============================================================================

#[derive(Debug, Args)]
pub struct CherryPickArgs {
    /// Commit revision, branch, or tag to cherry-pick onto `HEAD`.
    pub rev: String,
    /// 1-based parent number when cherry-picking a merge commit.
    #[arg(short = 'm', long = "mainline", value_name = "PARENT")]
    pub mainline: Option<usize>,
    /// Apply changes to working tree without creating a commit.
    #[arg(short = 'n', long = "no-commit")]
    pub no_commit: bool,
    /// Append `(cherry picked from commit <oid>)` line to the commit message.
    #[arg(short = 'x')]
    pub append_origin: bool,
    /// Sign commit using pure-Rust OpenSSH Ed25519 `SSHSIG` v1.
    #[arg(short = 'S', long = "sign", conflicts_with = "no_sign")]
    pub sign: bool,
    /// Disable SSH commit signing.
    #[arg(long = "no-sign")]
    pub no_sign: bool,
    /// Path or inline OpenSSH Ed25519 signing key override.
    #[arg(long = "signing-key", value_name = "KEY")]
    pub signing_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct RevertArgs {
    /// Commit revision to revert.
    pub rev: String,
    /// 1-based parent number when reverting a merge commit.
    #[arg(short = 'm', long = "mainline", value_name = "PARENT")]
    pub mainline: Option<usize>,
    /// Apply inverse changes to working tree without creating a commit.
    #[arg(short = 'n', long = "no-commit")]
    pub no_commit: bool,
    /// Sign commit using pure-Rust OpenSSH Ed25519 `SSHSIG` v1.
    #[arg(short = 'S', long = "sign", conflicts_with = "no_sign")]
    pub sign: bool,
    /// Disable SSH commit signing.
    #[arg(long = "no-sign")]
    pub no_sign: bool,
    /// Path or inline OpenSSH Ed25519 signing key override.
    #[arg(long = "signing-key", value_name = "KEY")]
    pub signing_key: Option<String>,
}

pub(crate) fn handle_cherry_pick(
    args: &CherryPickArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<CherryPickOutcome>> {
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
        let cmd_label = format!("oz cherry-pick {}", args.rev);
        let tx = MutationTransaction::begin(&repo, cmd_label, actor, Some(&pre_status.changes))?;

        let outcome = cherry_pick_commit(
            &repo,
            &CherryPickOptions {
                rev: args.rev.clone(),
                mainline: args.mainline,
                no_commit: args.no_commit,
                append_origin: args.append_origin,
                sign,
                signing_key: args.signing_key.clone(),
            },
        )?;

        let entry = tx.finish_with_message(&repo, Some(&outcome.message), None)?;
        if let Some(ref cid) = outcome.commit_oid {
            record_agent_provenance_if_active(&repo, cid, mode);
        }

        let head_oid = outcome.commit_oid.clone().or_else(|| repo.head_oid.clone());
        let branch = outcome.branch.clone();
        let op_id = entry.op_id;
        Ok(Envelope::success(outcome)
            .with_op_id(op_id.clone())
            .with_workspace(repo.workspace)
            .with_branch(branch)
            .with_head_oid(head_oid)
            .with_recovery_command(format!("oz undo {op_id}")))
    })();

    render_daily_result(mode, res, |env| {
        let d = &env.data;
        if let Some(ref oid) = d.commit_oid {
            println!(
                "[{} {}] cherry-picked {} ({} file(s))",
                d.branch.as_deref().unwrap_or("detached"),
                &oid[..oid.len().min(7)],
                &d.source_commit_oid[..d.source_commit_oid.len().min(7)],
                d.applied_paths.len()
            );
        } else {
            println!(
                "applied cherry-pick of {} to working tree ({} file(s))",
                &d.source_commit_oid[..d.source_commit_oid.len().min(7)],
                d.applied_paths.len()
            );
        }
        Ok(())
    })
}

pub(crate) fn handle_revert(
    args: &RevertArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<RevertOutcome>> {
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
        let cmd_label = format!("oz revert {}", args.rev);
        let tx = MutationTransaction::begin(&repo, cmd_label, actor, Some(&pre_status.changes))?;

        let outcome = revert_commit(
            &repo,
            &RevertOptions {
                rev: args.rev.clone(),
                mainline: args.mainline,
                no_commit: args.no_commit,
                sign,
                signing_key: args.signing_key.clone(),
            },
        )?;

        let entry = tx.finish_with_message(&repo, Some(&outcome.message), None)?;
        if let Some(ref cid) = outcome.commit_oid {
            record_agent_provenance_if_active(&repo, cid, mode);
        }

        let head_oid = outcome.commit_oid.clone().or_else(|| repo.head_oid.clone());
        let branch = outcome.branch.clone();
        let op_id = entry.op_id;
        Ok(Envelope::success(outcome)
            .with_op_id(op_id.clone())
            .with_workspace(repo.workspace)
            .with_branch(branch)
            .with_head_oid(head_oid)
            .with_recovery_command(format!("oz undo {op_id}")))
    })();

    render_daily_result(mode, res, |env| {
        let d = &env.data;
        if let Some(ref oid) = d.commit_oid {
            println!(
                "[{} {}] reverted {} ({} file(s))",
                d.branch.as_deref().unwrap_or("detached"),
                &oid[..oid.len().min(7)],
                &d.reverted_commit_oid[..d.reverted_commit_oid.len().min(7)],
                d.applied_paths.len()
            );
        } else {
            println!(
                "applied revert of {} to working tree ({} file(s))",
                &d.reverted_commit_oid[..d.reverted_commit_oid.len().min(7)],
                d.applied_paths.len()
            );
        }
        Ok(())
    })
}

fn record_agent_provenance_if_active(repo: &OzoneRepo, commit_oid: &str, mode: InteractionMode) {
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
        let _ = record_commit_provenance(repo, commit_oid, &prov_opts);
    }
}

// ============================================================================
// 5. `oz tag`
// ============================================================================

#[derive(Debug, Args)]
pub struct TagArgs {
    /// Tag name (or pattern when `--list` is used).
    pub name: Option<String>,
    /// Target revision when creating a tag (defaults to `HEAD`).
    pub target: Option<String>,
    /// List tags (optionally filtered by `<name>` pattern).
    #[arg(short = 'l', long = "list")]
    pub list: bool,
    /// Delete the specified tag.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,
    /// Create an annotated tag object.
    #[arg(short = 'a', long = "annotate")]
    pub annotate: bool,
    /// Sign the tag using pure-Rust OpenSSH Ed25519 `SSHSIG` v1 (`namespace = "git"`).
    #[arg(short = 's', visible_short_alias = 'S', long = "sign")]
    pub sign: bool,
    /// Verify the SSH signature on a signed tag.
    #[arg(short = 'v', long = "verify")]
    pub verify: bool,
    /// Replace an existing tag with the same name.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
    /// Annotated/signed tag message.
    #[arg(short = 'm', long = "message", value_name = "MSG")]
    pub message: Option<String>,
    /// Path or inline OpenSSH Ed25519 signing key override.
    #[arg(long = "signing-key", value_name = "KEY")]
    pub signing_key: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum TagCliData {
    List(TagListReport),
    Created(TagInfo),
    Deleted(TagDeleteOutcome),
    Verified(TagVerifyOutcome),
}

pub(crate) fn handle_tag(
    args: &TagArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    if args.delete {
        let res = (|| -> Result<Envelope<TagDeleteOutcome>> {
            let name = args.name.as_deref().ok_or_else(|| OzError::InvalidUsage {
                message: "missing tag name for `-d` / `--delete`".to_string(),
                recovery: vec!["oz tag -d <name>".to_string()],
            })?;
            let repo = discover_and_recover_repo(directory, false)?;
            let pre_status = compute_status(&repo)?;
            let actor = actor_from_mode(mode, &repo);
            let full_ref = format!("refs/tags/{name}");
            let tx = MutationTransaction::begin_with_refs(
                &repo,
                format!("oz tag -d {name}"),
                actor,
                &[full_ref.as_str()],
                Some(&pre_status.changes),
            )?;
            let outcome = tag_delete(&repo, name)?;
            let entry = tx.finish(&repo, Some(&pre_status.changes))?;
            let op_id = entry.op_id;
            Ok(Envelope::success(outcome)
                .with_op_id(op_id.clone())
                .with_workspace(repo.workspace)
                .with_branch(repo.branch)
                .with_head_oid(repo.head_oid)
                .with_recovery_command(format!("oz undo {op_id}")))
        })();
        return render_daily_result(mode, res, |env| {
            let d = &env.data;
            println!(
                "Deleted tag '{}' (was {})",
                d.name,
                &d.deleted_oid[..d.deleted_oid.len().min(7)]
            );
            Ok(())
        });
    }

    if args.verify {
        let res = (|| -> Result<Envelope<TagVerifyOutcome>> {
            let name = args.name.as_deref().ok_or_else(|| OzError::InvalidUsage {
                message: "missing tag name for `-v` / `--verify`".to_string(),
                recovery: vec!["oz tag -v <name>".to_string()],
            })?;
            let repo = discover_and_recover_repo(directory, false)?;
            let outcome = tag_verify(&repo, name)?;
            Ok(Envelope::success(outcome)
                .with_workspace(repo.workspace)
                .with_branch(repo.branch)
                .with_head_oid(repo.head_oid))
        })();
        return render_daily_result(mode, res, |env| {
            let d = &env.data;
            println!(
                "Good \"git\" signature on tag '{}' ({})",
                d.name, d.fingerprint
            );
            Ok(())
        });
    }

    let is_list = args.list || (args.name.is_none() && !args.annotate && !args.sign);
    if is_list {
        let res = (|| -> Result<Envelope<TagListReport>> {
            let repo = discover_and_recover_repo(directory, false)?;
            let report = tag_list(&repo, args.name.as_deref())?;
            Ok(Envelope::success(report)
                .with_workspace(repo.workspace)
                .with_branch(repo.branch)
                .with_head_oid(repo.head_oid))
        })();
        return render_daily_result(mode, res, |env| {
            if env.data.tags.is_empty() {
                return Ok(());
            }
            let mut out = String::new();
            for t in &env.data.tags {
                if t.signed {
                    let _ = writeln!(out, "{} [signed]", t.name);
                } else {
                    let _ = writeln!(out, "{}", t.name);
                }
            }
            emit_paged_output(mode, &out)
        });
    }

    let res = (|| -> Result<Envelope<TagInfo>> {
        let name = args.name.as_deref().ok_or_else(|| OzError::InvalidUsage {
            message: "missing tag name".to_string(),
            recovery: vec!["oz tag <name> [<rev>]".to_string()],
        })?;
        let repo = discover_and_recover_repo(directory, false)?;
        let pre_status = compute_status(&repo)?;
        let actor = actor_from_mode(mode, &repo);
        let full_ref = format!("refs/tags/{name}");
        let tx = MutationTransaction::begin_with_refs(
            &repo,
            format!("oz tag {name}"),
            actor,
            &[full_ref.as_str()],
            Some(&pre_status.changes),
        )?;
        let info = tag_create(
            &repo,
            &TagCreateOptions {
                name: name.to_string(),
                target_rev: args.target.clone(),
                message: args.message.clone(),
                annotate: args.annotate,
                sign: args.sign,
                force: args.force,
                signing_key: args.signing_key.clone(),
            },
        )?;
        let entry = tx.finish(&repo, Some(&pre_status.changes))?;
        let op_id = entry.op_id;
        Ok(Envelope::success(info)
            .with_op_id(op_id.clone())
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid)
            .with_recovery_command(format!("oz undo {op_id}")))
    })();

    render_daily_result(mode, res, |env| {
        let d = &env.data;
        if d.signed {
            println!(
                "Created signed tag '{}' -> {} ({})",
                d.name,
                &d.target_oid[..d.target_oid.len().min(7)],
                d.signature_fingerprint.as_deref().unwrap_or("ssh")
            );
        } else {
            println!(
                "Created {} tag '{}' -> {}",
                d.kind,
                d.name,
                &d.target_oid[..d.target_oid.len().min(7)]
            );
        }
        Ok(())
    })
}

// ============================================================================
// 6. `oz branch`
// ============================================================================

#[derive(Debug, Args)]
pub struct BranchArgs {
    /// Branch names / pattern depending on operation mode.
    pub args: Vec<String>,
    /// List local branches (default when no positional args are passed).
    #[arg(short = 'l', long = "list")]
    pub list: bool,
    /// List both local and remote-tracking branches (`-a` / `--all`).
    #[arg(short = 'a', long = "all", conflicts_with = "remotes")]
    pub all: bool,
    /// List only remote-tracking branches (`-r` / `--remotes`).
    #[arg(short = 'r', long = "remotes", conflicts_with = "all")]
    pub remotes: bool,
    /// Delete a merged branch.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,
    /// Force-delete a branch regardless of merge status.
    #[arg(short = 'D')]
    pub delete_force: bool,
    /// Rename a branch (`-m [<old>] <new>`).
    #[arg(short = 'm', long = "move", visible_alias = "rename")]
    pub rename: bool,
    /// Force-rename a branch (`-M [<old>] <new>`).
    #[arg(short = 'M')]
    pub rename_force: bool,
    /// Force branch creation when a branch of the same name exists.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
}

pub(crate) fn handle_branch(
    args: &BranchArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    if args.delete || args.delete_force {
        let res = (|| -> Result<Envelope<BranchDeleteOutcome>> {
            if args.args.len() != 1 {
                return Err(OzError::InvalidUsage {
                    message: "expected exactly 1 branch name for `-d` / `-D`".to_string(),
                    recovery: vec!["oz branch -d <branch>".to_string()],
                });
            }
            let name = &args.args[0];
            let repo = discover_and_recover_repo(directory, false)?;
            let pre_status = compute_status(&repo)?;
            let actor = actor_from_mode(mode, &repo);
            let full_ref = format!("refs/heads/{name}");
            let tx = MutationTransaction::begin_with_refs(
                &repo,
                format!("oz branch -d {name}"),
                actor,
                &[full_ref.as_str()],
                Some(&pre_status.changes),
            )?;
            let outcome = branch_delete(&repo, name, args.delete_force)?;
            let entry = tx.finish(&repo, Some(&pre_status.changes))?;
            let op_id = entry.op_id;
            Ok(Envelope::success(outcome)
                .with_op_id(op_id.clone())
                .with_workspace(repo.workspace)
                .with_branch(repo.branch)
                .with_head_oid(repo.head_oid)
                .with_recovery_command(format!("oz undo {op_id}")))
        })();
        return render_daily_result(mode, res, |env| {
            let d = &env.data;
            println!(
                "Deleted branch {} (was {})",
                d.name,
                &d.deleted_oid[..d.deleted_oid.len().min(7)]
            );
            Ok(())
        });
    }

    if args.rename || args.rename_force {
        let res = (|| -> Result<Envelope<BranchRenameOutcome>> {
            let (old_opt, new_name) = match args.args.as_slice() {
                [new_only] => (None, new_only.as_str()),
                [old, new_b] => (Some(old.as_str()), new_b.as_str()),
                _ => {
                    return Err(OzError::InvalidUsage {
                        message: "expected `[<old_branch>] <new_branch>` for `-m` / `-M`"
                            .to_string(),
                        recovery: vec!["oz branch -m <old> <new>".to_string()],
                    });
                }
            };
            let repo = discover_and_recover_repo(directory, false)?;
            let old_resolved = old_opt
                .map(str::to_string)
                .or_else(|| repo.branch.clone())
                .unwrap_or_default();
            let pre_status = compute_status(&repo)?;
            let actor = actor_from_mode(mode, &repo);
            let old_ref = format!("refs/heads/{old_resolved}");
            let new_ref = format!("refs/heads/{new_name}");
            let mut watched = vec![new_ref.as_str()];
            if !old_resolved.is_empty() {
                watched.push(old_ref.as_str());
            }
            let tx = MutationTransaction::begin_with_refs(
                &repo,
                format!("oz branch -m {old_resolved} {new_name}"),
                actor,
                &watched,
                Some(&pre_status.changes),
            )?;
            let outcome = branch_rename(&repo, old_opt, new_name, args.rename_force || args.force)?;
            let post_repo = OzoneRepo::discover(directory).unwrap_or_else(|_| repo.clone());
            let entry = tx.finish(&post_repo, Some(&pre_status.changes))?;
            let op_id = entry.op_id;
            let new_branch = if outcome.updated_current_head {
                Some(outcome.new_name.clone())
            } else {
                post_repo.branch
            };
            Ok(Envelope::success(outcome)
                .with_op_id(op_id.clone())
                .with_workspace(post_repo.workspace)
                .with_branch(new_branch)
                .with_head_oid(post_repo.head_oid)
                .with_recovery_command(format!("oz undo {op_id}")))
        })();
        return render_daily_result(mode, res, |env| {
            let d = &env.data;
            println!("Renamed branch '{}' -> '{}'", d.old_name, d.new_name);
            Ok(())
        });
    }

    if args.list || args.all || args.remotes || args.args.is_empty() {
        let res = (|| -> Result<Envelope<BranchListReport>> {
            let repo = discover_and_recover_repo(directory, false)?;
            let pattern = args.args.first().cloned();
            let report = branch_list_with_options(
                &repo,
                &BranchListOptions {
                    pattern,
                    include_remotes: args.all,
                    remotes_only: args.remotes,
                },
            )?;
            Ok(Envelope::success(report)
                .with_workspace(repo.workspace)
                .with_branch(repo.branch)
                .with_head_oid(repo.head_oid))
        })();
        return render_daily_result(mode, res, |env| {
            let mut out = String::new();
            for b in &env.data.branches {
                let marker = if b.is_current { "* " } else { "  " };
                let remote_prefix = if b.is_remote { "remotes/" } else { "" };
                let ws_suffix = b
                    .workspace
                    .as_ref()
                    .map(|w| format!(" [ws:{w}]"))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "{marker}{remote_prefix}{} ({}){} {}",
                    b.name, b.short_oid, ws_suffix, b.subject
                );
            }
            emit_paged_output(mode, &out)
        });
    }

    let res = (|| -> Result<Envelope<BranchCreateOutcome>> {
        if args.args.len() > 2 {
            return Err(OzError::InvalidUsage {
                message: "expected `oz branch <name> [<start_point>]`".to_string(),
                recovery: vec!["oz branch feat/new".to_string()],
            });
        }
        let name = &args.args[0];
        let start = args.args.get(1).map(String::as_str);
        let repo = discover_and_recover_repo(directory, false)?;
        let pre_status = compute_status(&repo)?;
        let actor = actor_from_mode(mode, &repo);
        let full_ref = format!("refs/heads/{name}");
        let tx = MutationTransaction::begin_with_refs(
            &repo,
            format!("oz branch {name}"),
            actor,
            &[full_ref.as_str()],
            Some(&pre_status.changes),
        )?;
        let outcome = branch_create(&repo, name, start, args.force)?;
        let entry = tx.finish(&repo, Some(&pre_status.changes))?;
        let op_id = entry.op_id;
        Ok(Envelope::success(outcome)
            .with_op_id(op_id.clone())
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid)
            .with_recovery_command(format!("oz undo {op_id}")))
    })();

    render_daily_result(mode, res, |env| {
        let d = &env.data;
        println!(
            "Created branch '{}' at {}",
            d.name,
            &d.head_oid[..d.head_oid.len().min(7)]
        );
        Ok(())
    })
}

// ============================================================================
// 7. `oz completions <shell>`
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    #[value(name = "powershell", alias = "pwsh")]
    PowerShell,
}

impl From<CompletionShell> for Shell {
    fn from(s: CompletionShell) -> Self {
        match s {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Zsh => Self::Zsh,
            CompletionShell::Fish => Self::Fish,
            CompletionShell::PowerShell => Self::PowerShell,
        }
    }
}

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// Target shell (`bash`, `zsh`, `fish`, `powershell`).
    #[arg(value_enum)]
    pub shell: CompletionShell,
}

#[derive(Debug, Serialize)]
pub struct CompletionsData {
    pub shell: CompletionShell,
    pub binary: &'static str,
    pub script: String,
}

pub(crate) fn handle_completions(
    args: &CompletionsArgs,
    mut cmd: Command,
    mode: InteractionMode,
) -> ExitCode {
    let mut buf = Vec::new();
    let shell: Shell = args.shell.into();
    generate(shell, &mut cmd, "oz", &mut buf);
    let script = String::from_utf8_lossy(&buf).into_owned();

    if mode.json {
        let env = Envelope::success(CompletionsData {
            shell: args.shell,
            binary: "oz",
            script,
        });
        print_json_envelope(&env);
    } else {
        print!("{script}");
    }
    ExitCode::Success
}

// ============================================================================
// 8. `oz restore` (selective working-tree restore)
// ============================================================================

#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// Revision to restore file contents from (defaults to HEAD).
    #[arg(short = 's', long = "source", value_name = "REV")]
    pub source: Option<String>,
    /// Delete specified untracked paths that do not exist in the source revision.
    #[arg(long = "remove-untracked")]
    pub remove_untracked: bool,
    /// File paths, directory prefixes, or file IDs (`f1`, `f2`) to restore.
    #[arg(required = true)]
    pub targets: Vec<String>,
}

pub(crate) fn handle_restore(
    args: &RestoreArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<RestoreOutcome>> {
        let repo = discover_and_recover_repo(directory, true)?;
        let pre_status = compute_status(&repo)?;
        let actor = actor_from_mode(mode, &repo);
        let cmd_label = format!("oz restore {}", args.targets.join(" "));
        let tx = MutationTransaction::begin(&repo, cmd_label, actor, Some(&pre_status.changes))?;
        let outcome = restore_paths(
            &repo,
            &RestoreOptions {
                targets: args.targets.clone(),
                source: args.source.clone(),
                remove_untracked: args.remove_untracked,
            },
        )?;
        let post_status = compute_status(&repo)?;
        let entry = tx.finish(&repo, Some(&post_status.changes))?;
        let op_id = entry.op_id;
        Ok(Envelope::success(outcome)
            .with_op_id(op_id.clone())
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid)
            .with_recovery_command(format!("oz undo {op_id}")))
    })();

    render_daily_result(mode, res, |env| {
        let d = &env.data;
        let short_src = &d.source_oid[..d.source_oid.len().min(7)];
        if !d.restored_paths.is_empty() {
            println!("Restored {} file(s) from {short_src}", d.restored_paths.len());
        }
        if !d.removed_paths.is_empty() {
            println!("Removed {} untracked path(s)", d.removed_paths.len());
        }
        Ok(())
    })
}

// ============================================================================
// 9. `oz switch` (native branch switch with OpLog undo)
// ============================================================================

#[derive(Debug, Args)]
pub struct SwitchArgs {
    /// Target branch, tag, or commit revision to switch to.
    pub target: Option<String>,
    /// Create a new branch starting at `<TARGET>` (or HEAD) and switch to it.
    #[arg(short = 'c', long = "create", value_name = "BRANCH", conflicts_with = "force_create")]
    pub create: Option<String>,
    /// Create or reset `<BRANCH>` starting at `<TARGET>` (or HEAD) and switch to it.
    #[arg(long = "force-create", value_name = "BRANCH", conflicts_with = "create")]
    pub force_create: Option<String>,
    /// Detach `HEAD` at the resolved commit revision.
    #[arg(short = 'd', long = "detach")]
    pub detach: bool,
    /// Overwrite conflicting dirty working-tree files.
    #[arg(short = 'f', long = "force", alias = "discard-changes")]
    pub force: bool,
}

pub(crate) fn handle_switch(
    args: &SwitchArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<SwitchOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let pre_status = compute_status(&repo)?;
        let actor = actor_from_mode(mode, &repo);
        let (create_branch, reset_existing_branch) = if let Some(ref c) = args.create {
            (Some(c.clone()), false)
        } else if let Some(ref fc) = args.force_create {
            (Some(fc.clone()), true)
        } else {
            (None, false)
        };

        let tx = MutationTransaction::begin_watching_all_branches(
            &repo,
            "oz switch".to_string(),
            actor,
            Some(&pre_status.changes),
        )?;

        let outcome = switch_branch(
            &repo,
            &SwitchOptions {
                target: args.target.clone(),
                create_branch,
                reset_existing_branch,
                detach: args.detach,
                allow_detached_commit: args.detach,
                force: args.force,
            },
        )?;

        let post_repo = OzoneRepo::discover(directory).unwrap_or(repo);
        let entry = tx.finish(&post_repo, None)?;
        let op_id = entry.op_id;
        Ok(Envelope::success(outcome.clone())
            .with_op_id(op_id.clone())
            .with_workspace(post_repo.workspace)
            .with_branch(outcome.branch)
            .with_head_oid(outcome.head_oid)
            .with_recovery_command(format!("oz undo {op_id}")))
    })();

    render_daily_result(mode, res, |env| {
        let d = &env.data;
        if let Some(ref br) = d.branch {
            if d.created_branch {
                println!("Switched to a new branch '{br}'");
            } else {
                println!("Switched to branch '{br}'");
            }
        } else if let Some(ref oid) = d.head_oid {
            println!("HEAD is now at {}", &oid[..oid.len().min(7)]);
        }
        Ok(())
    })
}

// ============================================================================
// 10. `oz grep` (working-tree & rev-scoped tree grep)
// ============================================================================

#[derive(Debug, Args)]
pub struct GrepArgs {
    /// Regex (or literal with `-F`) pattern to search for.
    pub pattern: String,
    /// Optional revision, branch, or remote-tracking ref (e.g. `origin/main`) to search without checking out.
    pub rev: Option<String>,
    /// Case-insensitive matching (`-i` / `--ignore-case`).
    #[arg(short = 'i', long = "ignore-case")]
    pub ignore_case: bool,
    /// Match pattern as a literal substring instead of a regular expression (`-F` / `--fixed-strings`).
    #[arg(short = 'F', long = "fixed-strings")]
    pub fixed_strings: bool,
    /// Only output matching file paths (`-l` / `--files-with-matches`).
    #[arg(short = 'l', long = "files-with-matches")]
    pub files_with_matches: bool,
    /// Cap total returned line matches.
    #[arg(long = "max-matches", value_name = "N")]
    pub max_matches: Option<usize>,
    /// Optional path prefixes, file paths, or globs after `--` (e.g. `oz grep foo origin/main -- tvva-ui/`).
    #[arg(last = true, value_name = "PATH")]
    pub paths: Vec<String>,
}

pub(crate) fn handle_grep(
    args: &GrepArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<GrepReport>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let report = grep_repo(
            &repo,
            &GrepOptions {
                pattern: args.pattern.clone(),
                rev: args.rev.clone(),
                paths: args.paths.clone(),
                ignore_case: args.ignore_case,
                fixed_strings: args.fixed_strings,
                files_with_matches: args.files_with_matches,
                max_matches: args.max_matches,
            },
        )?;
        let head_oid = report.commit_oid.clone().or(repo.head_oid);
        Ok(Envelope::success(report)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(head_oid))
    })();

    render_daily_result(mode, res, |env| {
        let d = &env.data;
        let mut out = String::new();
        let prefix = d
            .rev
            .as_deref()
            .map(|r| format!("{r}:"))
            .unwrap_or_default();
        if args.files_with_matches {
            for f in &d.matched_files {
                let _ = writeln!(out, "{prefix}{f}");
            }
        } else {
            for m in &d.matches {
                let _ = writeln!(out, "{prefix}{}:{}:{}", m.path, m.line_number, m.line);
            }
        }
        if !out.is_empty() {
            emit_paged_output(mode, &out)?;
        }
        Ok(())
    })
}

#[allow(dead_code)]
fn _unused_chrono_keep(_: DateTime<Utc>) {}
