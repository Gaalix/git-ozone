// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Drop-in `oz git <args...>` compatibility shim.
//!
//! Natively intercepts high-impact Git commands (`worktree`, `status`, `diff`, `add`,
//! `reset` unpinning, `commit`, `switch`, `checkout`, `clone`, `fetch`, `push`, and
//! `merge`/`rebase` seams) on Ozone's CoW + `gix` + MVCC OpLog engine. Remaining
//! read-only commands delegate directly to system `git`, while mutating legacy commands
//! (`reset --hard`, `clean`, `rm`, `mv`, `restore`, `revert`, `cherry-pick`, `stash`, etc.)
//! are wrapped in [`oz_oplog::MutationTransaction`] so `oz undo` reverses them safely.

use crate::commands::{
    attach_out_of_scope_warnings, print_abort_human, print_integrate_human, print_json_envelope,
    print_stack_rebase_human, wrap_integrate_envelope, wrap_stack_rebase_envelope,
};
use crate::recovery::discover_and_recover_repo;
use clap::Args;
use oz_agent::{
    AiProvenanceNote, ProvenanceOptions, detect_agent_session, record_commit_provenance,
};
use oz_core::{
    CheckoutPathsOptions, CommitOptions, DiffReport, Envelope, ExitCode, FileChange, GitConfig,
    InteractionMode, OzError, OzoneRepo, PathspecMatcher, Result, SwitchOptions,
    add_pinned_paths, checkout_paths, clear_pinned_paths, commit_changes,
    commit_changes_ignoring_pinned, compute_diff_with_overrides, compute_status,
    compute_status_with_overrides, create_or_reset_local_branch, is_local_branch,
    list_git_worktree_entries, list_tracked_paths_in_head, lookup_tree_entries_meta,
    normalize_lexical, read_pinned_paths, remove_pinned_paths, render_unified_diff,
    resolves_as_revision, switch_branch, validate_workspace_name,
};
use oz_cow::{
    GcWorkspacesOptions, RemoveWorkspaceOptions, SpawnWorkspaceOptions, gc_workspaces,
    list_workspaces, remove_workspace, spawn_workspace,
};
use oz_github::{
    CloneOptions, FetchOptions, FetchOutcome, ForceLeaseSpec, HttpAuthScheme, PushOptions,
    PushOutcome, RemoteScheme, clone_repo, fetch_remote, parse_git_remote,
    push_refs_with_options,
};
use oz_integrate::{
    IntegrateOptions, IntegrationMode, StackRebaseOptions, StackRebaseOutcome, abort_integration,
    active_session, continue_integration_with_verify, continue_stack_rebase, integrate,
    stack_rebase,
};
use oz_oplog::{ActorInfo, ActorKind, MutationTransaction};
use serde::Serialize;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Arguments accepted by `oz git <args...>`.
#[derive(Debug, Clone, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct GitShimArgs {
    /// Git subcommand and flags (`oz git <args...>`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "ARGS")]
    pub args: Vec<OsString>,
}

#[derive(Debug, Serialize)]
struct DiffData {
    #[serde(flatten)]
    report: DiffReport,
    unified_diff: String,
}

#[derive(Debug, Serialize)]
struct GitAddData {
    pinned_paths: Vec<String>,
    total_pinned: usize,
    dry_run: bool,
}

#[derive(Debug, Serialize)]
struct GitResetPinnedData {
    unpinned_paths: Vec<String>,
    remaining_pinned: usize,
}

