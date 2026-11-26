// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Repository health and maintenance subcommands (`oz gc`, `oz fsck`).

use crate::commands::print_json_envelope;
use clap::Args;
use oz_core::{Envelope, EnvelopeError, ExitCode, InteractionMode, OzoneRepo, Result};
use oz_oplog::{
    DEFAULT_OPLOG_RETENTION_DAYS, FsckCheckStatus, FsckOptions, FsckReport, OpLogGcOptions,
    OpLogGcOutcome, compact_oplog, fsck_repo,
};
use std::path::Path;

#[derive(Debug, Args)]
pub struct GcArgs {
    /// Retain `OpLog` entries newer than `<N>` days (defaults to `30`).
    #[arg(long = "oplog-days", default_value_t = DEFAULT_OPLOG_RETENTION_DAYS, value_name = "N")]
    pub oplog_days: u64,
    /// Optional hard cap on retained non-protected `OpLog` entries.
    #[arg(long = "max-entries", value_name = "N")]
    pub max_entries: Option<usize>,
    /// Preview deletions and compaction without modifying `.git/ozone/oplog/` or refs.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct FsckArgs {
    /// Safely recover/clean interrupted transactions, broken `OpLog` pointers, and orphaned
    /// metadata records without deleting user working-tree files.
    #[arg(long)]
    pub repair: bool,
}

pub(crate) fn handle_gc(
    args: &GcArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res: Result<Envelope<OpLogGcOutcome>> = (|| {
        let repo = OzoneRepo::discover(directory)?;
        let outcome = compact_oplog(
            &repo,
            &OpLogGcOptions {
                oplog_days: args.oplog_days,
                max_entries: args.max_entries,
                dry_run: args.dry_run,
                now: None,
            },
        )?;
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid)
            .with_recovery_command("oz fsck --json"))
    })();

    match res {
        Ok(env) => {
            if mode.json {
                print_json_envelope(&env);
            } else {
                let d = &env.data;
                let prefix = if d.dry_run { "gc (dry-run)" } else { "gc" };
                println!(
                    "{prefix}: removed {} oplog entry/entries, retained {} ({} protected, {} relinked), pruned {} ref(s), reclaimed {} bytes ({} bytes remaining)",
                    d.removed_entries.len(),
                    d.retained_entries,
                    d.protected_entries,
                    d.relinked_entries,
                    d.pruned_checkpoint_refs.len(),
                    d.reclaimed_bytes,
                    d.remaining_bytes,
                );
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

pub(crate) fn handle_fsck(
    args: &FsckArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let query_res: Result<(OzoneRepo, FsckReport)> = (|| {
        let repo = OzoneRepo::discover(directory)?;
        let report = fsck_repo(
            &repo,
            &FsckOptions {
                repair: args.repair,
            },
        )?;
        Ok((repo, report))
    })();

    let (repo, report) = match query_res {
        Ok(pair) => pair,
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
            return err.exit_code();
        }
    };

    let healthy = report.healthy;
    let exit_code = if healthy {
        ExitCode::Success
    } else {
        ExitCode::ConflictOrCheckFail
    };

    let any_repairable_unresolved = report.issues.iter().any(|i| !i.repaired && i.repairable);
    let mut env = Envelope::success(report)
        .with_workspace(repo.workspace)
        .with_branch(repo.branch)
        .with_head_oid(repo.head_oid);

    if !healthy {
        let msg = format!(
            "repository integrity check found {} unresolved issue(s) (git_fsck_passed={})",
            env.data.unresolved_issue_count, env.data.git_fsck_passed
        );
        env.ok = false;
        env.error = Some(EnvelopeError {
            code: "check_failed".to_string(),
            exit_code: exit_code.as_i32(),
            message: msg,
        });
        if any_repairable_unresolved {
            env = env.with_recovery_command("oz fsck --repair --json");
        }
    }

    if mode.json {
        print_json_envelope(&env);
    } else {
        let d = &env.data;
        let state_label = if d.healthy { "healthy" } else { "unhealthy" };
        println!(
            "fsck ({state_label}): {} oplog entries, {} snapshot objects, {} mounts, {} baselines",
            d.oplog_entries_checked,
            d.snapshot_objects_checked,
            d.mounts_checked,
            d.baselines_checked,
        );
        for chk in &d.checks {
            let tag = match chk.status {
                FsckCheckStatus::Ok => "ok",
                FsckCheckStatus::Repaired => "repaired",
                FsckCheckStatus::Warn => "warn",
                FsckCheckStatus::Fail => "fail",
            };
            println!("  [{tag}] {}: {}", chk.id, chk.summary);
        }
        for issue in &d.issues {
            let status_tag = if issue.repaired { "repaired" } else { "issue" };
            println!("  [{status_tag}] {}: {}", issue.code, issue.message);
        }
    }

    exit_code
}