#[derive(Debug, Serialize)]
struct GitCloneData {
    url: String,
    path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    ozone_dir: Option<PathBuf>,
    branch: Option<String>,
    head_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol_version: Option<u8>,
    #[serde(default)]
    partial_clone: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    filter: Option<String>,
    #[serde(default)]
    filter_negotiated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    checked_out_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    op_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct GitCommitData {
    #[serde(flatten)]
    outcome: oz_core::CommitOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance: Option<AiProvenanceNote>,
}

#[derive(Debug, Serialize)]
struct GitWorktreeItem {
    name: String,
    path: PathBuf,
    branch: Option<String>,
    head_oid: Option<String>,
    is_main: bool,
    healthy: bool,
}

#[derive(Debug, Serialize)]
struct GitDelegatedOutcome {
    command: String,
    exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    op_id: Option<String>,
}

/// Parsed leading Git global options preceding the Git subcommand.
#[derive(Debug, Clone)]
pub struct ParsedGitGlobals {
    pub effective_dir: Option<PathBuf>,
    pub delegated_globals: Vec<OsString>,
    pub config_overrides: Vec<(String, String)>,
    pub no_pager: bool,
    pub mode: InteractionMode,
    pub subcommand: Option<String>,
    pub subcommand_args: Vec<OsString>,
}

/// Dispatches `oz git <args...>`.
pub fn handle_git(
    args: &GitShimArgs,
    initial_directory: Option<&Path>,
    base_mode: InteractionMode,
) -> ExitCode {
    let parsed = parse_leading_git_globals(&args.args, initial_directory, base_mode);
    let mode = parsed.mode;

    if let Some(ref dir) = parsed.effective_dir
        && parsed.subcommand.as_deref() != Some("clone")
        && !dir.exists()
    {
        let err = OzError::InvalidUsage {
            message: format!("cannot change to '{}': no such directory", dir.display()),
            recovery: Vec::new(),
        };
        return emit_error(mode, &err);
    }

    let Some(ref subcmd) = parsed.subcommand else {
        if parsed
            .delegated_globals
            .iter()
            .any(|a| a == "--version" || a == "-v")
        {
            return delegate_readonly(&parsed, &[], &[]);
        }
        print_git_shim_help(mode);
        return ExitCode::Success;
    };

    match subcmd.as_str() {
        "worktree" => handle_git_worktree(&parsed),
        "status" => handle_git_status(&parsed),
        "diff" => handle_git_diff(&parsed),
        "add" => handle_git_add(&parsed),
        "reset" => handle_git_reset(&parsed),
        "commit" => handle_git_commit(&parsed),
        "switch" => handle_git_switch(&parsed, false),
        "checkout" => handle_git_checkout(&parsed),
        "merge" => handle_git_merge(&parsed),
        "rebase" => handle_git_rebase(&parsed),
        "clone" => handle_git_clone(&parsed),
        "fetch" => handle_git_fetch(&parsed),
        "push" => handle_git_push(&parsed),
        _ => handle_other_git_command(&parsed, subcmd),
    }
}

fn parse_leading_git_globals(
    raw_args: &[OsString],
    initial_directory: Option<&Path>,
    base_mode: InteractionMode,
) -> ParsedGitGlobals {
    let mut effective_dir = initial_directory.map(Path::to_path_buf);
    let mut delegated_globals = Vec::new();
    let mut config_overrides = Vec::new();
    let mut no_pager = false;
    let mut mode = base_mode;
    let mut subcommand = None;
    let mut subcommand_args = Vec::new();

    let mut i = 0usize;
    while i < raw_args.len() {
        let arg = &raw_args[i];
        let s = arg.to_string_lossy();

        if s == "--" {
            if i + 1 < raw_args.len() {
                subcommand = Some(raw_args[i + 1].to_string_lossy().into_owned());
                subcommand_args = raw_args[i + 2..].to_vec();
            }
            break;
        }

        if s == "-C" {
            if let Some(dir_os) = raw_args.get(i + 1) {
                apply_cd(&mut effective_dir, Path::new(dir_os));
                i += 2;
                continue;
            }
            delegated_globals.push(arg.clone());
            i += 1;
            continue;
        }
        if let Some(rest) = s.strip_prefix("-C")
            && !rest.is_empty()
        {
            apply_cd(&mut effective_dir, Path::new(rest));
            i += 1;
            continue;
        }

        if s == "-c" {
            if let Some(kv_os) = raw_args.get(i + 1) {
                let kv = kv_os.to_string_lossy();
                if let Ok((k, v)) = GitConfig::parse_cli_override(kv.trim()) {
                    config_overrides.push((k, v));
                }
                delegated_globals.push(arg.clone());
                delegated_globals.push(kv_os.clone());
                i += 2;
                continue;
            }
            delegated_globals.push(arg.clone());
            i += 1;
            continue;
        }
        if let Some(rest) = s.strip_prefix("-c")
            && !rest.is_empty()
        {
            if let Ok((k, v)) = GitConfig::parse_cli_override(rest.trim()) {
                config_overrides.push((k, v));
            }
            delegated_globals.push(arg.clone());
            i += 1;
            continue;
        }

        if s == "--no-pager" {
            no_pager = true;
            delegated_globals.push(arg.clone());
            i += 1;
            continue;
        }

        if s == "--json" {
            mode.json = true;
            mode.non_interactive = true;
            i += 1;
            continue;
        }
        if s == "--agent" {
            mode.agent = true;
            mode.non_interactive = true;
            i += 1;
            continue;
        }

        if matches!(
            s.as_ref(),
            "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix" | "--config-env"
        ) {
            delegated_globals.push(arg.clone());
            if let Some(val) = raw_args.get(i + 1) {
                if s == "--work-tree" && effective_dir.is_none() {
                    apply_cd(&mut effective_dir, Path::new(val));
                }
                delegated_globals.push(val.clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        if s.starts_with("--git-dir=")
            || s.starts_with("--work-tree=")
            || s.starts_with("--namespace=")
            || s.starts_with("--exec-path")
            || s.starts_with("--config-env=")
            || matches!(
                s.as_ref(),
                "-p" | "--paginate"
                    | "--bare"
                    | "--literal-pathspecs"
                    | "--glob-pathspecs"
                    | "--noglob-pathspecs"
                    | "--icase-pathspecs"
                    | "--no-optional-locks"
                    | "--no-replace-objects"
                    | "--html-path"
                    | "--man-path"
                    | "--info-path"
                    | "--version"
                    | "-v"
                    | "--help"
                    | "-h"
            )
        {
            if let Some(wt) = s.strip_prefix("--work-tree=")
                && effective_dir.is_none()
            {
                apply_cd(&mut effective_dir, Path::new(wt));
            }
            delegated_globals.push(arg.clone());
            i += 1;
            continue;
        }

        if s.starts_with('-') {
            delegated_globals.push(arg.clone());
            i += 1;
            continue;
        }

        subcommand = Some(s.into_owned());
        subcommand_args = raw_args[i + 1..].to_vec();
        break;
    }

    ParsedGitGlobals {
        effective_dir,
        delegated_globals,
        config_overrides,
        no_pager,
        mode,
        subcommand,
        subcommand_args,
    }
}

fn apply_cd(effective_dir: &mut Option<PathBuf>, next: &Path) {
    if next.is_absolute() {
        *effective_dir = Some(next.to_path_buf());
    } else if let Some(base) = effective_dir {
        *effective_dir = Some(base.join(next));
    } else {
        *effective_dir = Some(next.to_path_buf());
    }
}

fn invocation_dir(parsed: &ParsedGitGlobals) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match parsed.effective_dir {
        Some(ref p) if p.is_absolute() => normalize_lexical(p),
        Some(ref rel) => normalize_lexical(&cwd.join(rel)),
        None => normalize_lexical(&cwd),
    }
}

fn actor_from_mode(mode: InteractionMode, repo: &OzoneRepo) -> ActorInfo {
    let detected = detect_agent_session();
    let is_agent = mode.agent || detected.is_some();
    let client = detected
        .map(|s| s.client)
        .unwrap_or_else(|| "oz-git".to_string());
    ActorInfo {
        kind: if is_agent {
            ActorKind::Agent
        } else {
            ActorKind::Human
        },
        client: Some(client),
        workspace: repo.workspace.clone(),
    }
}

fn standalone_actor_from_mode(mode: InteractionMode) -> ActorInfo {
    let detected = detect_agent_session();
    let is_agent = mode.agent || detected.is_some();
    let client = detected
        .map(|s| s.client)
        .unwrap_or_else(|| "oz-git".to_string());
    ActorInfo {
        kind: if is_agent {
            ActorKind::Agent
        } else {
            ActorKind::Human
        },
        client: Some(client),
        workspace: None,
    }
}

fn print_git_shim_help(mode: InteractionMode) {
    let text = "oz git <subcommand> [args...]\n\
        Intercepted natively on Ozone: worktree, status, diff, add, reset (paths), commit, switch, checkout, clone, fetch, push, merge, rebase\n\
        Remaining commands delegate to real `git`, with mutating commands wrapped in MVCC OpLog snapshots (`oz undo`).";
    if mode.json {
        let env = Envelope::success(serde_json::json!({
            "usage": "oz git <subcommand> [args...]",
            "intercepted": [
                "worktree", "status", "diff", "add", "reset", "commit",
                "switch", "checkout", "merge", "rebase", "clone", "fetch", "push"
            ]
        }));
        print_json_envelope(&env);
    } else {
        println!("{text}");
    }
}

fn emit_error(mode: InteractionMode, err: &OzError) -> ExitCode {
    if mode.json {
        let env = Envelope::from_error(err);
        print_json_envelope(&env);
    } else {
        eprintln!("error: {err}");
        for cmd in err.recovery_commands() {
            eprintln!("hint: {cmd}");
        }
    }
    err.exit_code()
}

fn emit_json_or_human<T: Serialize, F>(
    mode: InteractionMode,
    res: Result<Envelope<T>>,
    human_fn: F,
) -> ExitCode
where
    F: FnOnce(&Envelope<T>),
{
    match res {
        Ok(env) => {
            if mode.json {
                print_json_envelope(&env);
            } else {
                human_fn(&env);
            }
            ExitCode::Success
        }
        Err(err) => emit_error(mode, &err),
    }
}

fn extract_json_agent_flags(args: &[OsString], mut mode: InteractionMode) -> (Vec<String>, InteractionMode) {
    let mut out = Vec::with_capacity(args.len());
    let mut after_sep = false;
    for raw in args {
        let s = raw.to_string_lossy();
        if s == "--" {
            after_sep = true;
            out.push(s.into_owned());
            continue;
        }
        if !after_sep {
            if s == "--json" {
                mode.json = true;
                mode.non_interactive = true;
                continue;
            }
            if s == "--agent" {
                mode.agent = true;
                mode.non_interactive = true;
                continue;
            }
        }
        out.push(s.into_owned());
    }
    (out, mode)
}

fn resolve_pathspec_to_repo_rel(repo: &OzoneRepo, inv_dir: &Path, raw: &str) -> String {
    if let Some(root_rel) = raw.strip_prefix(":/") {
        return normalize_rel_str(root_rel);
    }
    let workdir_norm = normalize_lexical(&repo.workdir_path);
    let inv_norm = normalize_lexical(inv_dir);

    let raw_path = Path::new(raw);
    if raw_path.is_absolute() {
        let abs_norm = normalize_lexical(raw_path);
        if let Ok(rel) = abs_norm.strip_prefix(&workdir_norm) {
            return path_to_slash(rel);
        }
        if let (Ok(abs_can), Ok(wd_can)) = (
            std::fs::canonicalize(&abs_norm),
            std::fs::canonicalize(&workdir_norm),
        ) && let Ok(rel) = abs_can.strip_prefix(&wd_can)
        {
            return path_to_slash(rel);
        }
        return normalize_rel_str(raw);
    }

    if let Ok(prefix) = inv_norm.strip_prefix(&workdir_norm) {
        if prefix.as_os_str().is_empty() {
            return normalize_rel_str(raw);
        }
        let joined = normalize_lexical(&inv_norm.join(raw));
        if let Ok(rel) = joined.strip_prefix(&workdir_norm) {
            let s = path_to_slash(rel);
            return if s.is_empty() { ".".to_string() } else { s };
        }
    } else if let (Ok(inv_can), Ok(wd_can)) = (
        std::fs::canonicalize(&inv_norm),
        std::fs::canonicalize(&workdir_norm),
    ) && let Ok(rel) = normalize_lexical(&inv_can.join(raw)).strip_prefix(&wd_can)
    {
        let s = path_to_slash(rel);
        return if s.is_empty() { ".".to_string() } else { s };
    }

    normalize_rel_str(raw)
}

fn normalize_rel_str(raw: &str) -> String {
    let replaced = raw.replace('\\', "/");
    let stripped = replaced.strip_prefix("./").unwrap_or(&replaced);
    let trimmed = stripped.trim_end_matches('/');
    if trimmed.is_empty() {
        ".".to_string()
    } else {
        trimmed.to_string()
    }
}

fn path_to_slash(path: &Path) -> String {
    let mut out = String::new();
    for comp in path.components() {
        if let std::path::Component::Normal(os) = comp {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(&os.to_string_lossy());
        }
    }
    out
}

// ============================================================================
// Real System Git Resolution & Execution
// ============================================================================

/// Resolves the real system `git` binary, checking `OZ_REAL_GIT` first and then
/// scanning `PATH` while skipping `current_exe()`, `oz`, and `git-ozone` to prevent recursion.
pub fn resolve_real_git() -> Result<PathBuf> {
    let current_exe_canon = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok());

    if let Some(override_raw) = std::env::var_os("OZ_REAL_GIT")
        && !override_raw.is_empty()
    {
        let candidate = PathBuf::from(&override_raw);
        if candidate.components().count() > 1 || candidate.is_file() {
            if !is_self_binary(&candidate, current_exe_canon.as_deref()) && candidate.is_file() {
                return Ok(candidate);
            }
            return Err(OzError::BinaryNotFound {
                name: "git".to_string(),
                message: format!(
                    "OZ_REAL_GIT '{}' is invalid or points to Ozone itself",
                    candidate.display()
                ),
            });
        }
        if let Some(path_var) = std::env::var_os("PATH")
            && let Some(found) =
                find_git_in_path(&override_raw.to_string_lossy(), &path_var, current_exe_canon.as_deref())
        {
            return Ok(found);
        }
        return Err(OzError::BinaryNotFound {
            name: "git".to_string(),
            message: format!("OZ_REAL_GIT '{}' not found on PATH", override_raw.to_string_lossy()),
        });
    }

    let path_var = std::env::var_os("PATH").ok_or_else(|| OzError::BinaryNotFound {
        name: "git".to_string(),
        message: "PATH environment variable is unset".to_string(),
    })?;

    find_git_in_path("git", &path_var, current_exe_canon.as_deref()).ok_or_else(|| {
        OzError::BinaryNotFound {
            name: "git".to_string(),
            message: "could not find system `git` executable on PATH (set OZ_REAL_GIT to override)"
                .to_string(),
        }
    })
}

fn find_git_in_path(
    base_name: &str,
    path_var: &OsStr,
    current_exe_canon: Option<&Path>,
) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = [
        format!("{base_name}.exe"),
        format!("{base_name}.cmd"),
        base_name.to_string(),
    ];
    #[cfg(not(windows))]
    let candidates = [base_name.to_string()];

    for dir in std::env::split_paths(path_var) {
        for cand in &candidates {
            let full = dir.join(cand);
            if !full.is_file() || !is_executable_file(&full) {
                continue;
            }
            if is_self_binary(&full, current_exe_canon) {
                continue;
            }
            return Some(full);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn is_self_binary(candidate: &Path, current_exe_canon: Option<&Path>) -> bool {
    if is_ozone_binary_name(candidate) {
        return true;
    }
    let canon = std::fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf());
    if is_ozone_binary_name(&canon) {
        return true;
    }
    current_exe_canon.is_some_and(|self_p| self_p == canon)
}

fn is_ozone_binary_name(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let lower = file_name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    matches!(stem, "oz" | "git-ozone")
}

fn build_real_git_command(
    parsed: &ParsedGitGlobals,
    subcmd: Option<&str>,
    args: &[OsString],
    suppress_editor: bool,
) -> Result<Command> {
    let real_git = resolve_real_git()?;
    let mut cmd = Command::new(real_git);
    if let Some(ref dir) = parsed.effective_dir {
        cmd.current_dir(dir);
    }
    cmd.args(&parsed.delegated_globals);
    if let Some(sc) = subcmd {
        cmd.arg(sc);
    }
    cmd.args(args);

    for (k, v) in parsed.mode.child_env_overrides() {
        cmd.env(k, v);
    }
    if parsed.no_pager {
        cmd.env("GIT_PAGER", "cat");
        cmd.env("PAGER", "cat");
    }
    let needs_piped_stdin = subcmd.is_some_and(|s| {
        matches!(
            s,
            "check-ignore" | "check-attr" | "hash-object" | "cat-file" | "rev-parse" | "update-index"
        )
    }) && args.iter().any(|a| {
        matches!(
            a.to_string_lossy().as_ref(),
            "--stdin" | "--batch" | "--batch-check"
        )
    });
    if parsed.mode.non_interactive && !needs_piped_stdin {
        cmd.stdin(Stdio::null());
        if suppress_editor {
            cmd.env("GIT_EDITOR", "true");
            cmd.env("EDITOR", "true");
        }
    } else if parsed.mode.non_interactive && suppress_editor {
        cmd.env("GIT_EDITOR", "true");
        cmd.env("EDITOR", "true");
    }
    Ok(cmd)
}

fn raw_code_to_exit_code(code: i32) -> ExitCode {
    match code {
        0 => ExitCode::Success,
        1 => ExitCode::ConflictOrCheckFail,
        _ => ExitCode::UsageOrGuardrail,
    }
}

fn delegate_readonly(
    parsed: &ParsedGitGlobals,
    subcmd_slice: &[&str],
    extra_args: &[OsString],
) -> ExitCode {
    let subcmd = subcmd_slice.first().copied();
    let mut all_args: Vec<OsString> = subcmd_slice
        .iter()
        .skip(1)
        .map(OsString::from)
        .collect();
    all_args.extend(extra_args.iter().cloned());

    let mut cmd = match build_real_git_command(parsed, subcmd, &all_args, false) {
        Ok(c) => c,
        Err(err) => return emit_error(parsed.mode, &err),
    };

    if parsed.mode.json {
        match cmd.output() {
            Ok(out) => {
                let code = out.status.code().unwrap_or(2);
                let cmd_label = format!("git {}", subcmd.unwrap_or_default());
                if out.status.success() {
                    let stdout_str = String::from_utf8_lossy(&out.stdout).into_owned();
                    let env = Envelope::success(serde_json::json!({
                        "command": cmd_label,
                        "exit_code": code,
                        "stdout": stdout_str,
                    }));
                    print_json_envelope(&env);
                    ExitCode::Success
                } else {
                    let stderr_str = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    let msg = if stderr_str.is_empty() {
                        format!("{cmd_label} exited with status {code}")
                    } else {
                        stderr_str
                    };
                    if code == 1 {
                        emit_error(
                            parsed.mode,
                            &OzError::CheckFailed {
                                message: msg,
                                recovery: Vec::new(),
                            },
                        )
                    } else {
                        emit_error(parsed.mode, &OzError::Git(msg))
                    }
                }
            }
            Err(err) => emit_error(parsed.mode, &OzError::Io(err)),
        }
    } else {
        match cmd.status() {
            Ok(st) => raw_code_to_exit_code(st.code().unwrap_or(2)),
            Err(err) => emit_error(parsed.mode, &OzError::Io(err)),
        }
    }
}

fn delegate_mutating(
    parsed: &ParsedGitGlobals,
    subcmd: &str,
    args: &[OsString],
    watch_all_refs: bool,
    suppress_editor: bool,
) -> ExitCode {
    let dir = parsed.effective_dir.as_deref();
    let repo_opt = discover_and_recover_repo(dir, false).ok();

    let tx_opt = match repo_opt {
        Some(ref repo) => {
            let cmd_str = format_command_string(subcmd, args);
            let actor = actor_from_mode(parsed.mode, repo);
            let res = if watch_all_refs {
                MutationTransaction::begin_watching_all_refs(repo, cmd_str, actor, None)
            } else {
                MutationTransaction::begin_watching_all_branches(repo, cmd_str, actor, None)
            };
            match res {
                Ok(tx) => Some(tx),
                Err(err) => return emit_error(parsed.mode, &err),
            }
        }
        None => None,
    };

    let mut cmd = match build_real_git_command(parsed, Some(subcmd), args, suppress_editor) {
        Ok(c) => c,
        Err(err) => return emit_error(parsed.mode, &err),
    };

    let (raw_code, captured_stdout, captured_stderr) = if parsed.mode.json {
        match cmd.output() {
            Ok(out) => (
                out.status.code().unwrap_or(2),
                Some(String::from_utf8_lossy(&out.stdout).into_owned()),
                Some(String::from_utf8_lossy(&out.stderr).into_owned()),
            ),
            Err(err) => return emit_error(parsed.mode, &OzError::Io(err)),
        }
    } else {
        match cmd.status() {
            Ok(st) => (st.code().unwrap_or(2), None, None),
            Err(err) => return emit_error(parsed.mode, &OzError::Io(err)),
        }
    };

    let op_id = if let (Some(repo), Some(tx)) = (repo_opt.as_ref(), tx_opt) {
        let post_repo = OzoneRepo::discover(dir).unwrap_or_else(|_| repo.clone());
        match tx.finish_if_changed(&post_repo, None) {
            Ok(opt_entry) => opt_entry.map(|e| e.op_id),
            Err(err) => return emit_error(parsed.mode, &err),
        }
    } else {
        None
    };

    if raw_code == 0
        && let Some(ref repo) = repo_opt
        && (subcmd == "reset" || subcmd == "clean")
    {
        let _ = sync_pinned_after_clean_or_reset(repo, subcmd, args);
    }

    if parsed.mode.json {
        if raw_code == 0 {
            let mut env = Envelope::success(serde_json::json!({
                "command": format_command_string(subcmd, args),
                "exit_code": raw_code,
                "stdout": captured_stdout.unwrap_or_default(),
            }));
            if let Some(ref id) = op_id {
                env = env
                    .with_op_id(id.clone())
                    .with_recovery_command(format!("oz undo {id}"));
            }
            if let Some(ref repo) = repo_opt {
                let post_repo = OzoneRepo::discover(dir).unwrap_or_else(|_| repo.clone());
                env = env
                    .with_workspace(post_repo.workspace)
                    .with_branch(post_repo.branch)
                    .with_head_oid(post_repo.head_oid);
            }
            print_json_envelope(&env);
            ExitCode::Success
        } else {
            let msg = captured_stderr
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("git {subcmd} exited with code {raw_code}"));
            let mut env = Envelope::failure(
                if raw_code == 1 {
                    "git_command_failed"
                } else {
                    "git_error"
                },
                raw_code_to_exit_code(raw_code),
                msg,
            );
            env.op_id = op_id.clone();
            if let Some(id) = op_id {
                env.recovery_commands.push(format!("oz undo {id}"));
            }
            print_json_envelope(&env);
            raw_code_to_exit_code(raw_code)
        }
    } else {
        let _ = GitDelegatedOutcome {
            command: subcmd.to_string(),
            exit_code: raw_code,
            op_id,
        };
        raw_code_to_exit_code(raw_code)
    }
}

fn sync_pinned_after_clean_or_reset(repo: &OzoneRepo, subcmd: &str, args: &[OsString]) -> Result<()> {
    let pinned = read_pinned_paths(repo)?;
    if pinned.is_empty() {
        return Ok(());
    }
    if subcmd == "reset" && args.iter().any(|a| a == "--hard") {
        return clear_pinned_paths(repo);
    }
    let status = compute_status(repo)?;
    let dirty_set: BTreeSet<&str> = status.changes.iter().map(|c| c.path.as_str()).collect();
    let stale: Vec<String> = pinned
        .iter()
        .filter(|p| !dirty_set.contains(p.as_str()))
        .cloned()
        .collect();
    if !stale.is_empty() {
        let _ = remove_pinned_paths(repo, stale)?;
    }
    Ok(())
}

fn format_command_string(subcmd: &str, args: &[OsString]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 2);
    parts.push("oz git".to_string());
    parts.push(subcmd.to_string());
    for a in args {
        parts.push(a.to_string_lossy().into_owned());
    }
    parts.join(" ")
}

// ============================================================================
// Native: `oz git worktree`
// ============================================================================

fn handle_git_worktree(parsed: &ParsedGitGlobals) -> ExitCode {
    let (args, mode) = extract_json_agent_flags(&parsed.subcommand_args, parsed.mode);
    let Some(action) = args.first().map(String::as_str) else {
        return delegate_readonly(parsed, &["worktree"], &parsed.subcommand_args);
    };
    let rest = &args[1..];
    let dir = parsed.effective_dir.as_deref();

    match action {
        "list" => {
            let mut porcelain = false;
            for arg in rest {
                if arg == "--porcelain" {
                    porcelain = true;
                }
            }
            let res = (|| -> Result<Envelope<Vec<GitWorktreeItem>>> {
                let repo = discover_and_recover_repo(dir, true)?;
                let summaries = list_workspaces(&repo)?;
                let main_repo = OzoneRepo::discover(Some(&repo.repo_root))?;

                let mut items = Vec::new();
                items.push(GitWorktreeItem {
                    name: "main".to_string(),
                    path: main_repo.repo_root.clone(),
                    branch: main_repo.branch.clone(),
                    head_oid: main_repo.head_oid.clone(),
                    is_main: true,
                    healthy: true,
                });

                let mut seen_paths = BTreeSet::new();
                seen_paths.insert(main_repo.repo_root.clone());

                for ws in summaries {
                    seen_paths.insert(ws.state.worktree_path.clone());
                    items.push(GitWorktreeItem {
                        name: ws.state.name,
                        path: ws.state.worktree_path,
                        branch: Some(ws.state.branch),
                        head_oid: ws.branch_head_oid.or(ws.state.base_commit),
                        is_main: false,
                        healthy: ws.healthy,
                    });
                }

                for (name, wt_path) in list_git_worktree_entries(&repo) {
                    if seen_paths.contains(&wt_path) {
                        continue;
                    }
                    let wt_repo = OzoneRepo::discover(Some(&wt_path)).ok();
                    items.push(GitWorktreeItem {
                        name,
                        path: wt_path.clone(),
                        branch: wt_repo.as_ref().and_then(|r| r.branch.clone()),
                        head_oid: wt_repo.as_ref().and_then(|r| r.head_oid.clone()),
                        is_main: false,
                        healthy: wt_path.is_dir(),
                    });
                }

                Ok(Envelope::success(items)
                    .with_workspace(repo.workspace)
                    .with_branch(repo.branch)
                    .with_head_oid(repo.head_oid))
            })();

            emit_json_or_human(mode, res, |env| {
                for item in &env.data {
                    let head_short = item
                        .head_oid
                        .as_deref()
                        .map(|h| &h[..h.len().min(7)])
                        .unwrap_or("0000000");
                    if porcelain {
                        println!("worktree {}", item.path.display());
                        println!("HEAD {}", item.head_oid.as_deref().unwrap_or("0000000"));
                        if let Some(ref br) = item.branch {
                            println!("branch refs/heads/{br}");
                        } else {
                            println!("detached");
                        }
                        println!();
                    } else {
                        let br_label = item
                            .branch
                            .as_deref()
                            .map(|b| format!("[{b}]"))
                            .unwrap_or_else(|| "(detached HEAD)".to_string());
                        println!("{:<40} {head_short} {br_label}", item.path.display());
                    }
                }
            })
        }
        "add" => {
            let res = (|| -> Result<Envelope<serde_json::Value>> {
                let mut branch_opt: Option<String> = None;
                let mut force_branch = false;
                let mut detach = false;
                let mut warm = true;
                let mut scope = Vec::new();
                let mut deny = Vec::new();
                let mut positionals = Vec::new();

                let mut i = 0usize;
                let mut after_sep = false;
                while i < rest.len() {
                    let arg = &rest[i];
                    if arg == "--" {
                        after_sep = true;
                        i += 1;
                        continue;
                    }
                    if !after_sep {
                        if arg == "-b" || arg == "-B" {
                            let val = rest.get(i + 1).ok_or_else(|| OzError::InvalidUsage {
                                message: format!("option `{arg}` requires a branch name"),
                                recovery: vec![
                                    "Example: `oz git worktree add -b feat/x ws-x main`."
                                        .to_string(),
                                ],
                            })?;
                            branch_opt = Some(val.clone());
                            force_branch = arg == "-B";
                            i += 2;
                            continue;
                        }
                        if arg == "--detach" {
                            detach = true;
                            i += 1;
                            continue;
                        }
                        if arg == "-f" || arg == "--force" {
                            i += 1;
                            continue;
                        }
                        if arg == "--no-warm" {
                            warm = false;
                            i += 1;
                            continue;
                        }
                        if arg == "--scope"
                            && let Some(val) = rest.get(i + 1)
                        {
                            scope.push(val.clone());
                            i += 2;
                            continue;
                        }
                        if arg == "--deny"
                            && let Some(val) = rest.get(i + 1)
                        {
                            deny.push(val.clone());
                            i += 2;
                            continue;
                        }
                        if arg.starts_with('-') {
                            return Err(OzError::InvalidUsage {
                                message: format!("unsupported flag `{arg}` for `oz git worktree add`"),
                                recovery: vec![
                                    "Use `oz git worktree add [-b|-B <branch>] <name_or_path> [<from>]`."
                                        .to_string(),
                                ],
                            });
                        }
                    }
                    positionals.push(arg.clone());
                    i += 1;
                }

                let name_or_path = positionals.first().ok_or_else(|| OzError::InvalidUsage {
                    message: "missing workspace name or path for `oz git worktree add`".to_string(),
                    recovery: vec![
                        "Run `oz git worktree add [-b <branch>] <name_or_path> [<from>]`."
                            .to_string(),
                    ],
                })?;
                let from_arg = positionals.get(1).cloned();

                let repo = discover_and_recover_repo(dir, true)?;
                let inv_dir = invocation_dir(parsed);
                let (ws_name, custom_target_path) =
                    parse_worktree_target(&repo, &inv_dir, name_or_path)?;

                let mut spawn_repo = repo.clone();
                if let Some(ref custom_path) = custom_target_path
                    && let Some(parent) = custom_path.parent()
                {
                    spawn_repo.config.workspace_template = format!("{}/{{ws}}", parent.display());
                }

                let pre_status = compute_status(&repo)?;
                let actor = actor_from_mode(mode, &repo);
                let tx = MutationTransaction::begin_watching_all_branches(
                    &repo,
                    format!("oz git worktree add {name_or_path}"),
                    actor,
                    Some(&pre_status.changes),
                )?;

                let (final_branch, final_from) = if let Some(ref br) = branch_opt {
                    if force_branch && is_local_branch(&repo, br)? {
                        let _ = create_or_reset_local_branch(&repo, br, from_arg.as_deref())?;
                        (Some(br.clone()), None)
                    } else {
                        (Some(br.clone()), from_arg)
                    }
                } else if let Some(ref from_spec) = from_arg {
                    if !detach && is_local_branch(&repo, from_spec)? {
                        (Some(from_spec.clone()), None)
                    } else {
                        (Some(ws_name.clone()), Some(from_spec.clone()))
                    }
                } else {
                    (Some(ws_name.clone()), None)
                };

                let opts = SpawnWorkspaceOptions {
                    name: ws_name.clone(),
                    branch: final_branch,
                    from_ref: final_from,
                    backend: None,
                    scope,
                    deny,
                    warm,
                };
                let spawned = spawn_workspace(&spawn_repo, &opts)?;
                let entry = tx.finish(&repo, Some(&pre_status.changes))?;

                Ok(Envelope::success(serde_json::json!({
                    "workspace": spawned.state.name,
                    "branch": spawned.state.branch,
                    "worktree_path": spawned.workspace.workspace_path,
                    "backend": spawned.state.backend,
                    "base_commit": spawned.state.base_commit,
                }))
                .with_op_id(entry.op_id.clone())
                .with_workspace(Some(spawned.state.name.clone()))
                .with_branch(Some(spawned.state.branch))
                .with_head_oid(spawned.state.base_commit)
                .with_recovery_command(format!("oz undo {}", entry.op_id)))
            })();

            emit_json_or_human(mode, res, |env| {
                println!(
                    "Preparing worktree (branch '{}') -> {}",
                    env.data["branch"].as_str().unwrap_or(""),
                    env.data["worktree_path"].as_str().unwrap_or("")
                );
            })
        }
        "remove" => {
            let res = (|| -> Result<Envelope<serde_json::Value>> {
                let mut force = false;
                let mut delete_branch = false;
                let mut target: Option<String> = None;
                for arg in rest {
                    if arg == "-f" || arg == "--force" {
                        force = true;
                    } else if arg == "--delete-branch" {
                        delete_branch = true;
                    } else if !arg.starts_with('-') && target.is_none() {
                        target = Some(arg.clone());
                    } else {
                        return Err(OzError::InvalidUsage {
                            message: format!("unexpected argument `{arg}` for `oz git worktree remove`"),
                            recovery: vec!["Run `oz git worktree remove [-f] <ws>`.".to_string()],
                        });
                    }
                }
                let target_str = target.ok_or_else(|| OzError::InvalidUsage {
                    message: "missing workspace name or path for `oz git worktree remove`"
                        .to_string(),
                    recovery: vec!["Run `oz git worktree remove [-f] <ws>`.".to_string()],
                })?;

                let repo = discover_and_recover_repo(dir, false)?;
                let inv_dir = invocation_dir(parsed);
                let ws_name = resolve_existing_workspace_name(&repo, &inv_dir, &target_str)?;

                let tx_repo = if repo.workspace.as_deref() == Some(ws_name.as_str()) {
                    OzoneRepo::discover(Some(&repo.repo_root))?
                } else {
                    repo.clone()
                };
                let pre_status = compute_status(&tx_repo)?;
                let actor = actor_from_mode(mode, &tx_repo);
                let tx = MutationTransaction::begin_watching_all_branches(
                    &tx_repo,
                    format!("oz git worktree remove {ws_name}"),
                    actor,
                    Some(&pre_status.changes),
                )?;

                let removed = remove_workspace(
                    &repo,
                    &ws_name,
                    &RemoveWorkspaceOptions {
                        force,
                        delete_branch,
                    },
                )?;
                let entry = tx.finish(&tx_repo, Some(&pre_status.changes))?;

                Ok(Envelope::success(serde_json::json!({
                    "name": removed.name,
                    "worktree_path": removed.worktree_path,
                    "branch": removed.branch,
                    "branch_deleted": removed.branch_deleted,
                }))
                .with_op_id(entry.op_id)
                .with_workspace(tx_repo.workspace)
                .with_branch(tx_repo.branch)
                .with_head_oid(tx_repo.head_oid))
            })();

            emit_json_or_human(mode, res, |env| {
                println!(
                    "removed worktree '{}' ({})",
                    env.data["name"].as_str().unwrap_or(""),
                    env.data["worktree_path"].as_str().unwrap_or("")
                );
            })
        }
        "prune" => {
            let res = (|| -> Result<Envelope<serde_json::Value>> {
                let mut dry_run = false;
                for arg in rest {
                    if arg == "-n" || arg == "--dry-run" {
                        dry_run = true;
                    } else if arg != "-v" && arg != "--verbose" {
                        return Err(OzError::InvalidUsage {
                            message: format!("unexpected argument `{arg}` for `oz git worktree prune`"),
                            recovery: vec!["Run `oz git worktree prune [-n]`.".to_string()],
                        });
                    }
                }
                let repo = discover_and_recover_repo(dir, true)?;
                let tx = if dry_run {
                    None
                } else {
                    let pre_status = compute_status(&repo)?;
                    let actor = actor_from_mode(mode, &repo);
                    Some((
                        MutationTransaction::begin_watching_all_branches(
                            &repo,
                            "oz git worktree prune",
                            actor,
                            Some(&pre_status.changes),
                        )?,
                        pre_status.changes,
                    ))
                };

                let report = gc_workspaces(
                    &repo,
                    &GcWorkspacesOptions {
                        target_branch: None,
                        prune_merged: false,
                        prune_missing: true,
                        dry_run,
                    },
                )?;

                // Also prune any orphaned non-Ozone `.git/worktrees/<name>` metadata dirs.
                let mut pruned_git_dirs = Vec::new();
                for (name, wt_path) in list_git_worktree_entries(&repo) {
                    if !wt_path.exists() {
                        pruned_git_dirs.push(name.clone());
                        if !dry_run {
                            let _ = std::fs::remove_dir_all(
                                repo.git_common_dir.join("worktrees").join(&name),
                            );
                        }
                    }
                }

                let mut env = Envelope::success(serde_json::json!({
                    "removed_workspaces": report.removed,
                    "skipped_dirty_workspaces": report.skipped_dirty,
                    "pruned_git_worktrees": pruned_git_dirs,
                    "dry_run": dry_run,
                }))
                .with_workspace(repo.workspace.clone())
                .with_branch(repo.branch.clone())
                .with_head_oid(repo.head_oid.clone());

                if let Some((tx, pre_changes)) = tx {
                    let entry = tx.finish(&repo, Some(&pre_changes))?;
                    env = env.with_op_id(entry.op_id);
                }
                Ok(env)
            })();

            emit_json_or_human(mode, res, |env| {
                let removed = env.data["removed_workspaces"]
                    .as_array()
                    .map(Vec::len)
                    .unwrap_or(0);
                let extra = env.data["pruned_git_worktrees"]
                    .as_array()
                    .map(Vec::len)
                    .unwrap_or(0);
                println!("pruned {} workspace(s) and {} stale git worktree dir(s)", removed, extra);
            })
        }
        _ => delegate_readonly(parsed, &["worktree"], &parsed.subcommand_args),
    }
}

fn parse_worktree_target(
    repo: &OzoneRepo,
    inv_dir: &Path,
    name_or_path: &str,
) -> Result<(String, Option<PathBuf>)> {
    let looks_like_path = name_or_path.contains('/')
        || name_or_path.contains('\\')
        || name_or_path == "."
        || name_or_path == "..";
    if !looks_like_path {
        validate_workspace_name(name_or_path)?;
        return Ok((name_or_path.to_string(), None));
    }

    let raw_p = Path::new(name_or_path);
    let abs = if raw_p.is_absolute() {
        normalize_lexical(raw_p)
    } else {
        normalize_lexical(&inv_dir.join(raw_p))
    };
    let ws_name = abs
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!("cannot derive workspace name from path '{name_or_path}'"),
            recovery: Vec::new(),
        })?
        .to_string();
    validate_workspace_name(&ws_name)?;

    let default_path = repo.config.render_workspace_path(&repo.repo_root, &ws_name)?;
    if abs == default_path {
        Ok((ws_name, None))
    } else {
        Ok((ws_name, Some(abs)))
    }
}

fn resolve_existing_workspace_name(
    repo: &OzoneRepo,
    inv_dir: &Path,
    target: &str,
) -> Result<String> {
    let summaries = list_workspaces(repo)?;
    if summaries.iter().any(|s| s.state.name == target) {
        return Ok(target.to_string());
    }
    let candidate = {
        let p = Path::new(target);
        if p.is_absolute() {
            normalize_lexical(p)
        } else {
            normalize_lexical(&inv_dir.join(p))
        }
    };
    if let Some(found) = summaries.iter().find(|s| {
        normalize_lexical(&s.state.worktree_path) == candidate
            || std::fs::canonicalize(&s.state.worktree_path)
                .ok()
                .zip(std::fs::canonicalize(&candidate).ok())
                .is_some_and(|(a, b)| a == b)
    }) {
        return Ok(found.state.name.clone());
    }
    if let Some(name) = candidate.file_name().and_then(|s| s.to_str())
        && summaries.iter().any(|s| s.state.name == name)
    {
        return Ok(name.to_string());
    }
    Ok(target.to_string())
}

// ============================================================================
// Native: `oz git status`
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusOutputFormat {
    Human,
    Short,
    Porcelain,
    PorcelainV2,
}

fn handle_git_status(parsed: &ParsedGitGlobals) -> ExitCode {
    let (args, mode) = extract_json_agent_flags(&parsed.subcommand_args, parsed.mode);

    let mut format = StatusOutputFormat::Human;
    let mut show_branch = false;
    let mut show_untracked = true;
    let mut explicit_untracked = false;
    let mut nul_terminated = false;
    let mut pathspecs = Vec::new();
    let mut after_sep = false;

    for raw in &args {
        if raw == "--" {
            after_sep = true;
            continue;
        }
        if !after_sep {
            if raw == "--ignored" || raw.starts_with("--ignored=") {
                return delegate_readonly(parsed, &["status"], &parsed.subcommand_args);
            }
            if raw == "--porcelain=v2" || raw == "--porcelain=2" {
                format = StatusOutputFormat::PorcelainV2;
                continue;
            }
            if raw == "--porcelain" || raw == "--porcelain=v1" || raw == "--porcelain=1" {
                format = StatusOutputFormat::Porcelain;
                continue;
            }
            if raw == "-z" || raw == "--null" {
                nul_terminated = true;
                if format == StatusOutputFormat::Human {
                    format = StatusOutputFormat::Porcelain;
                }
                continue;
            }
            if raw == "-s" || raw == "--short" {
                format = StatusOutputFormat::Short;
                continue;
            }
            if raw == "-b" || raw == "--branch" {
                show_branch = true;
                continue;
            }
            if raw == "-uno" || raw == "--untracked-files=no" {
                show_untracked = false;
                explicit_untracked = true;
                continue;
            }
            if raw == "-u"
                || raw == "-uall"
                || raw == "-unormal"
                || raw.starts_with("--untracked-files")
            {
                show_untracked = true;
                explicit_untracked = true;
                continue;
            }
            if raw == "--no-renames"
                || raw == "--renames"
                || raw == "--ahead-behind"
                || raw == "--no-ahead-behind"
                || raw.starts_with("--ignore-submodules")
            {
                continue;
            }
            if raw.starts_with('-') {
                // Unknown status flag -> delegate read-only if not --json
                if !mode.json {
                    return delegate_readonly(parsed, &["status"], &parsed.subcommand_args);
                }
                return emit_error(
                    mode,
                    &OzError::InvalidUsage {
                        message: format!("unsupported flag `{raw}` for `oz git status --json`"),
                        recovery: vec!["Run `oz git status --json`.".to_string()],
                    },
                );
            }
        }
        pathspecs.push(raw.clone());
    }
    if nul_terminated && !explicit_untracked {
        show_untracked = true;
    }

    let inv_dir = invocation_dir(parsed);
    let mut repo_ctx: Option<OzoneRepo> = None;
    let res = (|| -> Result<Envelope<oz_core::StatusReport>> {
        let repo = discover_and_recover_repo(parsed.effective_dir.as_deref(), false)?;
        let mut report = compute_status_with_overrides(&repo, &parsed.config_overrides)?;

        if !pathspecs.is_empty() {
            let resolved_specs: Vec<PathspecMatcher> = pathspecs
                .iter()
                .map(|p| {
                    let rel = resolve_pathspec_to_repo_rel(&repo, &inv_dir, p);
                    PathspecMatcher::compile(&rel)
                })
                .collect::<Result<Vec<_>>>()?;
            report
                .changes
                .retain(|c| resolved_specs.iter().any(|m| m.matches(&c.path)));
        }

        if !show_untracked {
            report.changes.retain(|c| {
                c.pinned
                    || !matches!(
                        c.kind,
                        oz_core::ChangeKind::Added | oz_core::ChangeKind::Untracked
                    )
            });
        }

        repo_ctx = Some(repo.clone());
        let workspace = report.workspace.clone();
        let branch = report.branch.clone();
        let head_oid = report.head_oid.clone();
        let env = Envelope::success(report)
            .with_workspace(workspace)
            .with_branch(branch)
            .with_head_oid(head_oid);
        Ok(attach_out_of_scope_warnings(&repo, env))
    })();

    emit_json_or_human(mode, res, |env| {
        let report = &env.data;
        let delim = if nul_terminated { '\0' } else { '\n' };
        match format {
            StatusOutputFormat::Porcelain | StatusOutputFormat::Short => {
                if show_branch {
                    let header = match (&report.branch, &report.head_oid) {
                        (Some(br), None) => format!("## No commits yet on {br}"),
                        (Some(br), Some(_)) => format!("## {br}"),
                        (None, _) => "## HEAD (no branch)".to_string(),
                    };
                    print!("{header}{delim}");
                }
                for change in &report.changes {
                    let (x, y) = status_xy_codes(change);
                    print!("{x}{y} {}{delim}", change.path);
                }
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            StatusOutputFormat::PorcelainV2 => {
                render_porcelain_v2_status(
                    repo_ctx.as_ref(),
                    report,
                    show_branch,
                    delim,
                    &parsed.config_overrides,
                );
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            StatusOutputFormat::Human => {
                match (&report.branch, &report.head_oid) {
                    (Some(br), _) => {
                        if let Some(ref ws) = report.workspace {
                            println!("On branch {br} (workspace: {ws})");
                        } else {
                            println!("On branch {br}");
                        }
                    }
                    (None, Some(oid)) => {
                        println!("HEAD detached at {}", &oid[..oid.len().min(7)]);
                    }
                    (None, None) => println!("HEAD detached (unborn)"),
                }
                if report.head_oid.is_none() {
                    println!("\nNo commits yet\n");
                }
                if report.changes.is_empty() {
                    println!("nothing to commit, working tree clean");
                    return;
                }

                let staged: Vec<&FileChange> =
                    report.changes.iter().filter(|c| c.pinned).collect();
                let unstaged: Vec<&FileChange> = report
                    .changes
                    .iter()
                    .filter(|c| {
                        !c.pinned
                            && !matches!(
                                c.kind,
                                oz_core::ChangeKind::Added | oz_core::ChangeKind::Untracked
                            )
                    })
                    .collect();
                let untracked: Vec<&FileChange> = report
                    .changes
                    .iter()
                    .filter(|c| {
                        !c.pinned
                            && matches!(
                                c.kind,
                                oz_core::ChangeKind::Added | oz_core::ChangeKind::Untracked
                            )
                    })
                    .collect();

                if !staged.is_empty() {
                    println!("Changes to be committed:");
                    println!("  (use \"oz git reset HEAD <file>...\" to unstage)");
                    for c in &staged {
                        let label = match c.kind {
                            oz_core::ChangeKind::Added => "new file:",
                            oz_core::ChangeKind::Deleted => "deleted:",
                            oz_core::ChangeKind::TypeChanged => "typechange:",
                            _ => "modified:",
                        };
                        println!("\t{:<12} {}", label, c.path);
                    }
                    println!();
                }
                if !unstaged.is_empty() {
                    println!("Changes not staged for commit:");
                    println!("  (use \"oz git add <file>...\" to update what will be committed)");
                    for c in &unstaged {
                        let label = match c.kind {
                            oz_core::ChangeKind::Deleted => "deleted:",
                            oz_core::ChangeKind::TypeChanged => "typechange:",
                            _ => "modified:",
                        };
                        println!("\t{:<12} {}", label, c.path);
                    }
                    println!();
                }
                if !untracked.is_empty() {
                    println!("Untracked files:");
                    println!(
                        "  (use \"oz git add <file>...\" to include in what will be committed)"
                    );
                    for c in &untracked {
                        println!("\t{}", c.path);
                    }
                }
            }
        }
    })
}

fn status_xy_codes(change: &FileChange) -> (char, char) {
    match (change.kind, change.pinned) {
        (oz_core::ChangeKind::Added | oz_core::ChangeKind::Untracked, true) => ('A', ' '),
        (oz_core::ChangeKind::Added | oz_core::ChangeKind::Untracked, false) => ('?', '?'),
        (oz_core::ChangeKind::Modified, true) => ('M', ' '),
        (oz_core::ChangeKind::Modified, false) => (' ', 'M'),
        (oz_core::ChangeKind::Deleted, true) => ('D', ' '),
        (oz_core::ChangeKind::Deleted, false) => (' ', 'D'),
        (oz_core::ChangeKind::TypeChanged, true) => ('T', ' '),
        (oz_core::ChangeKind::TypeChanged, false) => (' ', 'T'),
        (oz_core::ChangeKind::Renamed, true) => ('R', ' '),
        (oz_core::ChangeKind::Renamed, false) => (' ', 'R'),
        (oz_core::ChangeKind::Copied, true) => ('C', ' '),
        (oz_core::ChangeKind::Copied, false) => (' ', 'C'),
        (oz_core::ChangeKind::Conflicted, _) => ('U', 'U'),
    }
}

fn render_porcelain_v2_status(
    repo_opt: Option<&OzoneRepo>,
    report: &oz_core::StatusReport,
    show_branch: bool,
    delim: char,
    config_overrides: &[(String, String)],
) {
    if show_branch {
        let oid = report.head_oid.as_deref().unwrap_or("(initial)");
        let head = report.branch.as_deref().unwrap_or("(detached)");
        print!("# branch.oid {oid}{delim}");
        print!("# branch.head {head}{delim}");
        if let Some(repo) = repo_opt
            && let Some(br) = report.branch.as_deref()
            && let Ok(cfg) = repo.load_git_config(config_overrides)
            && let Some(remote) = cfg.get_str("branch", Some(br), "remote")
            && let Some(merge_ref) = cfg.get_str("branch", Some(br), "merge")
        {
            let short_merge = merge_ref.strip_prefix("refs/heads/").unwrap_or(merge_ref);
            print!("# branch.upstream {remote}/{short_merge}{delim}");
        }
    }

    let head_entries = if let Some(repo) = repo_opt
        && let Some(ref tree_hex) = report.base_tree
    {
        let paths: Vec<&str> = report.changes.iter().map(|c| c.path.as_str()).collect();
        lookup_tree_entries_meta(repo, tree_hex, &paths)
    } else {
        std::collections::BTreeMap::new()
    };

    const ZERO_OID: &str = "0000000000000000000000000000000000000000";

    for change in &report.changes {
        if !change.pinned
            && matches!(
                change.kind,
                oz_core::ChangeKind::Added | oz_core::ChangeKind::Untracked
            )
        {
            print!("? {}{delim}", change.path);
            continue;
        }

        let (x_raw, y_raw) = status_xy_codes(change);
        let x = if x_raw == ' ' { '.' } else { x_raw };
        let y = if y_raw == ' ' { '.' } else { y_raw };

        let (head_mode, head_oid) = head_entries
            .get(&change.path)
            .map(|(m, oid)| (*m, oid.as_str()))
            .unwrap_or(("000000", ZERO_OID));

        let work_mode = repo_opt
            .and_then(|r| std::fs::symlink_metadata(r.workdir_path.join(&change.path)).ok())
            .map(|m| disk_meta_octal(&m))
            .unwrap_or_else(|| "000000".to_string());

        let idx_mode = if change.pinned {
            work_mode.as_str()
        } else {
            head_mode
        };
        let sub = if head_mode == "160000" || work_mode == "160000" {
            "S..."
        } else {
            "N..."
        };

        print!(
            "1 {x}{y} {sub} {head_mode} {idx_mode} {work_mode} {head_oid} {head_oid} {}{delim}",
            change.path
        );
    }
}

fn disk_meta_octal(meta: &std::fs::Metadata) -> String {
    if meta.file_type().is_symlink() {
        return "120000".to_string();
    }
    if meta.file_type().is_dir() {
        return "160000".to_string();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if (meta.permissions().mode() & 0o111) != 0 {
            return "100755".to_string();
        }
    }
    "100644".to_string()
}

// ============================================================================
// Native: `oz git diff`
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffFormat {
    Unified,
    NameOnly,
    NameStatus,
    NumStat,
    ShortStat,
    Stat,
}

fn handle_git_diff(parsed: &ParsedGitGlobals) -> ExitCode {
    let (args, mode) = extract_json_agent_flags(&parsed.subcommand_args, parsed.mode);

    let mut cached = false;
    let mut format = DiffFormat::Unified;
    let mut exit_code_flag = false;
    let mut quiet = false;
    let mut nul_terminated = false;
    let mut ignore_all_submodules = false;
    let mut before_sep = Vec::new();
    let mut after_sep_paths = Vec::new();
    let mut after_sep = false;

    for raw in &args {
        if raw == "--" {
            after_sep = true;
            continue;
        }
        if after_sep {
            after_sep_paths.push(raw.clone());
            continue;
        }
        if raw == "--cached" || raw == "--staged" {
            cached = true;
            continue;
        }
        if raw == "--name-only" {
            format = DiffFormat::NameOnly;
            continue;
        }
        if raw == "--name-status" {
            format = DiffFormat::NameStatus;
            continue;
        }
        if raw == "--numstat" {
            format = DiffFormat::NumStat;
            continue;
        }
        if raw == "--shortstat" {
            format = DiffFormat::ShortStat;
            continue;
        }
        if raw == "--stat" {
            format = DiffFormat::Stat;
            continue;
        }
        if raw == "-z" || raw == "--null" {
            nul_terminated = true;
            continue;
        }
        if raw == "--exit-code" {
            exit_code_flag = true;
            continue;
        }
        if raw == "--quiet" {
            quiet = true;
            exit_code_flag = true;
            continue;
        }
        if raw == "--ignore-submodules" || raw == "--ignore-submodules=all" {
            ignore_all_submodules = true;
            continue;
        }
        if raw.starts_with("--ignore-submodules=") {
            continue;
        }
        if raw == "-p"
            || raw == "--patch"
            || raw == "--no-color"
            || raw == "--no-ext-diff"
            || raw == "--no-renames"
            || raw == "--no-textconv"
            || raw == "--histogram"
            || raw == "--patience"
            || raw.starts_with("--color")
            || raw.starts_with("-U")
            || raw.starts_with("--unified")
        {
            continue;
        }
        if raw.starts_with('-') {
            return delegate_readonly(parsed, &["diff"], &parsed.subcommand_args);
        }
        before_sep.push(raw.clone());
    }

    let inv_dir = invocation_dir(parsed);
    let repo = match discover_and_recover_repo(parsed.effective_dir.as_deref(), false) {
        Ok(r) => r,
        Err(err) => return emit_error(mode, &err),
    };

    // Detect commit ranges (`A..B`, `A...B`) or commit revisions other than plain `HEAD`
    // and delegate those read-only to real system `git`.
    let mut path_targets = Vec::new();
    for tok in &before_sep {
        if tok.contains("..") {
            return delegate_readonly(parsed, &["diff"], &parsed.subcommand_args);
        }
        let disk_exists = inv_dir.join(tok).exists() || repo.workdir_path.join(tok).exists();
        if !disk_exists && tok != "HEAD" && resolves_as_revision(&repo, tok) {
            return delegate_readonly(parsed, &["diff"], &parsed.subcommand_args);
        }
        if tok == "HEAD" && !disk_exists {
            if cached {
                return delegate_readonly(parsed, &["diff"], &parsed.subcommand_args);
            }
            // `oz git diff HEAD` is equivalent to working tree vs HEAD.
            continue;
        }
        path_targets.push(resolve_pathspec_to_repo_rel(&repo, &inv_dir, tok));
    }
    for tok in &after_sep_paths {
        path_targets.push(resolve_pathspec_to_repo_rel(&repo, &inv_dir, tok));
    }

    let res = (|| -> Result<Envelope<DiffData>> {
        let mut report =
            compute_diff_with_overrides(&repo, &path_targets, &parsed.config_overrides)?;
        if cached {
            report.files.retain(|f| f.pinned);
        }
        if ignore_all_submodules {
            let paths: Vec<&str> = report.files.iter().map(|f| f.path.as_str()).collect();
            let head_meta = if let Some(ref tree_hex) = compute_status(&repo).ok().and_then(|s| s.base_tree) {
                lookup_tree_entries_meta(&repo, tree_hex, &paths)
            } else {
                std::collections::BTreeMap::new()
            };
            report
                .files
                .retain(|f| head_meta.get(&f.path).is_none_or(|(m, _)| *m != "160000"));
        }
        if cached || ignore_all_submodules {
            report.total_additions = report.files.iter().map(|f| f.additions).sum();
            report.total_deletions = report.files.iter().map(|f| f.deletions).sum();
        }
        let unified_diff = render_unified_diff(&report);
        let ws = report.workspace.clone();
        let br = report.branch.clone();
        let oid = report.head_oid.clone();
        Ok(Envelope::success(DiffData {
            report,
            unified_diff,
        })
        .with_workspace(ws)
        .with_branch(br)
        .with_head_oid(oid))
    })();

    let has_diff = res.as_ref().is_ok_and(|env| !env.data.report.files.is_empty());

    let code = emit_json_or_human(mode, res, |env| {
        if quiet {
            return;
        }
        let report = &env.data.report;
        let delim = if nul_terminated { '\0' } else { '\n' };
        match format {
            DiffFormat::Unified => {
                if !env.data.unified_diff.is_empty() {
                    print!("{}", env.data.unified_diff);
                }
            }
            DiffFormat::NameOnly => {
                for f in &report.files {
                    print!("{}{delim}", f.path);
                }
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            DiffFormat::NameStatus => {
                for f in &report.files {
                    print!("{}\t{}{delim}", f.kind.short_code(), f.path);
                }
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            DiffFormat::NumStat => {
                for f in &report.files {
                    if f.is_binary {
                        print!("-\t-\t{}{delim}", f.path);
                    } else {
                        print!("{}\t{}\t{}{delim}", f.additions, f.deletions, f.path);
                    }
                }
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            DiffFormat::ShortStat => {
                if report.files.is_empty() {
                    return;
                }
                let file_word = if report.files.len() == 1 {
                    "file"
                } else {
                    "files"
                };
                let ins_word = if report.total_additions == 1 {
                    "insertion"
                } else {
                    "insertions"
                };
                let del_word = if report.total_deletions == 1 {
                    "deletion"
                } else {
                    "deletions"
                };
                println!(
                    " {} {file_word} changed, {} {ins_word}(+), {} {del_word}(-)",
                    report.files.len(),
                    report.total_additions,
                    report.total_deletions
                );
            }
            DiffFormat::Stat => {
                if report.files.is_empty() {
                    return;
                }
                let max_len = report
                    .files
                    .iter()
                    .map(|f| f.path.len())
                    .max()
                    .unwrap_or(0);
                for f in &report.files {
                    if f.is_binary {
                        println!(" {:<max_len$} | Bin", f.path);
                    } else {
                        let total = f.additions + f.deletions;
                        let plus_n = (f.additions as usize).min(20);
                        let minus_n = (f.deletions as usize).min(20);
                        println!(
                            " {:<max_len$} | {:>4} {}{}",
                            f.path,
                            total,
                            "+".repeat(plus_n),
                            "-".repeat(minus_n)
                        );
                    }
                }
                println!(
                    " {} file(s) changed, {} insertions(+), {} deletions(-)",
                    report.files.len(),
                    report.total_additions,
                    report.total_deletions
                );
            }
        }
    });

    if code == ExitCode::Success && exit_code_flag && has_diff {
        ExitCode::ConflictOrCheckFail
    } else {
        code
    }
}

// ============================================================================
// Native: `oz git add` & `oz git reset` (virtual pinned set)
// ============================================================================

fn handle_git_add(parsed: &ParsedGitGlobals) -> ExitCode {
    let (args, mode) = extract_json_agent_flags(&parsed.subcommand_args, parsed.mode);

    let mut all_flag = false;
    let mut update_only = false;
    let mut dry_run = false;
    let mut verbose = false;
    let mut pathspecs = Vec::new();
    let mut after_sep = false;

    for raw in &args {
        if raw == "--" {
            after_sep = true;
            continue;
        }
        if !after_sep {
            if raw == "-p" || raw == "--patch" || raw == "-i" || raw == "--interactive" {
                let err = OzError::InvalidUsage {
                    message: format!("interactive staging flag `{raw}` is not supported non-interactively"),
                    recovery: vec![
                        "Use `oz status` + `oz commit -m \"...\" --pick <h1a>` for surgical hunk commits."
                            .to_string(),
                    ],
                };
                return emit_error(mode, &err);
            }
            if raw == "-A" || raw == "--all" {
                all_flag = true;
                continue;
            }
            if raw == "-u" || raw == "--update" {
                update_only = true;
                continue;
            }
            if raw == "-n" || raw == "--dry-run" {
                dry_run = true;
                continue;
            }
            if raw == "-v" || raw == "--verbose" {
                verbose = true;
                continue;
            }
            if raw == "-f" || raw == "--force" {
                continue;
            }
            if raw.starts_with('-') {
                return emit_error(
                    mode,
                    &OzError::InvalidUsage {
                        message: format!("unsupported flag `{raw}` for `oz git add`"),
                        recovery: vec!["Run `oz git add [-A|-u] [--] <pathspec>...`.".to_string()],
                    },
                );
            }
        }
        pathspecs.push(raw.clone());
    }

    if pathspecs.is_empty() && !all_flag && !update_only {
        if mode.json {
            let env = Envelope::success(GitAddData {
                pinned_paths: Vec::new(),
                total_pinned: 0,
                dry_run,
            });
            print_json_envelope(&env);
        } else {
            eprintln!("Nothing specified, nothing added.");
            eprintln!("hint: Maybe you wanted to say 'oz git add .'?");
        }
        return ExitCode::Success;
    }

    let inv_dir = invocation_dir(parsed);
    let res = (|| -> Result<Envelope<GitAddData>> {
        let repo = discover_and_recover_repo(parsed.effective_dir.as_deref(), false)?;
        let status = compute_status(&repo)?;
        let tracked_in_head = if pathspecs.is_empty() {
            BTreeSet::new()
        } else {
            list_tracked_paths_in_head(&repo)?
        };

        let mut matched: BTreeSet<String> = BTreeSet::new();
        if pathspecs.is_empty() {
            for c in &status.changes {
                if update_only
                    && matches!(
                        c.kind,
                        oz_core::ChangeKind::Added | oz_core::ChangeKind::Untracked
                    )
                {
                    continue;
                }
                matched.insert(c.path.clone());
            }
        } else {
            for raw_spec in &pathspecs {
                let rel_spec = resolve_pathspec_to_repo_rel(&repo, &inv_dir, raw_spec);
                let matcher = PathspecMatcher::compile(&rel_spec)?;
                let mut hit = false;
                for c in &status.changes {
                    if matcher.matches(&c.path) {
                        hit = true;
                        if update_only
                            && matches!(
                                c.kind,
                                oz_core::ChangeKind::Added | oz_core::ChangeKind::Untracked
                            )
                        {
                            continue;
                        }
                        matched.insert(c.path.clone());
                    }
                }
                if !hit {
                    let exists_on_disk = inv_dir.join(raw_spec).exists()
                        || repo.workdir_path.join(&rel_spec).exists();
                    let matches_tracked = tracked_in_head.iter().any(|p| matcher.matches(p));
                    if !exists_on_disk && !matches_tracked {
                        return Err(OzError::InvalidUsage {
                            message: format!("pathspec '{raw_spec}' did not match any files"),
                            recovery: vec!["Run `oz git status` to inspect changed files.".to_string()],
                        });
                    }
                }
            }
        }

        let pinned_paths: Vec<String> = matched.iter().cloned().collect();
        let total_pinned = if dry_run {
            let mut cur = read_pinned_paths(&repo)?;
            cur.extend(matched);
            cur.len()
        } else {
            add_pinned_paths(&repo, &pinned_paths)?.len()
        };

        Ok(Envelope::success(GitAddData {
            pinned_paths,
            total_pinned,
            dry_run,
        })
        .with_workspace(repo.workspace)
        .with_branch(repo.branch)
        .with_head_oid(repo.head_oid))
    })();

    emit_json_or_human(mode, res, |env| {
        if verbose || dry_run {
            for p in &env.data.pinned_paths {
                println!("add '{p}'");
            }
        }
    })
}

fn handle_git_reset(parsed: &ParsedGitGlobals) -> ExitCode {
    let (args, mode) = extract_json_agent_flags(&parsed.subcommand_args, parsed.mode);

    let has_mode_flag = args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--hard" | "--soft" | "--mixed" | "--merge" | "--keep" | "-p" | "--patch"
        )
    });
    if has_mode_flag {
        return delegate_mutating(parsed, "reset", &parsed.subcommand_args, false, false);
    }

    // Check if `oz git reset [HEAD] [--] <paths...>` or `oz git reset [HEAD]` (unpin mode).
    let mut after_sep = false;
    let mut quiet = false;
    let mut positionals = Vec::new();
    for arg in &args {
        if arg == "--" {
            after_sep = true;
            continue;
        }
        if !after_sep {
            if arg == "-q" || arg == "--quiet" {
                quiet = true;
                continue;
            }
            if arg.starts_with('-') {
                return delegate_mutating(parsed, "reset", &parsed.subcommand_args, false, false);
            }
        }
        positionals.push(arg.clone());
    }

    let inv_dir = invocation_dir(parsed);
    let repo = match discover_and_recover_repo(parsed.effective_dir.as_deref(), false) {
        Ok(r) => r,
        Err(err) => return emit_error(mode, &err),
    };

    // Determine if first positional is a non-HEAD revision (e.g. `oz git reset HEAD~1`).
    let path_specs: Vec<String> = if positionals.is_empty() {
        Vec::new()
    } else if positionals[0] == "HEAD" {
        positionals[1..].to_vec()
    } else if !after_sep && positionals.len() == 1 {
        let first = &positionals[0];
        let is_pinned_or_disk = read_pinned_paths(&repo)
            .ok()
            .is_some_and(|p| p.contains(&resolve_pathspec_to_repo_rel(&repo, &inv_dir, first)))
            || inv_dir.join(first).exists();
        if !is_pinned_or_disk && resolves_as_revision(&repo, first) {
            return delegate_mutating(parsed, "reset", &parsed.subcommand_args, false, false);
        }
        positionals
    } else {
        positionals
    };

    let res = (|| -> Result<Envelope<GitResetPinnedData>> {
        let current_pinned = read_pinned_paths(&repo)?;
        let (unpinned, remaining) = if path_specs.is_empty() {
            let removed: Vec<String> = current_pinned.into_iter().collect();
            clear_pinned_paths(&repo)?;
            (removed, 0usize)
        } else {
            let matchers: Vec<PathspecMatcher> = path_specs
                .iter()
                .map(|p| {
                    let rel = resolve_pathspec_to_repo_rel(&repo, &inv_dir, p);
                    PathspecMatcher::compile(&rel)
                })
                .collect::<Result<Vec<_>>>()?;
            let to_remove: Vec<String> = current_pinned
                .iter()
                .filter(|p| matchers.iter().any(|m| m.matches(p)))
                .cloned()
                .collect();
            let rem = remove_pinned_paths(&repo, &to_remove)?;
            (to_remove, rem.len())
        };

        Ok(Envelope::success(GitResetPinnedData {
            unpinned_paths: unpinned,
            remaining_pinned: remaining,
        })
        .with_workspace(repo.workspace)
        .with_branch(repo.branch)
        .with_head_oid(repo.head_oid))
    })();

    emit_json_or_human(mode, res, |env| {
        if !quiet && !env.data.unpinned_paths.is_empty() {
            println!("Unpinned {} file(s)", env.data.unpinned_paths.len());
        }
    })
}

// ============================================================================
// Native: `oz git commit`
// ============================================================================

fn handle_git_commit(parsed: &ParsedGitGlobals) -> ExitCode {
    let (args, mode) = extract_json_agent_flags(&parsed.subcommand_args, parsed.mode);

    let mut messages: Vec<String> = Vec::new();
    let mut msg_file: Option<String> = None;
    let mut all_flag = false;
    let mut allow_empty = false;
    let mut force = false;
    let mut allow_out_of_scope = false;
    let mut amend = false;
    let mut no_edit = false;
    let mut quiet = false;
    let mut explicit_sign: Option<bool> = None;
    let mut explicit_signing_key: Option<String> = None;
    let mut config_gpgsign: Option<bool> = None;
    let mut config_gpg_format: Option<String> = None;
    let mut config_signing_key: Option<String> = None;
    let mut author_name: Option<String> = None;
    let mut author_email: Option<String> = None;
    let mut pathspecs: Vec<String> = Vec::new();

    for (k, v) in &parsed.config_overrides {
        if k.eq_ignore_ascii_case("user.name") {
            author_name = Some(v.clone());
        } else if k.eq_ignore_ascii_case("user.email") {
            author_email = Some(v.clone());
        } else if k.eq_ignore_ascii_case("user.signingkey") {
            config_signing_key = Some(v.clone());
        } else if k.eq_ignore_ascii_case("gpg.format") {
            config_gpg_format = Some(v.clone());
        } else if k.eq_ignore_ascii_case("commit.gpgsign") {
            config_gpgsign = parse_git_bool(v);
        }
    }

    let mut no_verify = false;
    let mut i = 0usize;
    let mut after_sep = false;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            after_sep = true;
            i += 1;
            continue;
        }
        if after_sep {
            pathspecs.push(arg.clone());
            i += 1;
            continue;
        }

        if arg == "-m" || arg == "--message" {
            let Some(val) = args.get(i + 1) else {
                return emit_error(
                    mode,
                    &OzError::InvalidUsage {
                        message: format!("option `{arg}` requires a value"),
                        recovery: vec!["Pass `oz git commit -m \"feat: ...\"`.".to_string()],
                    },
                );
            };
            messages.push(val.clone());
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--message=") {
            messages.push(val.to_string());
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("-m")
            && !val.is_empty()
        {
            messages.push(val.to_string());
            i += 1;
            continue;
        }

        if arg == "-F" || arg == "--file" {
            let Some(val) = args.get(i + 1) else {
                return emit_error(
                    mode,
                    &OzError::InvalidUsage {
                        message: format!("option `{arg}` requires a file path"),
                        recovery: vec!["Pass `oz git commit -F <file>`.".to_string()],
                    },
                );
            };
            msg_file = Some(val.clone());
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--file=") {
            msg_file = Some(val.to_string());
            i += 1;
            continue;
        }

        if arg == "-S" || arg == "--gpg-sign" {
            explicit_sign = Some(true);
            i += 1;
            continue;
        }
        if arg == "--no-gpg-sign" {
            explicit_sign = Some(false);
            explicit_signing_key = None;
            i += 1;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--gpg-sign=") {
            explicit_sign = Some(true);
            if !val.trim().is_empty() {
                explicit_signing_key = Some(val.trim().to_string());
            }
            i += 1;
            continue;
        }

        if arg == "-a" || arg == "--all" {
            all_flag = true;
            i += 1;
            continue;
        }
        if arg == "--allow-empty" {
            allow_empty = true;
            i += 1;
            continue;
        }
        if arg == "-f" || arg == "--force" {
            force = true;
            i += 1;
            continue;
        }
        if arg == "--allow-out-of-scope" {
            allow_out_of_scope = true;
            i += 1;
            continue;
        }
        if arg == "--amend" {
            amend = true;
            i += 1;
            continue;
        }
        if arg == "--no-edit" {
            no_edit = true;
            i += 1;
            continue;
        }
        if arg == "-q" || arg == "--quiet" {
            quiet = true;
            i += 1;
            continue;
        }
        if arg == "--author"
            && let Some(val) = args.get(i + 1)
        {
            if let Some((n, e)) = parse_author_spec(val) {
                author_name = Some(n);
                author_email = Some(e);
            }
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--author=") {
            if let Some((n, e)) = parse_author_spec(val) {
                author_name = Some(n);
                author_email = Some(e);
            }
            i += 1;
            continue;
        }
        if arg == "-n" || arg == "--no-verify" {
            no_verify = true;
            i += 1;
            continue;
        }
        if arg == "-v" || arg == "--verbose" {
            i += 1;
            continue;
        }

        // Combined short flags starting with boolean short flags (`-am`, `-aS`, `-nvq`, etc.)
        if let Some(short) = arg.strip_prefix('-')
            && !short.starts_with('-')
            && short.len() > 1
            && short
                .chars()
                .next()
                .is_some_and(|c| matches!(c, 'a' | 'n' | 'v' | 'q' | 'f'))
        {
            let mut handled = true;
            let mut consumed_next = false;
            for (byte_idx, ch) in short.char_indices() {
                match ch {
                    'a' => all_flag = true,
                    'n' => no_verify = true,
                    'v' => {}
                    'q' => quiet = true,
                    'f' => force = true,
                    'S' => {
                        explicit_sign = Some(true);
                        let rest = &short[byte_idx + ch.len_utf8()..];
                        if !rest.is_empty() {
                            explicit_signing_key = Some(rest.to_string());
                        }
                        break;
                    }
                    'm' => {
                        let rest = &short[byte_idx + ch.len_utf8()..];
                        if !rest.is_empty() {
                            messages.push(rest.to_string());
                        } else if let Some(val) = args.get(i + 1) {
                            messages.push(val.clone());
                            consumed_next = true;
                        } else {
                            return emit_error(
                                mode,
                                &OzError::InvalidUsage {
                                    message: "option `-m` requires a value".to_string(),
                                    recovery: vec![
                                        "Pass `oz git commit -m \"feat: ...\"`.".to_string(),
                                    ],
                                },
                            );
                        }
                        break;
                    }
                    _ => {
                        handled = false;
                        break;
                    }
                }
            }
            if handled {
                i += if consumed_next { 2 } else { 1 };
                continue;
            }
        }

        // Sticked `-S<keyid>` form (`git commit -S~/.ssh/id_ed25519 -m "..."`)
        if let Some(keyid) = arg.strip_prefix("-S")
            && !arg.starts_with("--")
            && !keyid.is_empty()
        {
            explicit_sign = Some(true);
            explicit_signing_key = Some(keyid.to_string());
            i += 1;
            continue;
        }

        if arg.starts_with('-') {
            return emit_error(
                mode,
                &OzError::InvalidUsage {
                    message: format!("unsupported flag `{arg}` for `oz git commit`"),
                    recovery: vec![
                        "Use `oz git commit [-S] [--amend [--no-edit]] -m \"<message>\" [-a] [--allow-empty] [--force]`."
                            .to_string(),
                    ],
                },
            );
        }

        pathspecs.push(arg.clone());
        i += 1;
    }

    if let Some(ref fpath) = msg_file {
        match read_git_message_file(parsed, fpath) {
            Ok(Some(trimmed)) => messages.push(trimmed),
            Ok(None) => {}
            Err(err) => return emit_error(mode, &err),
        }
    }

    let full_message = if amend {
        if messages.is_empty() {
            if !no_edit {
                let err = OzError::InvalidUsage {
                    message: "`oz git commit --amend` requires `-m <msg>`, `-F <file>`, or `--no-edit` (never spawns $EDITOR)"
                        .to_string(),
                    recovery: vec!["Pass `--amend --no-edit` or `--amend -m \"...\"`.".to_string()],
                };
                return emit_error(mode, &err);
            }
            String::new()
        } else {
            let joined = messages.join("\n\n");
            if joined.trim().is_empty() {
                let err = OzError::InvalidUsage {
                    message: "commit message cannot be empty".to_string(),
                    recovery: vec!["oz git commit --amend -m \"feat: ...\"".to_string()],
                };
                return emit_error(mode, &err);
            }
            joined
        }
    } else {
        if messages.is_empty() {
            let err = OzError::InvalidUsage {
                message: "missing commit message; pass `-m <message>` or `-F <file>` (never spawns $EDITOR in non-interactive mode)"
                    .to_string(),
                recovery: vec!["oz git commit -m \"feat: ...\"".to_string()],
            };
            return emit_error(mode, &err);
        }
        let joined = messages.join("\n\n");
        if joined.trim().is_empty() {
            let err = OzError::InvalidUsage {
                message: "commit message cannot be empty".to_string(),
                recovery: vec!["oz git commit -m \"feat: ...\"".to_string()],
            };
            return emit_error(mode, &err);
        }
        joined
    };

    let sign = explicit_sign.or_else(|| match config_gpgsign {
        Some(false) => Some(false),
        Some(true) => {
            if config_gpg_format
                .as_deref()
                .is_none_or(|f| f.trim().eq_ignore_ascii_case("ssh"))
            {
                Some(true)
            } else {
                None
            }
        }
        None => None,
    });
    let signing_key = explicit_signing_key.or(config_signing_key);

    let inv_dir = invocation_dir(parsed);
    let res = (|| -> Result<Envelope<GitCommitData>> {
        let repo = discover_and_recover_repo(parsed.effective_dir.as_deref(), false)?;
        let pre_status = compute_status(&repo)?;
        let actor = actor_from_mode(mode, &repo);
        let cmd_label = if amend {
            "oz git commit --amend"
        } else {
            "oz git commit"
        };
        let tx = MutationTransaction::begin(&repo, cmd_label, actor, Some(&pre_status.changes))?;

        let pick: Vec<String> = pathspecs
            .iter()
            .map(|p| resolve_pathspec_to_repo_rel(&repo, &inv_dir, p))
            .collect();

        let opts = CommitOptions {
            message: full_message,
            pick,
            exclude: Vec::new(),
            force,
            allow_out_of_scope,
            allow_empty,
            author_name,
            author_email,
            sign,
            signing_key,
            amend,
            no_verify,
            config_overrides: parsed.config_overrides.clone(),
        };
        let outcome = if all_flag {
            commit_changes_ignoring_pinned(&repo, &opts)?
        } else {
            commit_changes(&repo, &opts)?
        };

        let entry = if outcome.remaining_dirty_count == 0 {
            tx.finish_with_message(&repo, Some(&outcome.message), Some(&[]))?
        } else {
            let post_status = compute_status(&repo)?;
            tx.finish_with_message(&repo, Some(&outcome.message), Some(&post_status.changes))?
        };

        let prov_opts = ProvenanceOptions {
            force_agent: mode.agent,
            inspect_env: true,
            ..Default::default()
        };
        let provenance = record_commit_provenance(&repo, &outcome.commit_oid, &prov_opts)?;

        let branch = outcome.branch.clone();
        let head_oid = Some(outcome.commit_oid.clone());
        let op_id = entry.op_id;
        Ok(Envelope::success(GitCommitData {
            outcome,
            provenance,
        })
        .with_op_id(op_id.clone())
        .with_workspace(repo.workspace)
        .with_branch(branch)
        .with_head_oid(head_oid)
        .with_recovery_command(format!("oz undo {op_id}")))
    })();

    emit_json_or_human(mode, res, |env| {
        if quiet {
            return;
        }
        let d = &env.data.outcome;
        let short = &d.commit_oid[..d.commit_oid.len().min(12)];
        let amend_tag = if d.amended { " (amended)" } else { "" };
        let sign_tag = if d.signed { ", signed" } else { "" };
        println!(
            "[{} {short}]{amend_tag} {} file(s) committed ({} dirty remaining, Change-Id: {}{sign_tag})",
            d.branch.as_deref().unwrap_or("detached"),
            d.committed_paths.len(),
            d.remaining_dirty_count,
            d.change_id
        );
    })
}

fn parse_git_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn parse_author_spec(spec: &str) -> Option<(String, String)> {
    let lt = spec.rfind('<')?;
    let gt = spec.rfind('>')?;
    if gt <= lt {
        return None;
    }
    let name = spec[..lt].trim().to_string();
    let email = spec[lt + 1..gt].trim().to_string();
    if name.is_empty() || email.is_empty() {
        return None;
    }
    Some((name, email))
}

// ============================================================================
// Native: `oz git switch` & `oz git checkout`
// ============================================================================

fn handle_git_switch(parsed: &ParsedGitGlobals, allow_detached_commit: bool) -> ExitCode {
    let (args, mode) = extract_json_agent_flags(&parsed.subcommand_args, parsed.mode);

    let mut create_branch: Option<String> = None;
    let mut reset_existing = false;
    let mut detach = false;
    let mut force = false;
    let mut positionals = Vec::new();
    let mut i = 0usize;
    let mut after_sep = false;

    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            after_sep = true;
            i += 1;
            continue;
        }
        if !after_sep {
            if arg == "-c" || arg == "-C" || arg == "-b" || arg == "-B" {
                let Some(val) = args.get(i + 1) else {
                    return emit_error(
                        mode,
                        &OzError::InvalidUsage {
                            message: format!("option `{arg}` requires a branch name"),
                            recovery: vec!["Pass `oz git switch -c <branch> [<start>]`.".to_string()],
                        },
                    );
                };
                create_branch = Some(val.clone());
                reset_existing = arg == "-C" || arg == "-B";
                i += 2;
                continue;
            }
            if arg == "-d" || arg == "--detach" {
                detach = true;
                i += 1;
                continue;
            }
            if arg == "-f" || arg == "--force" || arg == "--discard-changes" {
                force = true;
                i += 1;
                continue;
            }
            if arg == "-q" || arg == "--quiet" {
                i += 1;
                continue;
            }
            if arg.starts_with('-') {
                return emit_error(
                    mode,
                    &OzError::InvalidUsage {
                        message: format!("unsupported flag `{arg}` for `oz git switch`"),
                        recovery: vec![
                            "Use `oz git switch [-c|-C <branch>] <target> [-f]`.".to_string(),
                        ],
                    },
                );
            }
        }
        positionals.push(arg.clone());
        i += 1;
    }

    let target = positionals.first().cloned();
    let res = (|| -> Result<Envelope<oz_core::SwitchOutcome>> {
        let repo = discover_and_recover_repo(parsed.effective_dir.as_deref(), false)?;
        let pre_status = compute_status(&repo)?;
        let actor = actor_from_mode(mode, &repo);
        let cmd_str = format_command_string("switch", &parsed.subcommand_args);
        let tx = MutationTransaction::begin_watching_all_branches(
            &repo,
            cmd_str,
            actor,
            Some(&pre_status.changes),
        )?;

        let outcome = switch_branch(
            &repo,
            &SwitchOptions {
                target,
                create_branch,
                reset_existing_branch: reset_existing,
                detach,
                allow_detached_commit,
                force,
            },
        )?;

        let post_repo = OzoneRepo::discover(parsed.effective_dir.as_deref())
            .unwrap_or_else(|_| repo.clone());
        let entry = tx.finish(&post_repo, None)?;

        Ok(Envelope::success(outcome.clone())
            .with_op_id(entry.op_id.clone())
            .with_workspace(post_repo.workspace)
            .with_branch(outcome.branch)
            .with_head_oid(outcome.head_oid)
            .with_recovery_command(format!("oz undo {}", entry.op_id)))
    })();

    emit_json_or_human(mode, res, |env| {
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
    })
}

fn handle_git_checkout(parsed: &ParsedGitGlobals) -> ExitCode {
    let (args, mode) = extract_json_agent_flags(&parsed.subcommand_args, parsed.mode);

    // Delegate interactive/conflict-stage checkout forms (`-p`, `--ours`, `--theirs`).
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "-p" | "--patch" | "--ours" | "--theirs"))
    {
        if args.iter().any(|a| a == "-p" || a == "--patch") && mode.non_interactive {
            return emit_error(
                mode,
                &OzError::InvalidUsage {
                    message: "`git checkout -p` is interactive and disabled in non-interactive mode"
                        .to_string(),
                    recovery: vec!["Pass `oz git checkout -- <file>`.".to_string()],
                },
            );
        }
        return delegate_mutating(parsed, "checkout", &parsed.subcommand_args, false, false);
    }

    let has_create_or_detach = args
        .iter()
        .any(|a| matches!(a.as_str(), "-b" | "-B" | "-d" | "--detach"));
    let sep_idx = args.iter().position(|a| a == "--");

    if let Some(idx) = sep_idx
        && !has_create_or_detach
    {
        // `oz git checkout [<rev>] -- <paths...>`
        let before = &args[..idx];
        let after = &args[idx + 1..];
        let mut rev: Option<String> = None;
        for tok in before {
            if tok == "-f" || tok == "--force" || tok == "-q" || tok == "--quiet" {
                continue;
            }
            if !tok.starts_with('-') && rev.is_none() {
                rev = Some(tok.clone());
            } else {
                return delegate_mutating(
                    parsed,
                    "checkout",
                    &parsed.subcommand_args,
                    false,
                    false,
                );
            }
        }
        return run_native_checkout_paths(parsed, mode, rev, after);
    }

    if !has_create_or_detach {
        // Check whether positional args look like paths rather than a branch/commit.
        let positionals: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
        if positionals.len() == 1 {
            let candidate = positionals[0];
            if let Ok(repo) = discover_and_recover_repo(parsed.effective_dir.as_deref(), false)
                && !is_local_branch(&repo, candidate).unwrap_or(false)
                && !resolves_as_revision(&repo, candidate)
            {
                return run_native_checkout_paths(
                    parsed,
                    mode,
                    None,
                    std::slice::from_ref(candidate),
                );
            }
        } else if positionals.len() > 1 {
            let first = positionals[0];
            if let Ok(repo) = discover_and_recover_repo(parsed.effective_dir.as_deref(), false) {
                if !is_local_branch(&repo, first).unwrap_or(false)
                    && resolves_as_revision(&repo, first)
                {
                    let rest: Vec<String> = positionals[1..].iter().map(|s| (*s).clone()).collect();
                    return run_native_checkout_paths(
                        parsed,
                        mode,
                        Some(first.clone()),
                        &rest,
                    );
                } else if !resolves_as_revision(&repo, first) {
                    let all_paths: Vec<String> = positionals.iter().map(|s| (*s).clone()).collect();
                    return run_native_checkout_paths(parsed, mode, None, &all_paths);
                }
            }
        }
    }

    handle_git_switch(parsed, true)
}

fn run_native_checkout_paths(
    parsed: &ParsedGitGlobals,
    mode: InteractionMode,
    revision: Option<String>,
    raw_paths: &[String],
) -> ExitCode {
    let inv_dir = invocation_dir(parsed);
    let res = (|| -> Result<Envelope<oz_core::CheckoutPathsOutcome>> {
        let repo = discover_and_recover_repo(parsed.effective_dir.as_deref(), false)?;
        let pre_status = compute_status(&repo)?;
        let actor = actor_from_mode(mode, &repo);
        let cmd_str = format_command_string("checkout", &parsed.subcommand_args);
        let tx = MutationTransaction::begin(&repo, cmd_str, actor, Some(&pre_status.changes))?;

        let paths: Vec<String> = raw_paths
            .iter()
            .map(|p| resolve_pathspec_to_repo_rel(&repo, &inv_dir, p))
            .collect();
        let outcome = checkout_paths(&repo, &CheckoutPathsOptions { revision, paths })?;
        let entry = tx.finish(&repo, None)?;

        Ok(Envelope::success(outcome)
            .with_op_id(entry.op_id.clone())
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid)
            .with_recovery_command(format!("oz undo {}", entry.op_id)))
    })();

    emit_json_or_human(mode, res, |env| {
        println!(
            "Updated {} path(s) from {}",
            env.data.restored_paths.len(),
            env.data.revision
        );
    })
}

// ============================================================================
// Native In-Memory Integration & Stacked Rebase: `merge` & `rebase`
// ============================================================================

fn read_git_message_file(parsed: &ParsedGitGlobals, fpath: &str) -> Result<Option<String>> {
    let content = if fpath == "-" {
        let mut buf = String::new();
        use std::io::Read as _;
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        let resolved = if Path::new(fpath).is_absolute() {
            PathBuf::from(fpath)
        } else {
            invocation_dir(parsed).join(fpath)
        };
        std::fs::read_to_string(&resolved)?
    };
    let trimmed = content.trim_end_matches(['\r', '\n']);
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

enum GitMergeAction {
    Continue { verify_cmd: Option<String> },
    Abort,
    Integrate {
        sources: Vec<String>,
        message: Option<String>,
        verify_cmd: Option<String>,
        allow_out_of_scope: bool,
        force: bool,
    },
    Fallback,
}

fn parse_git_merge_action(
    parsed: &ParsedGitGlobals,
    args: &[String],
) -> Result<GitMergeAction> {
    let mut is_continue = false;
    let mut is_abort = false;
    let mut messages = Vec::new();
    let mut msg_file: Option<String> = None;
    let mut verify_cmd: Option<String> = None;
    let mut allow_out_of_scope = false;
    let mut force = false;
    let mut sources = Vec::new();
    let mut after_sep = false;
    let mut i = 0usize;

    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            after_sep = true;
            i += 1;
            continue;
        }
        if !after_sep && arg.starts_with('-') {
            match arg.as_str() {
                "--continue" => {
                    is_continue = true;
                    i += 1;
                }
                "--abort" => {
                    is_abort = true;
                    i += 1;
                }
                "-m" | "--message" => {
                    let Some(val) = args.get(i + 1) else {
                        return Err(OzError::InvalidUsage {
                            message: format!("option `{arg}` requires a merge commit message"),
                            recovery: vec!["oz git merge <branch> -m \"...\"".to_string()],
                        });
                    };
                    messages.push(val.clone());
                    i += 2;
                }
                "-F" | "--file" => {
                    let Some(val) = args.get(i + 1) else {
                        return Err(OzError::InvalidUsage {
                            message: format!("option `{arg}` requires a file path"),
                            recovery: vec!["oz git merge <branch> -F <file>".to_string()],
                        });
                    };
                    msg_file = Some(val.clone());
                    i += 2;
                }
                "--verify" => {
                    let Some(val) = args.get(i + 1) else {
                        return Err(OzError::InvalidUsage {
                            message: "option `--verify` requires a command string".to_string(),
                            recovery: vec!["oz git merge <branch> --verify \"cargo test\"".to_string()],
                        });
                    };
                    verify_cmd = Some(val.clone());
                    i += 2;
                }
                "--allow-out-of-scope" => {
                    allow_out_of_scope = true;
                    i += 1;
                }
                "--force" => {
                    force = true;
                    i += 1;
                }
                "--no-ff" | "--ff" | "--no-edit" | "--edit" | "-e" | "-q" | "--quiet"
                | "-v" | "--verbose" | "--stat" | "--no-stat" | "--progress" | "--no-progress"
                | "--no-verify" => {
                    i += 1;
                }
                _ => {
                    if let Some(val) = arg.strip_prefix("--message=") {
                        messages.push(val.to_string());
                        i += 1;
                    } else if let Some(val) = arg.strip_prefix("-m")
                        && !arg.starts_with("--")
                    {
                        messages.push(val.to_string());
                        i += 1;
                    } else if let Some(val) = arg.strip_prefix("--file=") {
                        msg_file = Some(val.to_string());
                        i += 1;
                    } else if let Some(val) = arg.strip_prefix("-F")
                        && !arg.starts_with("--")
                    {
                        msg_file = Some(val.to_string());
                        i += 1;
                    } else if let Some(val) = arg.strip_prefix("--verify=") {
                        verify_cmd = Some(val.to_string());
                        i += 1;
                    } else {
                        return Ok(GitMergeAction::Fallback);
                    }
                }
            }
            continue;
        }
        sources.push(arg.clone());
        i += 1;
    }

    if is_continue && is_abort {
        return Err(OzError::InvalidUsage {
            message: "cannot pass both `--continue` and `--abort` to `oz git merge`".to_string(),
            recovery: vec![
                "oz git merge --continue".to_string(),
                "oz git merge --abort".to_string(),
            ],
        });
    }
    if is_continue {
        if !sources.is_empty() {
            return Ok(GitMergeAction::Fallback);
        }
        return Ok(GitMergeAction::Continue { verify_cmd });
    }
    if is_abort {
        if !sources.is_empty() {
            return Ok(GitMergeAction::Fallback);
        }
        return Ok(GitMergeAction::Abort);
    }
    if sources.is_empty() {
        return Ok(GitMergeAction::Fallback);
    }

    if let Some(ref fpath) = msg_file
        && let Some(trimmed) = read_git_message_file(parsed, fpath)?
    {
        messages.push(trimmed);
    }
    let message = (!messages.is_empty()).then(|| messages.join("\n\n"));

    Ok(GitMergeAction::Integrate {
        sources,
        message,
        verify_cmd,
        allow_out_of_scope,
        force,
    })
}

fn can_resolve_all_merge_sources_natively(repo: &OzoneRepo, sources: &[String]) -> Result<bool> {
    let summaries = list_workspaces(repo)?;
    for src in sources {
        let short = src.strip_prefix("refs/heads/").unwrap_or(src);
        let matches_ws = summaries
            .iter()
            .any(|s| s.state.name == *src || s.state.branch == short);
        if !matches_ws && !is_local_branch(repo, short)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Handles `oz git merge ...` natively via `oz_integrate::integrate`, falling back to
/// OpLog-wrapped system `git merge` only when unsupported flags or non-local refs are passed.
pub fn handle_git_merge(parsed: &ParsedGitGlobals) -> ExitCode {
    let (args, mode) = extract_json_agent_flags(&parsed.subcommand_args, parsed.mode);
    let action = match parse_git_merge_action(parsed, &args) {
        Ok(a) => a,
        Err(err) => return emit_error(mode, &err),
    };

    match action {
        GitMergeAction::Fallback => {
            delegate_mutating(parsed, "merge", &parsed.subcommand_args, false, true)
        }
        GitMergeAction::Continue { verify_cmd } => {
            let repo = match discover_and_recover_repo(parsed.effective_dir.as_deref(), true) {
                Ok(r) => r,
                Err(err) => return emit_error(mode, &err),
            };
            let session = match active_session(&repo) {
                Ok(s) => s,
                Err(err) => return emit_error(mode, &err),
            };
            if session
                .as_ref()
                .is_some_and(|s| s.mode == IntegrationMode::Integrate)
            {
                let actor = actor_from_mode(mode, &repo);
                let res = continue_integration_with_verify(&repo, verify_cmd.as_deref(), actor)
                    .map(|out| wrap_integrate_envelope(out, repo.workspace.clone()));
                return emit_json_or_human(mode, res, |env| print_integrate_human(&env.data));
            }
            if repo.git_dir.join("MERGE_HEAD").exists() {
                return delegate_mutating(parsed, "merge", &parsed.subcommand_args, false, true);
            }
            emit_error(
                mode,
                &OzError::InvalidUsage {
                    message: "no merge in progress to continue".to_string(),
                    recovery: vec!["oz git merge <branch>".to_string()],
                },
            )
        }
        GitMergeAction::Abort => {
            let repo = match discover_and_recover_repo(parsed.effective_dir.as_deref(), false) {
                Ok(r) => r,
                Err(err) => return emit_error(mode, &err),
            };
            let session = match active_session(&repo) {
                Ok(s) => s,
                Err(err) => return emit_error(mode, &err),
            };
            if session
                .as_ref()
                .is_some_and(|s| s.mode == IntegrationMode::Integrate)
            {
                let res = abort_integration(&repo).map(|out| {
                    Envelope::success(out)
                        .with_workspace(repo.workspace.clone())
                        .with_branch(repo.branch.clone())
                        .with_head_oid(repo.head_oid.clone())
                });
                return emit_json_or_human(mode, res, |env| print_abort_human(&env.data));
            }
            if repo.git_dir.join("MERGE_HEAD").exists() {
                return delegate_mutating(parsed, "merge", &parsed.subcommand_args, false, true);
            }
            emit_error(
                mode,
                &OzError::InvalidUsage {
                    message: "no merge in progress to abort".to_string(),
                    recovery: vec!["oz git status".to_string()],
                },
            )
        }
        GitMergeAction::Integrate {
            sources,
            message,
            verify_cmd,
            allow_out_of_scope,
            force,
        } => {
            let repo = match discover_and_recover_repo(parsed.effective_dir.as_deref(), true) {
                Ok(r) => r,
                Err(err) => return emit_error(mode, &err),
            };
            if repo.branch.is_none()
                || repo.git_dir.join("MERGE_HEAD").exists()
                || !can_resolve_all_merge_sources_natively(&repo, &sources).unwrap_or(false)
            {
                return delegate_mutating(parsed, "merge", &parsed.subcommand_args, false, true);
            }

            let actor = actor_from_mode(mode, &repo);
            let opts = IntegrateOptions {
                sources,
                into: repo.branch.clone(),
                verify_cmd,
                allow_out_of_scope,
                force,
                message,
            };
            let res = integrate(&repo, &opts, actor)
                .map(|out| wrap_integrate_envelope(out, repo.workspace.clone()));
            emit_json_or_human(mode, res, |env| print_integrate_human(&env.data))
        }
    }
}

enum GitRebaseAction {
    Continue,
    Abort,
    StackRebase {
        onto: String,
        update_refs: bool,
    },
    Fallback,
}

fn parse_git_rebase_action(args: &[String], mode: InteractionMode) -> Result<GitRebaseAction> {
    let mut is_continue = false;
    let mut is_abort = false;
    let mut explicit_onto: Option<String> = None;
    let mut update_refs = false;
    let mut positionals = Vec::new();
    let mut after_sep = false;
    let mut i = 0usize;

    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            after_sep = true;
            i += 1;
            continue;
        }
        if !after_sep && arg.starts_with('-') {
            match arg.as_str() {
                "--continue" => {
                    is_continue = true;
                    i += 1;
                }
                "--abort" => {
                    is_abort = true;
                    i += 1;
                }
                "-i" | "--interactive" => {
                    if mode.non_interactive {
                        return Err(OzError::InvalidUsage {
                            message: "`git rebase -i` is interactive and disabled in non-interactive mode"
                                .to_string(),
                            recovery: vec!["Run `oz git rebase <branch>` or `oz stack rebase`.".to_string()],
                        });
                    }
                    return Ok(GitRebaseAction::Fallback);
                }
                "--onto" => {
                    let Some(val) = args.get(i + 1) else {
                        return Err(OzError::InvalidUsage {
                            message: "option `--onto` requires a base branch".to_string(),
                            recovery: vec!["oz git rebase --onto <branch>".to_string()],
                        });
                    };
                    explicit_onto = Some(val.clone());
                    i += 2;
                }
                "--update-refs" => {
                    update_refs = true;
                    i += 1;
                }
                "--no-update-refs" | "-q" | "--quiet" | "-v" | "--verbose" | "--stat" | "-n"
                | "--no-stat" | "--no-verify" => {
                    i += 1;
                }
                _ => {
                    if let Some(val) = arg.strip_prefix("--onto=") {
                        explicit_onto = Some(val.to_string());
                        i += 1;
                    } else {
                        return Ok(GitRebaseAction::Fallback);
                    }
                }
            }
            continue;
        }
        positionals.push(arg.clone());
        i += 1;
    }

    if is_continue && is_abort {
        return Err(OzError::InvalidUsage {
            message: "cannot pass both `--continue` and `--abort` to `oz git rebase`".to_string(),
            recovery: vec![
                "oz git rebase --continue".to_string(),
                "oz git rebase --abort".to_string(),
            ],
        });
    }
    if is_continue {
        if !positionals.is_empty() {
            return Ok(GitRebaseAction::Fallback);
        }
        return Ok(GitRebaseAction::Continue);
    }
    if is_abort {
        if !positionals.is_empty() {
            return Ok(GitRebaseAction::Fallback);
        }
        return Ok(GitRebaseAction::Abort);
    }

    let onto = match (explicit_onto, positionals.as_slice()) {
        (Some(base), []) => base,
        (None, [upstream]) => upstream.clone(),
        _ => return Ok(GitRebaseAction::Fallback),
    };

    Ok(GitRebaseAction::StackRebase { onto, update_refs })
}

/// Handles `oz git rebase ...` natively via `oz_integrate::stack_rebase`, falling back to
/// OpLog-wrapped system `git rebase` only when unsupported flags or remote/range forms apply.
pub fn handle_git_rebase(parsed: &ParsedGitGlobals) -> ExitCode {
    let (args, mode) = extract_json_agent_flags(&parsed.subcommand_args, parsed.mode);
    let action = match parse_git_rebase_action(&args, mode) {
        Ok(a) => a,
        Err(err) => return emit_error(mode, &err),
    };

    match action {
        GitRebaseAction::Fallback => {
            delegate_mutating(parsed, "rebase", &parsed.subcommand_args, false, true)
        }
        GitRebaseAction::Continue => {
            let repo = match discover_and_recover_repo(parsed.effective_dir.as_deref(), true) {
                Ok(r) => r,
                Err(err) => return emit_error(mode, &err),
            };
            let session = match active_session(&repo) {
                Ok(s) => s,
                Err(err) => return emit_error(mode, &err),
            };
            if session
                .as_ref()
                .is_some_and(|s| s.mode == IntegrationMode::StackRebase)
            {
                let actor = actor_from_mode(mode, &repo);
                let res = continue_stack_rebase(&repo, actor).map(|out| {
                    let post_repo = OzoneRepo::discover(parsed.effective_dir.as_deref())
                        .unwrap_or_else(|_| repo.clone());
                    wrap_stack_rebase_envelope(out, &post_repo)
                });
                return emit_json_or_human(mode, res, |env| print_stack_rebase_human(&env.data));
            }
            if repo.git_dir.join("rebase-merge").exists()
                || repo.git_dir.join("rebase-apply").exists()
            {
                return delegate_mutating(parsed, "rebase", &parsed.subcommand_args, false, true);
            }
            emit_error(
                mode,
                &OzError::InvalidUsage {
                    message: "no rebase in progress to continue".to_string(),
                    recovery: vec!["oz git rebase <branch>".to_string()],
                },
            )
        }
        GitRebaseAction::Abort => {
            let repo = match discover_and_recover_repo(parsed.effective_dir.as_deref(), false) {
                Ok(r) => r,
                Err(err) => return emit_error(mode, &err),
            };
            let session = match active_session(&repo) {
                Ok(s) => s,
                Err(err) => return emit_error(mode, &err),
            };
            if session
                .as_ref()
                .is_some_and(|s| s.mode == IntegrationMode::StackRebase)
            {
                let res = abort_integration(&repo).map(|out| {
                    Envelope::success(out)
                        .with_workspace(repo.workspace.clone())
                        .with_branch(repo.branch.clone())
                        .with_head_oid(repo.head_oid.clone())
                });
                return emit_json_or_human(mode, res, |env| print_abort_human(&env.data));
            }
            if repo.git_dir.join("rebase-merge").exists()
                || repo.git_dir.join("rebase-apply").exists()
            {
                return delegate_mutating(parsed, "rebase", &parsed.subcommand_args, false, true);
            }
            emit_error(
                mode,
                &OzError::InvalidUsage {
                    message: "no rebase in progress to abort".to_string(),
                    recovery: vec!["oz git status".to_string()],
                },
            )
        }
        GitRebaseAction::StackRebase { onto, update_refs } => {
            let repo = match discover_and_recover_repo(parsed.effective_dir.as_deref(), true) {
                Ok(r) => r,
                Err(err) => return emit_error(mode, &err),
            };
            let onto_short = onto.strip_prefix("refs/heads/").unwrap_or(&onto);
            let Some(ref cur_branch) = repo.branch else {
                return delegate_mutating(parsed, "rebase", &parsed.subcommand_args, false, true);
            };
            if repo.git_dir.join("rebase-merge").exists()
                || repo.git_dir.join("rebase-apply").exists()
                || !is_local_branch(&repo, onto_short).unwrap_or(false)
            {
                return delegate_mutating(parsed, "rebase", &parsed.subcommand_args, false, true);
            }
            if cur_branch == onto_short {
                let noop = StackRebaseOutcome {
                    onto_branch: onto_short.to_string(),
                    onto_oid: repo.head_oid.clone().unwrap_or_default(),
                    branches: Vec::new(),
                    rebased_commits: Vec::new(),
                    op_id: None,
                    updated_workspaces: Vec::new(),
                };
                let env = wrap_stack_rebase_envelope(noop, &repo);
                return emit_json_or_human(mode, Ok(env), |_| {
                    println!("Current branch {cur_branch} is up to date.");
                });
            }

            let actor = actor_from_mode(mode, &repo);
            let opts = StackRebaseOptions {
                branches: if update_refs {
                    Vec::new()
                } else {
                    vec![cur_branch.clone()]
                },
                onto: Some(onto_short.to_string()),
            };
            let res = stack_rebase(&repo, &opts, actor).map(|out| {
                let post_repo = OzoneRepo::discover(parsed.effective_dir.as_deref())
                    .unwrap_or_else(|_| repo.clone());
                wrap_stack_rebase_envelope(out, &post_repo)
            });
            emit_json_or_human(mode, res, |env| print_stack_rebase_human(&env.data))
        }
    }
}

// ============================================================================
// Native Wrappers: `clone`, `fetch`, `push`
// ============================================================================

fn has_non_config_exotic_globals(parsed: &ParsedGitGlobals) -> bool {
    parsed.delegated_globals.iter().any(|g| {
        let s = g.to_string_lossy();
        !s.starts_with("-c")
            && !matches!(
                s.as_ref(),
                "--no-pager" | "-p" | "--paginate" | "--no-optional-locks"
            )
    })
}

fn has_exotic_git_globals(parsed: &ParsedGitGlobals) -> bool {
    if !parsed.config_overrides.is_empty() {
        return true;
    }
    has_non_config_exotic_globals(parsed)
}

enum GitCloneAction {
    Native {
        url: String,
        explicit_dest: Option<PathBuf>,
        branch: Option<String>,
        filter: Option<String>,
        remote_name: Option<String>,
        checkout: bool,
        quiet: bool,
    },
    Fallback,
}

fn parse_git_clone_action(args: &[String], has_exotic_globals: bool) -> Result<GitCloneAction> {
    if has_exotic_globals {
        return Ok(GitCloneAction::Fallback);
    }

    let mut branch: Option<String> = None;
    let mut filter: Option<String> = None;
    let mut remote_name: Option<String> = None;
    let mut checkout = true;
    let mut quiet = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut after_sep = false;
    let mut i = 0usize;

    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            after_sep = true;
            i += 1;
            continue;
        }
        if !after_sep && arg.starts_with('-') {
            match arg.as_str() {
                "-b" | "--branch" => {
                    let Some(val) = args.get(i + 1) else {
                        return Err(OzError::InvalidUsage {
                            message: format!("option `{arg}` requires a branch or tag name"),
                            recovery: vec!["oz git clone -b <branch> <url> [<dir>]".to_string()],
                        });
                    };
                    branch = Some(val.clone());
                    i += 2;
                    continue;
                }
                "--filter" => {
                    let Some(val) = args.get(i + 1) else {
                        return Err(OzError::InvalidUsage {
                            message: "option `--filter` requires an object filter specification"
                                .to_string(),
                            recovery: vec![
                                "oz git clone --filter=blob:none <url> [<dir>]".to_string(),
                            ],
                        });
                    };
                    filter = Some(val.clone());
                    i += 2;
                    continue;
                }
                "-o" | "--origin" => {
                    let Some(val) = args.get(i + 1) else {
                        return Err(OzError::InvalidUsage {
                            message: format!("option `{arg}` requires a remote name"),
                            recovery: vec!["oz git clone -o origin <url> [<dir>]".to_string()],
                        });
                    };
                    if val.trim() != "origin" {
                        return Ok(GitCloneAction::Fallback);
                    }
                    remote_name = Some("origin".to_string());
                    i += 2;
                    continue;
                }
                "-q" | "--quiet" => {
                    quiet = true;
                    i += 1;
                    continue;
                }
                "-n" | "--no-checkout" => {
                    checkout = false;
                    i += 1;
                    continue;
                }
                "-v" | "--verbose" | "--progress" | "--no-progress" => {
                    i += 1;
                    continue;
                }
                _ => {
                    if let Some(val) = arg.strip_prefix("--branch=") {
                        branch = Some(val.to_string());
                        i += 1;
                        continue;
                    }
                    if let Some(val) = arg.strip_prefix("-b")
                        && !arg.starts_with("--")
                        && !val.is_empty()
                    {
                        branch = Some(val.to_string());
                        i += 1;
                        continue;
                    }
                    if let Some(val) = arg.strip_prefix("--filter=") {
                        filter = Some(val.to_string());
                        i += 1;
                        continue;
                    }
                    if let Some(val) = arg.strip_prefix("--origin=") {
                        if val.trim() != "origin" {
                            return Ok(GitCloneAction::Fallback);
                        }
                        remote_name = Some("origin".to_string());
                        i += 1;
                        continue;
                    }
                    if let Some(val) = arg.strip_prefix("-o")
                        && !arg.starts_with("--")
                        && !val.is_empty()
                    {
                        if val.trim() != "origin" {
                            return Ok(GitCloneAction::Fallback);
                        }
                        remote_name = Some("origin".to_string());
                        i += 1;
                        continue;
                    }
                    return Ok(GitCloneAction::Fallback);
                }
            }
        }
        positionals.push(arg.clone());
        i += 1;
    }

    if positionals.is_empty() || positionals.len() > 2 {
        return Ok(GitCloneAction::Fallback);
    }

    let url = positionals[0].clone();
    let explicit_dest = positionals.get(1).map(PathBuf::from);
    Ok(GitCloneAction::Native {
        url,
        explicit_dest,
        branch,
        filter,
        remote_name,
        checkout,
        quiet,
    })
}

fn resolve_clone_url_for_inv_dir(raw_url: &str, inv_dir: &Path) -> String {
    let trimmed = raw_url.trim();
    if let Ok(parsed) = parse_git_remote(trimmed, None)
        && parsed.scheme == RemoteScheme::File
        && !trimmed.starts_with("file://")
    {
        let p = Path::new(trimmed);
        if !p.is_absolute() {
            return normalize_lexical(&inv_dir.join(p))
                .to_string_lossy()
                .into_owned();
        }
    }
    raw_url.to_string()
}

fn handle_git_clone(parsed: &ParsedGitGlobals) -> ExitCode {
    let (clean_args, mut mode) = extract_json_agent_flags(&parsed.subcommand_args, parsed.mode);
    mode.non_interactive = true;
    let os_args: Vec<OsString> = clean_args.iter().map(OsString::from).collect();

    let action = match parse_git_clone_action(&clean_args, has_exotic_git_globals(parsed)) {
        Ok(a) => a,
        Err(err) => return emit_error(mode, &err),
    };

    if let GitCloneAction::Native {
        url,
        explicit_dest,
        branch,
        filter,
        remote_name,
        checkout,
        quiet,
    } = action
    {
        let base_dir = invocation_dir(parsed);
        let dest_rel_or_abs = explicit_dest.unwrap_or_else(|| derive_clone_dir_name(&url, false));
        let dest_dir = if dest_rel_or_abs.is_absolute() {
            normalize_lexical(&dest_rel_or_abs)
        } else {
            normalize_lexical(&base_dir.join(&dest_rel_or_abs))
        };
        let effective_url = resolve_clone_url_for_inv_dir(&url, &base_dir);
        let actor = standalone_actor_from_mode(mode);

        let res = (|| -> Result<Envelope<GitCloneData>> {
            let outcome = clone_repo(
                &CloneOptions {
                    url: effective_url,
                    destination: dest_dir,
                    branch,
                    filter,
                    bare: false,
                    remote_name,
                    checkout,
                    auth_token: None,
                    auth_scheme: HttpAuthScheme::BasicToken,
                },
                actor,
            )?;

            let ozone_dir = outcome.git_dir.join("ozone");
            let out_branch = outcome.branch.clone();
            let out_head = outcome.head_oid.clone();
            let out_op_id = outcome.op_id.clone();
            let warnings = outcome.warnings.clone();

            let mut env = Envelope::success(GitCloneData {
                url,
                path: outcome.destination,
                ozone_dir: Some(ozone_dir),
                branch: out_branch.clone(),
                head_oid: out_head.clone(),
                remote_name: Some(outcome.remote_name),
                protocol_version: Some(outcome.protocol_version),
                partial_clone: outcome.partial_clone,
                filter: outcome.filter,
                filter_negotiated: outcome.filter_negotiated,
                checked_out_files: Some(outcome.checked_out_files),
                op_id: out_op_id.clone(),
            })
            .with_branch(out_branch)
            .with_head_oid(out_head);

            if let Some(op_id) = out_op_id {
                env = env.with_op_id(op_id);
            }
            for w in warnings {
                env = env.with_warning(w);
            }
            Ok(env)
        })();

        return emit_json_or_human(mode, res, |env| {
            if !quiet {
                println!("Cloned '{}' into '{}'", env.data.url, env.data.path.display());
            }
        });
    }

    // Legacy fallback (`--bare`, `--mirror`, `--depth`, `--recurse-submodules`, custom `-c`, etc.)
    let Some((url, dest_rel_or_abs, is_bare)) = parse_clone_targets(&clean_args) else {
        return delegate_readonly(parsed, &["clone"], &os_args);
    };

    let base_dir = invocation_dir(parsed);
    let dest_dir = if dest_rel_or_abs.is_absolute() {
        normalize_lexical(&dest_rel_or_abs)
    } else {
        normalize_lexical(&base_dir.join(&dest_rel_or_abs))
    };

    let clone_parsed = ParsedGitGlobals {
        effective_dir: parsed.effective_dir.clone(),
        delegated_globals: parsed.delegated_globals.clone(),
        config_overrides: parsed.config_overrides.clone(),
        no_pager: parsed.no_pager,
        mode,
        subcommand: parsed.subcommand.clone(),
        subcommand_args: os_args.clone(),
    };
    let mut cmd = match build_real_git_command(&clone_parsed, Some("clone"), &os_args, false) {
        Ok(c) => c,
        Err(err) => return emit_error(mode, &err),
    };

    let out = if mode.json {
        cmd.output()
    } else {
        cmd.status().map(|status| std::process::Output {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    };

    let output = match out {
        Ok(o) => o,
        Err(err) => return emit_error(mode, &OzError::Io(err)),
    };

    if !output.status.success() {
        if mode.json {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let err = OzError::Git(if stderr.is_empty() {
                format!(
                    "git clone exited with status {}",
                    output.status.code().unwrap_or(2)
                )
            } else {
                stderr
            });
            return emit_error(mode, &err);
        }
        return raw_code_to_exit_code(output.status.code().unwrap_or(2));
    }

    let init_res = (|| -> Result<Envelope<GitCloneData>> {
        if is_bare {
            Ok(Envelope::success(GitCloneData {
                url,
                path: dest_dir,
                ozone_dir: None,
                branch: None,
                head_oid: None,
                remote_name: None,
                protocol_version: None,
                partial_clone: false,
                filter: None,
                filter_negotiated: false,
                checked_out_files: None,
                op_id: None,
            }))
        } else {
            let repo = OzoneRepo::discover(Some(&dest_dir))?;
            repo.layout.ensure_initialized()?;
            Ok(Envelope::success(GitCloneData {
                url,
                path: repo.repo_root,
                ozone_dir: Some(repo.layout.ozone_dir),
                branch: repo.branch.clone(),
                head_oid: repo.head_oid.clone(),
                remote_name: None,
                protocol_version: None,
                partial_clone: false,
                filter: None,
                filter_negotiated: false,
                checked_out_files: None,
                op_id: None,
            })
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
        }
    })();

    emit_json_or_human(mode, init_res, |_env| {})
}

fn parse_clone_targets(args: &[String]) -> Option<(String, PathBuf, bool)> {
    let mut is_bare = false;
    let mut positionals = Vec::new();
    let mut i = 0usize;
    let mut after_sep = false;

    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            after_sep = true;
            i += 1;
            continue;
        }
        if !after_sep && arg.starts_with('-') {
            if arg == "--bare" || arg == "--mirror" {
                is_bare = true;
                i += 1;
                continue;
            }
            if matches!(
                arg.as_str(),
                "-b" | "--branch"
                    | "-o"
                    | "--origin"
                    | "--depth"
                    | "--shallow-since"
                    | "--shallow-exclude"
                    | "--filter"
                    | "-j"
                    | "--jobs"
                    | "-c"
                    | "--config"
                    | "--reference"
                    | "--reference-if-able"
                    | "--separate-git-dir"
                    | "--template"
                    | "-u"
                    | "--upload-pack"
                    | "--server-option"
                    | "--bundle-uri"
            ) {
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        positionals.push(arg.clone());
        i += 1;
    }

    let url = positionals.first()?.clone();
    let dest = if let Some(explicit) = positionals.get(1) {
        PathBuf::from(explicit)
    } else {
        derive_clone_dir_name(&url, is_bare)
    };
    Some((url, dest, is_bare))
}

fn derive_clone_dir_name(url: &str, is_bare: bool) -> PathBuf {
    let trimmed = url.trim_end_matches(['/', '\\']);
    let last_seg = trimmed
        .rsplit(['/', '\\', ':'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("repo");
    let stripped = if is_bare {
        last_seg.to_string()
    } else {
        last_seg
            .strip_suffix(".git")
            .filter(|s| !s.is_empty())
            .unwrap_or(last_seg)
            .to_string()
    };
    PathBuf::from(if is_bare && !stripped.ends_with(".git") {
        format!("{stripped}.git")
    } else {
        stripped
    })
}

enum GitFetchAction {
    Native {
        remote: Option<String>,
        refspecs: Vec<String>,
        prune: bool,
        filter: Option<String>,
        quiet: bool,
    },
    Fallback,
}

fn parse_git_fetch_action(args: &[String], has_exotic_globals: bool) -> Result<GitFetchAction> {
    if has_exotic_globals {
        return Ok(GitFetchAction::Fallback);
    }

    let mut prune = false;
    let mut filter: Option<String> = None;
    let mut quiet = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut after_sep = false;
    let mut i = 0usize;

    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            after_sep = true;
            i += 1;
            continue;
        }
        if !after_sep && arg.starts_with('-') {
            match arg.as_str() {
                "-p" | "--prune" => {
                    prune = true;
                    i += 1;
                    continue;
                }
                "--no-prune" => {
                    prune = false;
                    i += 1;
                    continue;
                }
                "--filter" => {
                    let Some(val) = args.get(i + 1) else {
                        return Err(OzError::InvalidUsage {
                            message: "option `--filter` requires an object filter specification"
                                .to_string(),
                            recovery: vec!["oz git fetch --filter=blob:none [<remote>]".to_string()],
                        });
                    };
                    filter = Some(val.clone());
                    i += 2;
                    continue;
                }
                "-q" | "--quiet" => {
                    quiet = true;
                    i += 1;
                    continue;
                }
                "-v" | "--verbose" | "--progress" | "--no-progress" => {
                    i += 1;
                    continue;
                }
                _ => {
                    if let Some(val) = arg.strip_prefix("--filter=") {
                        filter = Some(val.to_string());
                        i += 1;
                        continue;
                    }
                    if let Some(short) = arg.strip_prefix('-')
                        && !short.starts_with('-')
                        && short.len() > 1
                        && short.chars().all(|c| matches!(c, 'p' | 'q' | 'v'))
                    {
                        for ch in short.chars() {
                            match ch {
                                'p' => prune = true,
                                'q' => quiet = true,
                                'v' => {}
                                _ => {}
                            }
                        }
                        i += 1;
                        continue;
                    }
                    return Ok(GitFetchAction::Fallback);
                }
            }
        }
        positionals.push(arg.clone());
        i += 1;
    }

    let remote = positionals.first().cloned();
    let refspecs = if positionals.len() > 1 {
        positionals[1..].to_vec()
    } else {
        Vec::new()
    };

    Ok(GitFetchAction::Native {
        remote,
        refspecs,
        prune,
        filter,
        quiet,
    })
}

fn handle_git_fetch(parsed: &ParsedGitGlobals) -> ExitCode {
    let (clean_args, mode) = extract_json_agent_flags(&parsed.subcommand_args, parsed.mode);
    let os_args: Vec<OsString> = clean_args.iter().map(OsString::from).collect();
    let mut adjusted = ParsedGitGlobals {
        effective_dir: parsed.effective_dir.clone(),
        delegated_globals: parsed.delegated_globals.clone(),
        config_overrides: parsed.config_overrides.clone(),
        no_pager: parsed.no_pager,
        mode,
        subcommand: parsed.subcommand.clone(),
        subcommand_args: os_args.clone(),
    };
    if clean_args.iter().any(|a| a == "--dry-run" || a == "-n") {
        return delegate_readonly(&adjusted, &["fetch"], &os_args);
    }

    let action = match parse_git_fetch_action(&clean_args, has_exotic_git_globals(parsed)) {
        Ok(a) => a,
        Err(err) => return emit_error(mode, &err),
    };

    match action {
        GitFetchAction::Fallback => {
            adjusted.mode.non_interactive = true;
            delegate_mutating(&adjusted, "fetch", &os_args, true, false)
        }
        GitFetchAction::Native {
            remote,
            refspecs,
            prune,
            filter,
            quiet,
        } => {
            let res = (|| -> Result<Envelope<FetchOutcome>> {
                let repo = discover_and_recover_repo(parsed.effective_dir.as_deref(), false)?;
                let actor = actor_from_mode(mode, &repo);
                let outcome = fetch_remote(
                    &repo,
                    &FetchOptions {
                        remote,
                        refspecs,
                        prune,
                        filter,
                        tags: false,
                        auth_token: None,
                        auth_scheme: HttpAuthScheme::BasicToken,
                    },
                    actor,
                )?;

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

            emit_json_or_human(mode, res, |env| {
                if quiet {
                    return;
                }
                let d = &env.data;
                if d.updated_refs.is_empty() && d.pruned_refs.is_empty() {
                    return;
                }
                println!("From {}", d.remote_url);
                for u in &d.updated_refs {
                    let new_short = &u.new_oid[..u.new_oid.len().min(7)];
                    match &u.old_oid {
                        Some(old) => {
                            let old_short = &old[..old.len().min(7)];
                            let sep = if u.forced { "..." } else { ".." };
                            let prefix = if u.forced { " + " } else { "   " };
                            println!("{prefix}{old_short}{sep}{new_short}  {} -> {}", u.remote_ref, u.local_ref);
                        }
                        None => {
                            println!(" * [new ref]         {} -> {}", u.remote_ref, u.local_ref);
                        }
                    }
                }
                for p in &d.pruned_refs {
                    println!(" - [deleted]         (none)     -> {p}");
                }
            })
        }
    }
}

enum GitPushAction {
    Native {
        remote: Option<String>,
        refspecs: Vec<String>,
        atomic: bool,
        force: bool,
        force_with_lease: bool,
        lease_specs: Vec<ForceLeaseSpec>,
        set_upstream: bool,
        quiet: bool,
        no_verify: bool,
    },
    Fallback,
}

fn parse_git_push_action(args: &[String], has_exotic_globals: bool) -> GitPushAction {
    if has_exotic_globals {
        return GitPushAction::Fallback;
    }

    let mut atomic = false;
    let mut force = false;
    let mut force_with_lease = false;
    let mut lease_specs: Vec<ForceLeaseSpec> = Vec::new();
    let mut set_upstream = false;
    let mut quiet = false;
    let mut no_verify = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut after_sep = false;
    let mut i = 0usize;

    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            after_sep = true;
            i += 1;
            continue;
        }
        if !after_sep && arg.starts_with('-') {
            match arg.as_str() {
                "--atomic" => {
                    atomic = true;
                    i += 1;
                    continue;
                }
                "--no-atomic" => {
                    atomic = false;
                    i += 1;
                    continue;
                }
                "--force-with-lease" => {
                    force_with_lease = true;
                    i += 1;
                    continue;
                }
                "--no-force-with-lease" => {
                    force_with_lease = false;
                    lease_specs.clear();
                    i += 1;
                    continue;
                }
                "-f" | "--force" => {
                    force = true;
                    i += 1;
                    continue;
                }
                "-u" | "--set-upstream" => {
                    set_upstream = true;
                    i += 1;
                    continue;
                }
                "-q" | "--quiet" => {
                    quiet = true;
                    i += 1;
                    continue;
                }
                "--no-verify" => {
                    no_verify = true;
                    i += 1;
                    continue;
                }
                "-v" | "--verbose" | "--progress" | "--no-progress" => {
                    i += 1;
                    continue;
                }
                _ => {
                    if let Some(spec) = arg.strip_prefix("--force-with-lease=") {
                        lease_specs.push(ForceLeaseSpec::parse(spec));
                        i += 1;
                        continue;
                    }
                    if let Some(short) = arg.strip_prefix('-')
                        && !short.starts_with('-')
                        && short.len() > 1
                        && short.chars().all(|c| matches!(c, 'f' | 'u' | 'q' | 'v'))
                    {
                        for ch in short.chars() {
                            match ch {
                                'f' => force = true,
                                'u' => set_upstream = true,
                                'q' => quiet = true,
                                'v' => {}
                                _ => {}
                            }
                        }
                        i += 1;
                        continue;
                    }
                    return GitPushAction::Fallback;
                }
            }
        }
        positionals.push(arg.clone());
        i += 1;
    }

    let remote = positionals.first().cloned();
    let refspecs = if positionals.len() > 1 {
        positionals[1..].to_vec()
    } else {
        Vec::new()
    };

    GitPushAction::Native {
        remote,
        refspecs,
        atomic,
        force,
        force_with_lease,
        lease_specs,
        set_upstream,
        quiet,
        no_verify,
    }
}

fn read_configured_branch_remote(repo: &OzoneRepo) -> Option<String> {
    let branch = repo.branch.as_deref()?;
    let cfg_path = repo.git_common_dir.join("config");
    let raw = std::fs::read_to_string(cfg_path).ok()?;
    let target_header = format!("[branch \"{branch}\"]");
    let mut inside = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            inside = trimmed == target_header;
            continue;
        }
        if inside
            && let Some((k, v)) = trimmed.split_once('=')
            && k.trim().eq_ignore_ascii_case("remote")
        {
            let val = v.trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn handle_git_push(parsed: &ParsedGitGlobals) -> ExitCode {
    let (clean_args, mode) = extract_json_agent_flags(&parsed.subcommand_args, parsed.mode);
    let os_args: Vec<OsString> = clean_args.iter().map(OsString::from).collect();
    let mut adjusted = ParsedGitGlobals {
        effective_dir: parsed.effective_dir.clone(),
        delegated_globals: parsed.delegated_globals.clone(),
        config_overrides: parsed.config_overrides.clone(),
        no_pager: parsed.no_pager,
        mode,
        subcommand: parsed.subcommand.clone(),
        subcommand_args: os_args.clone(),
    };
    if clean_args.iter().any(|a| a == "--dry-run" || a == "-n") {
        return delegate_readonly(&adjusted, &["push"], &os_args);
    }

    match parse_git_push_action(&clean_args, has_non_config_exotic_globals(parsed)) {
        GitPushAction::Fallback => {
            adjusted.mode.non_interactive = true;
            delegate_mutating(&adjusted, "push", &os_args, true, false)
        }
        GitPushAction::Native {
            remote,
            refspecs,
            atomic,
            force,
            force_with_lease,
            lease_specs,
            set_upstream,
            quiet,
            no_verify,
        } => {
            let res = (|| -> Result<Envelope<PushOutcome>> {
                let repo = discover_and_recover_repo(parsed.effective_dir.as_deref(), false)?;
                let effective_remote = remote.or_else(|| read_configured_branch_remote(&repo));
                let actor = actor_from_mode(mode, &repo);
                let outcome = push_refs_with_options(
                    &repo,
                    &PushOptions {
                        remote: effective_remote,
                        refspecs,
                        atomic,
                        force,
                        force_with_lease,
                        lease_specs,
                        delete: false,
                        set_upstream,
                        auth_token: None,
                        auth_scheme: HttpAuthScheme::BasicToken,
                    },
                    no_verify,
                    &parsed.config_overrides,
                    actor,
                )?;

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

            emit_json_or_human(mode, res, |env| {
                if quiet {
                    return;
                }
                let d = &env.data;
                println!("To {}", d.remote_url);
                for p in &d.pushed_refs {
                    let local_label = p.local_ref.as_deref().unwrap_or("HEAD");
                    if p.deleted {
                        println!(" - [deleted]         {}", p.remote_ref);
                    } else if let (Some(old), Some(new)) = (&p.old_oid, &p.new_oid) {
                        let old_short = &old[..old.len().min(7)];
                        let new_short = &new[..new.len().min(7)];
                        if p.forced {
                            println!(
                                " + {old_short}...{new_short} {local_label} -> {} (forced update)",
                                p.remote_ref
                            );
                        } else {
                            println!("   {old_short}..{new_short}  {local_label} -> {}", p.remote_ref);
                        }
                    } else {
                        println!(" * [new branch]      {local_label} -> {}", p.remote_ref);
                    }
                }
                for br in &d.upstream_configured {
                    println!("branch '{br}' set up to track '{}/{br}'.", d.remote);
                }
            })
        }
    }
}

// ============================================================================
// Remaining Read-Only vs Mutating Legacy Commands
// ============================================================================

fn handle_other_git_command(parsed: &ParsedGitGlobals, subcmd: &str) -> ExitCode {
    if is_read_only_git_command(subcmd, &parsed.subcommand_args) {
        delegate_readonly(parsed, &[subcmd], &parsed.subcommand_args)
    } else {
        let watch_all_refs = matches!(subcmd, "pull" | "remote" | "fetch" | "push");
        let suppress_editor = matches!(
            subcmd,
            "revert" | "cherry-pick" | "pull" | "am" | "tag" | "notes"
        );
        delegate_mutating(
            parsed,
            subcmd,
            &parsed.subcommand_args,
            watch_all_refs,
            suppress_editor,
        )
    }
}

fn is_read_only_git_command(subcmd: &str, args: &[OsString]) -> bool {
    let str_args: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    if matches!(
        subcmd,
        "log"
            | "show"
            | "blame"
            | "annotate"
            | "shortlog"
            | "whatchanged"
            | "rev-parse"
            | "rev-list"
            | "merge-base"
            | "describe"
            | "name-rev"
            | "cat-file"
            | "ls-files"
            | "ls-tree"
            | "show-ref"
            | "for-each-ref"
            | "grep"
            | "diff-files"
            | "diff-index"
            | "diff-tree"
            | "check-ignore"
            | "check-attr"
            | "var"
            | "version"
            | "help"
            | "ls-remote"
            | "count-objects"
            | "verify-commit"
            | "verify-tag"
    ) {
        return true;
    }

    match subcmd {
        "branch" => {
            let mutating = str_args.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "-d" | "-D"
                        | "--delete"
                        | "-m"
                        | "-M"
                        | "--move"
                        | "-c"
                        | "-C"
                        | "--copy"
                        | "-f"
                        | "--force"
                        | "--set-upstream-to"
                        | "-u"
                        | "--unset-upstream"
                        | "--edit-description"
                ) || a.starts_with("--set-upstream-to=")
            });
            if mutating {
                return false;
            }
            let explicitly_listing = str_args.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "-l" | "--list" | "-a" | "--all" | "-r" | "--remotes" | "--show-current"
                )
            });
            explicitly_listing || str_args.iter().all(|a| a.starts_with('-'))
        }
        "tag" => {
            let mutating = str_args.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "-d" | "--delete" | "-a" | "--annotate" | "-s" | "--sign" | "-f" | "--force"
                )
            });
            if mutating {
                return false;
            }
            str_args
                .iter()
                .any(|a| matches!(a.as_str(), "-l" | "--list" | "-v" | "--verify"))
                || str_args.iter().all(|a| a.starts_with('-'))
        }
        "stash" => matches!(
            str_args.first().map(String::as_str),
            Some("list" | "show")
        ),
        "remote" => !str_args.iter().any(|a| {
            matches!(
                a.as_str(),
                "add" | "remove" | "rm" | "rename" | "set-url" | "prune" | "update" | "set-head"
                    | "set-branches"
            )
        }),
        "config" => str_args.iter().any(|a| {
            matches!(
                a.as_str(),
                "--get"
                    | "--get-all"
                    | "--get-regexp"
                    | "--get-urlmatch"
                    | "--list"
                    | "-l"
                    | "--show-origin"
                    | "--show-scope"
            )
        }),
        "symbolic-ref" => !str_args.iter().any(|a| a == "-d" || a == "--delete")
            && str_args.iter().filter(|a| !a.starts_with('-')).count() <= 1,
        "reflog" => matches!(
            str_args.first().map(String::as_str),
            None | Some("show" | "exists")
        ),
        _ => false,
    }
}
