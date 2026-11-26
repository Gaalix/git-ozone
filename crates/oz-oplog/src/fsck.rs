// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Repository integrity verification (`oz fsck`) and safe non-destructive repair (`--repair`)
//! covering colocated `.git/` (`git fsck --full`), `OpLog` DAG & undo-chain consistency,
//! snapshot object reachability, interrupted transactions, and orphaned mounts/baselines.

use crate::store::{
    atomic_write_bytes, entry_path, head_path, lock_head, ops_dir, process_oplog_mutex,
    validate_op_id, write_head_op_id,
};
use crate::types::{
    DirtyFileState, FsckCategory, FsckCheckStatus, FsckCheckSummary, FsckIssue, FsckOptions,
    FsckReport, FsckSeverity, OpKind, OpLogEntry,
};
use chrono::Utc;
use gix::bstr::ByteSlice;
use gix::hash::ObjectId;
use gix::refs::FullName;
use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};
use oz_core::{
    MountHealth, MountRecord, OzError, OzoneRepo, Result, WorkspaceState, probe_mount_health_os,
};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use walkdir::WalkDir;

#[derive(Debug, Deserialize)]
struct MinimalBaselineMeta {
    commit_oid: String,
}

#[derive(Debug, Deserialize)]
struct MinimalIntegrateSession {
    sandbox_path: PathBuf,
}

/// Verifies colocated `.git/` integrity (including `git fsck --full`), `OpLog` chain consistency,
/// snapshot reachability, and Ozone metadata health (`oz fsck [--repair]`).
///
/// With `opts.repair == true`, safely recovers/cleans interrupted transactions and orphaned
/// records without ever deleting user working-tree files.
pub fn fsck_repo(repo: &OzoneRepo, opts: &FsckOptions) -> Result<FsckReport> {
    let mut issues: Vec<FsckIssue> = Vec::new();
    let mut repaired_actions: Vec<String> = Vec::new();
    let mut checks: Vec<FsckCheckSummary> = Vec::new();

    // 1. Colocated `.git/` integrity + `git fsck --full` + orphaned `.git/worktrees/*`.
    let (git_fsck_passed, gix_repo_opt) =
        check_and_repair_git(repo, opts.repair, &mut issues, &mut repaired_actions);
    checks.push(summarize_category(
        "git_integrity",
        FsckCategory::Git,
        &issues,
        if git_fsck_passed {
            "colocated .git/ and `git fsck --full` passed".to_string()
        } else {
            "colocated .git/ integrity check reported errors".to_string()
        },
    ));

    // 2. Interrupted transactions under `.git/ozone/` and orphaned `_integrate/` state.
    check_and_repair_interrupted_txns(repo, opts.repair, &mut issues, &mut repaired_actions);
    checks.push(summarize_category(
        "interrupted_transactions",
        FsckCategory::Transaction,
        &issues,
        "no interrupted Ozone transactions or stray temp files".to_string(),
    ));

    // 3. OpLog DAG, `oplog/HEAD`, corrupt entries, and undo/redo references.
    let (oplog_entries_checked, valid_entries) =
        check_and_repair_oplog(repo, opts.repair, &mut issues, &mut repaired_actions)?;
    checks.push(summarize_category(
        "oplog_chain",
        FsckCategory::Oplog,
        &issues,
        format!("{oplog_entries_checked} OpLog entry/entries verified and linked from HEAD"),
    ));

    // 4. Snapshot object reachability in `.git/objects/` & `refs/ozone/*`.
    let snapshot_objects_checked = check_and_repair_snapshots(
        gix_repo_opt.as_ref(),
        &valid_entries,
        opts.repair,
        &mut issues,
        &mut repaired_actions,
    )?;
    checks.push(summarize_category(
        "snapshot_reachability",
        FsckCategory::Snapshot,
        &issues,
        format!("{snapshot_objects_checked} referenced snapshot object(s) reachable"),
    ));

    // 5. Workspace state files, mounts, and stale daemon sockets.
    let (mounts_checked, active_baseline_ids) =
        check_and_repair_mounts(repo, opts.repair, &mut issues, &mut repaired_actions)?;
    checks.push(summarize_category(
        "mounts_and_workspaces",
        FsckCategory::Mount,
        &issues,
        format!("{mounts_checked} workspace/mount record(s) verified"),
    ));

    // 6. Warm baselines (`.git/ozone/baselines/`).
    let baselines_checked = check_and_repair_baselines(
        repo,
        gix_repo_opt.as_ref(),
        &active_baseline_ids,
        opts.repair,
        &mut issues,
        &mut repaired_actions,
    )?;
    checks.push(summarize_category(
        "warm_baselines",
        FsckCategory::Baseline,
        &issues,
        format!("{baselines_checked} warm baseline(s) verified"),
    ));

    let issue_count = issues.len();
    let repaired_count = issues.iter().filter(|i| i.repaired).count();
    let unresolved_issue_count = issues.iter().filter(|i| !i.repaired).count();
    let healthy = git_fsck_passed && unresolved_issue_count == 0;

    Ok(FsckReport {
        healthy,
        repair_mode: opts.repair,
        git_fsck_passed,
        oplog_entries_checked,
        snapshot_objects_checked,
        mounts_checked,
        baselines_checked,
        issue_count,
        unresolved_issue_count,
        repaired_count,
        checks,
        issues,
        repaired_actions,
    })
}

fn summarize_category(
    id: &str,
    category: FsckCategory,
    issues: &[FsckIssue],
    ok_summary: String,
) -> FsckCheckSummary {
    let cat_issues: Vec<&FsckIssue> = issues.iter().filter(|i| i.category == category).collect();
    if cat_issues.is_empty() {
        return FsckCheckSummary {
            id: id.to_string(),
            category,
            status: FsckCheckStatus::Ok,
            summary: ok_summary,
        };
    }

    let unresolved_errors = cat_issues
        .iter()
        .filter(|i| !i.repaired && i.severity == FsckSeverity::Error)
        .count();
    let unresolved_warnings = cat_issues
        .iter()
        .filter(|i| !i.repaired && i.severity == FsckSeverity::Warning)
        .count();
    let repaired_here = cat_issues.iter().filter(|i| i.repaired).count();

    let (status, summary) = if unresolved_errors > 0 {
        (
            FsckCheckStatus::Fail,
            format!("{unresolved_errors} unresolved error(s) ({repaired_here} repaired)"),
        )
    } else if unresolved_warnings > 0 {
        (
            FsckCheckStatus::Warn,
            format!("{unresolved_warnings} warning(s) ({repaired_here} repaired)"),
        )
    } else {
        (
            FsckCheckStatus::Repaired,
            format!("repaired {repaired_here} issue(s)"),
        )
    };

    FsckCheckSummary {
        id: id.to_string(),
        category,
        status,
        summary,
    }
}

fn check_and_repair_git(
    repo: &OzoneRepo,
    repair: bool,
    issues: &mut Vec<FsckIssue>,
    repaired_actions: &mut Vec<String>,
) -> (bool, Option<gix::Repository>) {
    let mut git_ok = true;
    let gix_repo_opt = match gix::open(&repo.repo_root) {
        Ok(gr) => {
            if let Err(err) = gr.head() {
                git_ok = false;
                issues.push(FsckIssue {
                    code: "corrupt_git_head".to_string(),
                    category: FsckCategory::Git,
                    severity: FsckSeverity::Error,
                    message: format!("repository HEAD is invalid: {err}"),
                    path: Some(repo.git_dir.join("HEAD").display().to_string()),
                    op_id: None,
                    repairable: false,
                    repaired: false,
                    action: None,
                });
            }
            let idx_path = repo.git_dir.join("index");
            if idx_path.is_file()
                && let Err(err) = gr.open_index()
            {
                git_ok = false;
                issues.push(FsckIssue {
                    code: "corrupt_git_index".to_string(),
                    category: FsckCategory::Git,
                    severity: FsckSeverity::Error,
                    message: format!("Git index is unreadable/corrupt: {err}"),
                    path: Some(idx_path.display().to_string()),
                    op_id: None,
                    repairable: false,
                    repaired: false,
                    action: None,
                });
            }
            Some(gr)
        }
        Err(err) => {
            git_ok = false;
            issues.push(FsckIssue {
                code: "corrupt_git_repo".to_string(),
                category: FsckCategory::Git,
                severity: FsckSeverity::Error,
                message: format!("failed to open colocated Git repository: {err}"),
                path: Some(repo.git_common_dir.display().to_string()),
                op_id: None,
                repairable: false,
                repaired: false,
                action: None,
            });
            None
        }
    };

    // Run real `git fsck --full`.
    if let Some(real_git) = resolve_real_git() {
        let mut cmd = Command::new(&real_git);
        cmd.args(["fsck", "--full"])
            .current_dir(&repo.repo_root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .stdin(Stdio::null());
        if !repo.repo_root.join(".git").exists() {
            cmd.env("GIT_DIR", &repo.git_common_dir);
        }
        match cmd.output() {
            Ok(out) => {
                if !out.status.success() {
                    git_ok = false;
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let detail = format!("{stdout}\n{stderr}")
                        .lines()
                        .filter(|l| !l.trim().is_empty() && !l.starts_with("dangling "))
                        .take(5)
                        .collect::<Vec<_>>()
                        .join("; ");
                    issues.push(FsckIssue {
                        code: "git_fsck_failed".to_string(),
                        category: FsckCategory::Git,
                        severity: FsckSeverity::Error,
                        message: if detail.is_empty() {
                            format!("`git fsck --full` exited with status {}", out.status)
                        } else {
                            format!("`git fsck --full` failed: {detail}")
                        },
                        path: Some(repo.git_common_dir.display().to_string()),
                        op_id: None,
                        repairable: false,
                        repaired: false,
                        action: None,
                    });
                }
            }
            Err(err) => {
                git_ok = false;
                issues.push(FsckIssue {
                    code: "git_fsck_exec_error".to_string(),
                    category: FsckCategory::Git,
                    severity: FsckSeverity::Error,
                    message: format!("failed to run `git fsck --full`: {err}"),
                    path: Some(real_git.display().to_string()),
                    op_id: None,
                    repairable: false,
                    repaired: false,
                    action: None,
                });
            }
        }
    } else {
        issues.push(FsckIssue {
            code: "git_binary_missing".to_string(),
            category: FsckCategory::Git,
            severity: FsckSeverity::Warning,
            message: "system `git` binary not found; skipped `git fsck --full`".to_string(),
            path: None,
            op_id: None,
            repairable: false,
            repaired: false,
            action: None,
        });
    }

    // Check orphaned `.git/worktrees/<name>` entries (without deleting any existing worktree).
    let wt_dir = repo.git_common_dir.join("worktrees");
    if wt_dir.is_dir()
        && let Ok(rd) = fs::read_dir(&wt_dir)
    {
        for item in rd.flatten() {
            let entry_dir = item.path();
            if !entry_dir.is_dir() {
                continue;
            }
            let Some(name) = entry_dir.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let has_state = repo.layout.workspace_state_path(name).is_file();
            let has_mount = repo.layout.ws_mount_record_path(name).is_file();
            if has_state || has_mount {
                continue;
            }
            let gitdir_file = entry_dir.join("gitdir");
            let target_missing = match fs::read_to_string(&gitdir_file) {
                Ok(raw) => {
                    let target = PathBuf::from(raw.trim());
                    let wt_root = target.parent().unwrap_or(target.as_path());
                    probe_mount_health_os(wt_root) == MountHealth::NotMounted && !wt_root.exists()
                }
                Err(_) => true,
            };
            if target_missing {
                let mut repaired = false;
                let mut action = None;
                if repair && fs::remove_dir_all(&entry_dir).is_ok() {
                    repaired = true;
                    let msg = format!(
                        "removed orphaned git worktree metadata {}",
                        entry_dir.display()
                    );
                    repaired_actions.push(msg.clone());
                    action = Some(msg);
                }
                issues.push(FsckIssue {
                    code: "orphaned_git_worktree".to_string(),
                    category: FsckCategory::Git,
                    severity: FsckSeverity::Warning,
                    message: format!(
                        "orphaned .git/worktrees/{name} metadata with missing worktree directory"
                    ),
                    path: Some(entry_dir.display().to_string()),
                    op_id: None,
                    repairable: true,
                    repaired,
                    action,
                });
            }
        }
    }

    (git_ok, gix_repo_opt)
}

fn check_and_repair_interrupted_txns(
    repo: &OzoneRepo,
    repair: bool,
    issues: &mut Vec<FsckIssue>,
    repaired_actions: &mut Vec<String>,
) {
    let layout = &repo.layout;
    if !layout.ozone_dir.is_dir() {
        return;
    }

    // 1. Scan temp files inside `.git/ozone/` subdirs.
    let temp_dirs = [
        layout.ozone_dir.clone(),
        layout.state_dir(),
        layout.oplog_dir(),
        ops_dir(layout),
        layout.ozone_dir.join("index_cache"),
        layout.ozone_dir.join("integrate"),
    ];
    for dir in &temp_dirs {
        if !dir.is_dir() {
            continue;
        }
        let Ok(rd) = fs::read_dir(dir) else {
            continue;
        };
        for item in rd.flatten() {
            let name = item.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let path = item.path();
            if path.is_file() && is_interrupted_temp_filename(name_str) {
                let mut repaired = false;
                let mut action = None;
                if repair && fs::remove_file(&path).is_ok() {
                    repaired = true;
                    let msg = format!(
                        "removed interrupted transaction temp file {}",
                        path.display()
                    );
                    repaired_actions.push(msg.clone());
                    action = Some(msg);
                }
                issues.push(FsckIssue {
                    code: "interrupted_transaction".to_string(),
                    category: FsckCategory::Transaction,
                    severity: FsckSeverity::Warning,
                    message: format!("leftover interrupted transaction file '{}'", path.display()),
                    path: Some(path.display().to_string()),
                    op_id: None,
                    repairable: true,
                    repaired,
                    action,
                });
            }
        }
    }

    // 2. Scan interrupted `.staging-*` and `.removing-*` dirs in `.git/ozone/baselines/`.
    let baselines_dir = layout.baselines_dir();
    if baselines_dir.is_dir()
        && let Ok(rd) = fs::read_dir(&baselines_dir)
    {
        for item in rd.flatten() {
            let name = item.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let path = item.path();
            if path.is_dir()
                && (name_str.starts_with(".staging-") || name_str.starts_with(".removing-"))
            {
                let mut repaired = false;
                let mut action = None;
                if repair && fs::remove_dir_all(&path).is_ok() {
                    repaired = true;
                    let msg = format!(
                        "removed interrupted baseline staging dir {}",
                        path.display()
                    );
                    repaired_actions.push(msg.clone());
                    action = Some(msg);
                }
                issues.push(FsckIssue {
                    code: "interrupted_baseline".to_string(),
                    category: FsckCategory::Transaction,
                    severity: FsckSeverity::Warning,
                    message: format!("interrupted baseline directory '{}'", path.display()),
                    path: Some(path.display().to_string()),
                    op_id: None,
                    repairable: true,
                    repaired,
                    action,
                });
            }
        }
    }

    // 3. Inspect `.git/ozone/integrate/session.json` and `../<repo>.oz/_integrate`.
    let integrate_dir = layout.ozone_dir.join("integrate");
    let session_path = integrate_dir.join("session.json");
    if session_path.is_file() {
        match fs::read_to_string(&session_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<MinimalIntegrateSession>(&raw).ok())
        {
            None => {
                let mut repaired = false;
                let mut action = None;
                if repair && fs::remove_file(&session_path).is_ok() {
                    let _ = fs::remove_dir(&integrate_dir);
                    repaired = true;
                    let msg = format!(
                        "removed corrupt integrate session {}",
                        session_path.display()
                    );
                    repaired_actions.push(msg.clone());
                    action = Some(msg);
                }
                issues.push(FsckIssue {
                    code: "corrupt_integrate_session".to_string(),
                    category: FsckCategory::Transaction,
                    severity: FsckSeverity::Error,
                    message: format!(
                        "corrupt integrate session file '{}'",
                        session_path.display()
                    ),
                    path: Some(session_path.display().to_string()),
                    op_id: None,
                    repairable: true,
                    repaired,
                    action,
                });
            }
            Some(sess) => {
                if !sess.sandbox_path.exists()
                    && probe_mount_health_os(&sess.sandbox_path) == MountHealth::NotMounted
                {
                    let mut repaired = false;
                    let mut action = None;
                    if repair && fs::remove_file(&session_path).is_ok() {
                        let _ = fs::remove_dir(&integrate_dir);
                        repaired = true;
                        let msg = format!(
                            "removed orphaned integrate session {} (sandbox missing)",
                            session_path.display()
                        );
                        repaired_actions.push(msg.clone());
                        action = Some(msg);
                    }
                    issues.push(FsckIssue {
                        code: "orphaned_integrate_session".to_string(),
                        category: FsckCategory::Transaction,
                        severity: FsckSeverity::Warning,
                        message: format!(
                            "integrate session '{}' references missing sandbox '{}'",
                            session_path.display(),
                            sess.sandbox_path.display()
                        ),
                        path: Some(session_path.display().to_string()),
                        op_id: None,
                        repairable: true,
                        repaired,
                        action,
                    });
                }
            }
        }
    } else if let Ok(sandbox_dir) = repo
        .config
        .render_workspace_path(&repo.repo_root, "_integrate")
        && sandbox_dir.is_dir()
    {
        let has_files = dir_has_user_files(&sandbox_dir);
        let mut repaired = false;
        let mut action = None;
        if repair && !has_files && fs::remove_dir_all(&sandbox_dir).is_ok() {
            repaired = true;
            let msg = format!(
                "removed empty orphaned _integrate sandbox {}",
                sandbox_dir.display()
            );
            repaired_actions.push(msg.clone());
            action = Some(msg);
        }
        issues.push(FsckIssue {
            code: "orphaned_integrate_sandbox".to_string(),
            category: FsckCategory::Transaction,
            severity: FsckSeverity::Warning,
            message: if has_files {
                format!(
                    "orphaned _integrate sandbox at '{}' without session.json (preserving files; run `oz integrate --abort` to discard)",
                    sandbox_dir.display()
                )
            } else {
                format!(
                    "empty orphaned _integrate sandbox at '{}' without session.json",
                    sandbox_dir.display()
                )
            },
            path: Some(sandbox_dir.display().to_string()),
            op_id: None,
            repairable: !has_files,
            repaired,
            action,
        });
    }
}

fn has_json_extension(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

fn is_interrupted_temp_filename(name: &str) -> bool {
    name.starts_with(".tmp") || (name.starts_with('.') && name.contains(".tmp."))
}

fn check_and_repair_oplog(
    repo: &OzoneRepo,
    repair: bool,
    issues: &mut Vec<FsckIssue>,
    repaired_actions: &mut Vec<String>,
) -> Result<(usize, BTreeMap<String, OpLogEntry>)> {
    let layout = &repo.layout;
    let oplog = layout.oplog_dir();
    if !oplog.is_dir() {
        return Ok((0, BTreeMap::new()));
    }

    let proc_mutex = process_oplog_mutex(&oplog);
    let _lock = if repair {
        Some(lock_head(layout, &proc_mutex)?)
    } else {
        None
    };

    let ops = ops_dir(layout);
    let mut valid_entries: BTreeMap<String, OpLogEntry> = BTreeMap::new();
    let mut corrupt_paths: Vec<PathBuf> = Vec::new();

    if ops.is_dir() {
        for item in fs::read_dir(&ops)? {
            let item = item?;
            let path = item.path();
            let name = item.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if name_str.starts_with('.') {
                continue;
            }
            if !path.is_file() || !has_json_extension(name_str) {
                corrupt_paths.push(path);
                continue;
            }
            let stem = &name_str[..name_str.len() - ".json".len()];
            if validate_op_id(stem).is_err() {
                corrupt_paths.push(path);
                continue;
            }
            match fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<OpLogEntry>(&raw).ok())
            {
                Some(entry) if entry.op_id == stem => {
                    valid_entries.insert(entry.op_id.clone(), entry);
                }
                _ => {
                    corrupt_paths.push(path);
                }
            }
        }
    }

    corrupt_paths.sort();
    for cp in &corrupt_paths {
        let mut repaired = false;
        let mut action = None;
        if repair {
            let quarantine_dir = oplog.join("quarantine");
            let fname = cp.file_name().and_then(|s| s.to_str()).unwrap_or("unknown");
            let file_len = fs::metadata(cp).map_or(0, |m| m.len());
            if file_len == 0 {
                if fs::remove_file(cp).is_ok() {
                    repaired = true;
                    let msg = format!("removed 0-byte corrupt oplog file {}", cp.display());
                    repaired_actions.push(msg.clone());
                    action = Some(msg);
                }
            } else if fs::create_dir_all(&quarantine_dir).is_ok() {
                let dst = quarantine_dir.join(format!("{fname}.corrupt"));
                if fs::rename(cp, &dst).is_ok() {
                    repaired = true;
                    let msg = format!(
                        "quarantined corrupt oplog entry {} -> {}",
                        cp.display(),
                        dst.display()
                    );
                    repaired_actions.push(msg.clone());
                    action = Some(msg);
                }
            }
        }
        issues.push(FsckIssue {
            code: "corrupt_oplog_entry".to_string(),
            category: FsckCategory::Oplog,
            severity: FsckSeverity::Error,
            message: format!("corrupt or malformed oplog entry '{}'", cp.display()),
            path: Some(cp.display().to_string()),
            op_id: None,
            repairable: true,
            repaired,
            action,
        });
    }

    // Read raw HEAD without failing on malformed contents.
    let head_file = head_path(layout);
    let raw_head_opt = match fs::read_to_string(&head_file) {
        Ok(s) => {
            let t = s.trim().to_string();
            if t.is_empty() { None } else { Some(Ok(t)) }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => Some(Err(err.to_string())),
    };

    let mut head_op_id: Option<String> = None;
    match raw_head_opt {
        None => {
            if !valid_entries.is_empty() {
                let newest = newest_entry_id(&valid_entries);
                let mut repaired = false;
                let mut action = None;
                if repair && let Some(ref target_id) = newest {
                    write_head_op_id(layout, Some(target_id))?;
                    head_op_id = Some(target_id.clone());
                    repaired = true;
                    let msg = format!("reconstructed missing oplog/HEAD -> {target_id}");
                    repaired_actions.push(msg.clone());
                    action = Some(msg);
                }
                issues.push(FsckIssue {
                    code: "missing_oplog_head".to_string(),
                    category: FsckCategory::Oplog,
                    severity: FsckSeverity::Error,
                    message: format!(
                        "oplog/HEAD is missing/empty while {} valid entry/entries exist",
                        valid_entries.len()
                    ),
                    path: Some(head_file.display().to_string()),
                    op_id: newest,
                    repairable: true,
                    repaired,
                    action,
                });
            }
        }
        Some(Err(err_msg)) => {
            let newest = newest_entry_id(&valid_entries);
            let mut repaired = false;
            let mut action = None;
            if repair {
                write_head_op_id(layout, newest.as_deref())?;
                head_op_id.clone_from(&newest);
                repaired = true;
                let msg = match &newest {
                    Some(id) => format!("repaired unreadable oplog/HEAD -> {id}"),
                    None => "cleared unreadable oplog/HEAD".to_string(),
                };
                repaired_actions.push(msg.clone());
                action = Some(msg);
            }
            issues.push(FsckIssue {
                code: "broken_oplog_head".to_string(),
                category: FsckCategory::Oplog,
                severity: FsckSeverity::Error,
                message: format!("oplog/HEAD is unreadable: {err_msg}"),
                path: Some(head_file.display().to_string()),
                op_id: newest,
                repairable: true,
                repaired,
                action,
            });
        }
        Some(Ok(raw_id)) => {
            if validate_op_id(&raw_id).is_err() || !valid_entries.contains_key(&raw_id) {
                let newest = newest_entry_id(&valid_entries);
                let mut repaired = false;
                let mut action = None;
                if repair {
                    write_head_op_id(layout, newest.as_deref())?;
                    head_op_id.clone_from(&newest);
                    repaired = true;
                    let msg = match &newest {
                        Some(id) => format!("repaired broken oplog/HEAD '{raw_id}' -> {id}"),
                        None => format!("cleared dangling oplog/HEAD '{raw_id}'"),
                    };
                    repaired_actions.push(msg.clone());
                    action = Some(msg);
                }
                issues.push(FsckIssue {
                    code: "broken_oplog_head".to_string(),
                    category: FsckCategory::Oplog,
                    severity: FsckSeverity::Error,
                    message: format!("oplog/HEAD points to missing/invalid op '{raw_id}'"),
                    path: Some(head_file.display().to_string()),
                    op_id: Some(raw_id),
                    repairable: true,
                    repaired,
                    action,
                });
            } else {
                head_op_id = Some(raw_id);
            }
        }
    }

    if valid_entries.is_empty() {
        return Ok((0, valid_entries));
    }

    // Track entries modified during `--repair` so we persist them once cleanly.
    let mut dirty_entries: HashSet<String> = HashSet::new();

    // Check dangling parent links, self-loops, and cycles.
    let all_ids: Vec<String> = valid_entries.keys().cloned().collect();
    for op_id in &all_ids {
        let parents = valid_entries[op_id].parents.clone();
        let mut bad_parents = Vec::new();
        for p in &parents {
            if p == op_id || !valid_entries.contains_key(p) {
                bad_parents.push(p.clone());
            }
        }
        if !bad_parents.is_empty() {
            let mut repaired = false;
            let mut action = None;
            if repair && let Some(e) = valid_entries.get_mut(op_id) {
                e.parents.retain(|p| p != op_id && !bad_parents.contains(p));
                dirty_entries.insert(op_id.clone());
                repaired = true;
                let msg = format!(
                    "removed dangling parent reference(s) {} from {op_id}",
                    bad_parents.join(", ")
                );
                repaired_actions.push(msg.clone());
                action = Some(msg);
            }
            issues.push(FsckIssue {
                code: "dangling_oplog_parent".to_string(),
                category: FsckCategory::Oplog,
                severity: FsckSeverity::Error,
                message: format!(
                    "oplog entry '{op_id}' references missing/self parent(s): {}",
                    bad_parents.join(", ")
                ),
                path: entry_path(layout, op_id)
                    .ok()
                    .map(|p| p.display().to_string()),
                op_id: Some(op_id.clone()),
                repairable: true,
                repaired,
                action,
            });
        }
    }

    // Check cycles in parent graph (DFS).
    let cycle_edges = find_cycle_edges(&valid_entries);
    if !cycle_edges.is_empty() {
        for (child, parent) in &cycle_edges {
            let mut repaired = false;
            let mut action = None;
            if repair && let Some(e) = valid_entries.get_mut(child) {
                e.parents.retain(|p| p != parent);
                dirty_entries.insert(child.clone());
                repaired = true;
                let msg = format!("broke oplog parent cycle edge {child} -> {parent}");
                repaired_actions.push(msg.clone());
                action = Some(msg);
            }
            issues.push(FsckIssue {
                code: "oplog_cycle".to_string(),
                category: FsckCategory::Oplog,
                severity: FsckSeverity::Error,
                message: format!("cycle detected in oplog parent chain ({child} -> {parent})"),
                path: entry_path(layout, child)
                    .ok()
                    .map(|p| p.display().to_string()),
                op_id: Some(child.clone()),
                repairable: true,
                repaired,
                action,
            });
        }
    }

    // Check `inverse_of` and `undone_ops` consistency.
    for op_id in &all_ids {
        let entry_snapshot = valid_entries[op_id].clone();
        if let Some(ref inv) = entry_snapshot.inverse_of {
            let missing = !valid_entries.contains_key(inv);
            let wrong_redo_target = entry_snapshot.kind == OpKind::Redo
                && valid_entries
                    .get(inv)
                    .is_some_and(|t| t.kind != OpKind::Undo);
            if missing || wrong_redo_target {
                let mut repaired = false;
                let mut action = None;
                if repair && let Some(e) = valid_entries.get_mut(op_id) {
                    e.inverse_of = None;
                    dirty_entries.insert(op_id.clone());
                    repaired = true;
                    let msg = format!("cleared broken inverse_of reference '{inv}' on {op_id}");
                    repaired_actions.push(msg.clone());
                    action = Some(msg);
                }
                issues.push(FsckIssue {
                    code: if wrong_redo_target {
                        "invalid_redo_target".to_string()
                    } else {
                        "dangling_undo_reference".to_string()
                    },
                    category: FsckCategory::Oplog,
                    severity: FsckSeverity::Error,
                    message: if wrong_redo_target {
                        format!("redo entry '{op_id}' targets non-undo op '{inv}'")
                    } else {
                        format!("oplog entry '{op_id}' inverse_of references missing op '{inv}'")
                    },
                    path: entry_path(layout, op_id)
                        .ok()
                        .map(|p| p.display().to_string()),
                    op_id: Some(op_id.clone()),
                    repairable: true,
                    repaired,
                    action,
                });
            }
        }

        let missing_undone: Vec<String> = entry_snapshot
            .undone_ops
            .iter()
            .filter(|u| !valid_entries.contains_key(u.as_str()))
            .cloned()
            .collect();
        if !missing_undone.is_empty() {
            let mut repaired = false;
            let mut action = None;
            if repair && let Some(e) = valid_entries.get_mut(op_id) {
                e.undone_ops.retain(|u| !missing_undone.contains(u));
                dirty_entries.insert(op_id.clone());
                repaired = true;
                let msg = format!(
                    "pruned missing undone_ops {} from {op_id}",
                    missing_undone.join(", ")
                );
                repaired_actions.push(msg.clone());
                action = Some(msg);
            }
            issues.push(FsckIssue {
                code: "dangling_undo_reference".to_string(),
                category: FsckCategory::Oplog,
                severity: FsckSeverity::Error,
                message: format!(
                    "oplog entry '{op_id}' undone_ops references missing op(s): {}",
                    missing_undone.join(", ")
                ),
                path: entry_path(layout, op_id)
                    .ok()
                    .map(|p| p.display().to_string()),
                op_id: Some(op_id.clone()),
                repairable: true,
                repaired,
                action,
            });
        }
    }

    // Check reachability from `oplog/HEAD`.
    if let Some(ref hid) = head_op_id {
        let mut reachable = reachable_from_head(&valid_entries, hid);
        let unlinked: Vec<String> = valid_entries
            .keys()
            .filter(|id| !reachable.contains(id.as_str()))
            .cloned()
            .collect();

        if !unlinked.is_empty() {
            // Find unlinked tips (unlinked nodes that are not parents of any other unlinked node).
            let unlinked_set: HashSet<&str> = unlinked.iter().map(String::as_str).collect();
            let mut has_unlinked_child: HashSet<&str> = HashSet::new();
            for uid in &unlinked {
                for p in &valid_entries[uid].parents {
                    if unlinked_set.contains(p.as_str()) {
                        has_unlinked_child.insert(p.as_str());
                    }
                }
            }
            let mut unlinked_tips: Vec<String> = unlinked
                .iter()
                .filter(|uid| !has_unlinked_child.contains(uid.as_str()))
                .cloned()
                .collect();
            unlinked_tips.sort_by(|a, b| {
                valid_entries[b]
                    .timestamp
                    .cmp(&valid_entries[a].timestamp)
                    .then_with(|| b.cmp(a))
            });

            let mut current_head = hid.clone();
            let mut advanced_head_to: Option<String> = None;
            let mut attached_parents: Vec<String> = Vec::new();

            if repair {
                let children = build_reverse_children_map(&valid_entries);
                let mut can_reach_head =
                    entries_reaching_target(&valid_entries, &children, &current_head);

                // If the newest unlinked tip already descends from current HEAD, `HEAD` lagged behind.
                if let Some(newest_tip) = unlinked_tips.first()
                    && can_reach_head.contains(newest_tip.as_str())
                {
                    current_head.clone_from(newest_tip);
                    write_head_op_id(layout, Some(&current_head))?;
                    advanced_head_to = Some(current_head.clone());
                    reachable = reachable_from_head(&valid_entries, &current_head);
                    can_reach_head =
                        entries_reaching_target(&valid_entries, &children, &current_head);
                }

                // Link any remaining unlinked tips as parents on `current_head` (skipping any that
                // already reach `current_head` to preserve acyclicity).
                let mut remaining_tips: Vec<String> = unlinked_tips
                    .into_iter()
                    .filter(|t| {
                        !reachable.contains(t.as_str())
                            && t != &current_head
                            && !can_reach_head.contains(t.as_str())
                    })
                    .collect();
                remaining_tips.sort();
                if !remaining_tips.is_empty()
                    && let Some(head_entry) = valid_entries.get_mut(&current_head)
                {
                    for tip in &remaining_tips {
                        if !head_entry.parents.contains(tip) {
                            head_entry.parents.push(tip.clone());
                            attached_parents.push(tip.clone());
                        }
                    }
                    dirty_entries.insert(current_head.clone());
                }
            }

            for uid in &unlinked {
                let mut repaired = false;
                let mut action = None;
                if repair {
                    repaired = true;
                    let msg = if let Some(ref new_h) = advanced_head_to {
                        format!("advanced lagging oplog/HEAD -> {new_h} (recovering {uid})")
                    } else {
                        format!(
                            "linked orphaned oplog entry {uid} into HEAD ({current_head}) parent chain via [{}]",
                            attached_parents.join(", ")
                        )
                    };
                    if !repaired_actions.contains(&msg) {
                        repaired_actions.push(msg.clone());
                    }
                    action = Some(msg);
                }
                issues.push(FsckIssue {
                    code: "unlinked_oplog_entry".to_string(),
                    category: FsckCategory::Oplog,
                    severity: FsckSeverity::Warning,
                    message: format!(
                        "oplog entry '{uid}' is not reachable from oplog/HEAD '{hid}'"
                    ),
                    path: entry_path(layout, uid)
                        .ok()
                        .map(|p| p.display().to_string()),
                    op_id: Some(uid.clone()),
                    repairable: true,
                    repaired,
                    action,
                });
            }
        }
    }

    if repair {
        for op_id in dirty_entries {
            if let Some(entry) = valid_entries.get(&op_id) {
                let p = entry_path(layout, &op_id)?;
                let json = serde_json::to_vec_pretty(entry)?;
                atomic_write_bytes(&p, &json)?;
            }
        }
    }

    Ok((valid_entries.len(), valid_entries))
}

fn newest_entry_id(entries: &BTreeMap<String, OpLogEntry>) -> Option<String> {
    entries
        .values()
        .max_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then_with(|| a.op_id.cmp(&b.op_id))
        })
        .map(|e| e.op_id.clone())
}

fn reachable_from_head(entries: &BTreeMap<String, OpLogEntry>, head_id: &str) -> HashSet<String> {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    if entries.contains_key(head_id) {
        visited.insert(head_id.to_string());
        queue.push_back(head_id.to_string());
    }
    while let Some(id) = queue.pop_front() {
        if let Some(entry) = entries.get(&id) {
            for p in &entry.parents {
                if entries.contains_key(p) && visited.insert(p.clone()) {
                    queue.push_back(p.clone());
                }
            }
        }
    }
    visited
}

fn build_reverse_children_map(
    entries: &BTreeMap<String, OpLogEntry>,
) -> HashMap<&str, Vec<&str>> {
    let mut children: HashMap<&str, Vec<&str>> = HashMap::with_capacity(entries.len());
    for (child_id, entry) in entries {
        for parent_id in &entry.parents {
            if let Some((parent_key, _)) = entries.get_key_value(parent_id.as_str()) {
                children
                    .entry(parent_key.as_str())
                    .or_default()
                    .push(child_id.as_str());
            }
        }
    }
    children
}

fn entries_reaching_target<'a>(
    entries: &'a BTreeMap<String, OpLogEntry>,
    children: &HashMap<&'a str, Vec<&'a str>>,
    target_id: &str,
) -> HashSet<&'a str> {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    if let Some((target_key, _)) = entries.get_key_value(target_id) {
        visited.insert(target_key.as_str());
        queue.push_back(target_key.as_str());
    }
    while let Some(node) = queue.pop_front() {
        if let Some(child_list) = children.get(node) {
            for &child in child_list {
                if visited.insert(child) {
                    queue.push_back(child);
                }
            }
        }
    }
    visited
}

fn find_cycle_edges(entries: &BTreeMap<String, OpLogEntry>) -> Vec<(String, String)> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color: HashMap<&str, Color> =
        entries.keys().map(|k| (k.as_str(), Color::White)).collect();
    let mut edges = Vec::new();

    for start in entries.keys() {
        if color.get(start.as_str()) != Some(&Color::White) {
            continue;
        }
        let mut stack: Vec<(&str, usize)> = vec![(start.as_str(), 0)];
        color.insert(start.as_str(), Color::Gray);
        while let Some((node, idx)) = stack.last_mut() {
            let parents = &entries[*node].parents;
            if *idx < parents.len() {
                let p = parents[*idx].as_str();
                *idx += 1;
                if !entries.contains_key(p) {
                    continue;
                }
                match color.get(p).copied().unwrap_or(Color::White) {
                    Color::Gray => {
                        edges.push(((*node).to_string(), p.to_string()));
                    }
                    Color::White => {
                        color.insert(p, Color::Gray);
                        stack.push((p, 0));
                    }
                    Color::Black => {}
                }
            } else {
                color.insert(*node, Color::Black);
                stack.pop();
            }
        }
    }
    edges
}

fn check_and_repair_snapshots(
    gix_repo_opt: Option<&gix::Repository>,
    valid_entries: &BTreeMap<String, OpLogEntry>,
    repair: bool,
    issues: &mut Vec<FsckIssue>,
    repaired_actions: &mut Vec<String>,
) -> Result<usize> {
    let Some(gix_repo) = gix_repo_opt else {
        return Ok(0);
    };

    let mut checked_oids: HashSet<String> = HashSet::new();

    for entry in valid_entries.values() {
        for (ws_key, snap) in &entry.ws_snapshots {
            let tree_fields = [
                ("base_tree", Some(snap.base_tree.as_str())),
                ("dirty_tree", snap.dirty_tree.as_deref()),
                ("pre_base_tree", snap.pre_base_tree.as_deref()),
                ("pre_dirty_tree", snap.pre_dirty_tree.as_deref()),
            ];
            for (field, oid_opt) in tree_fields {
                if let Some(hex) = oid_opt {
                    checked_oids.insert(hex.to_string());
                    verify_tree_exists(gix_repo, hex, &entry.op_id, field, issues);
                }
            }

            for (field, oid_opt) in [
                ("head_commit", snap.head_commit.as_deref()),
                ("pre_head_commit", snap.pre_head_commit.as_deref()),
            ] {
                if let Some(hex) = oid_opt {
                    checked_oids.insert(hex.to_string());
                    verify_commit_exists(gix_repo, hex, &entry.op_id, field, issues);
                }
            }

            for df in snap.dirty_files.iter().chain(snap.pre_dirty_files.iter()) {
                if let DirtyFileState::Present { blob_oid, .. } = &df.state {
                    checked_oids.insert(blob_oid.clone());
                    verify_blob_exists(gix_repo, blob_oid, &entry.op_id, &df.path, issues);
                }
            }

            if entry.kind == OpKind::Checkpoint
                && let Some(ref dirty_hex) = snap.dirty_tree
                && let Ok(oid) = ObjectId::from_hex(dirty_hex.as_bytes())
                && (oid.is_empty_tree() || gix_repo.find_tree(oid).is_ok())
            {
                let ref_name = format!("refs/ozone/op-checkpoints/{ws_key}/{}", entry.op_id);
                if gix_repo
                    .try_find_reference(&ref_name)
                    .ok()
                    .flatten()
                    .is_none()
                {
                    let mut repaired = false;
                    let mut action = None;
                    if repair
                        && let Ok(full) = FullName::try_from(ref_name.as_str())
                        && gix_repo
                            .edit_reference(RefEdit::update(
                                full,
                                oid,
                                PreviousValue::MustNotExist,
                                "oz fsck --repair",
                            ))
                            .is_ok()
                    {
                        repaired = true;
                        let msg =
                            format!("restored missing checkpoint ref {ref_name} -> {dirty_hex}");
                        repaired_actions.push(msg.clone());
                        action = Some(msg);
                    }
                    issues.push(FsckIssue {
                        code: "missing_checkpoint_ref".to_string(),
                        category: FsckCategory::Snapshot,
                        severity: FsckSeverity::Warning,
                        message: format!(
                            "checkpoint op '{}' missing ref '{ref_name}'",
                            entry.op_id
                        ),
                        path: Some(ref_name),
                        op_id: Some(entry.op_id.clone()),
                        repairable: true,
                        repaired,
                        action,
                    });
                }
            }
        }
    }

    // Check `refs/ozone/*` for dangling objects and orphaned `op-checkpoints`.
    if let Ok(platform) = gix_repo.references()
        && let Ok(prefixed) = platform.prefixed("refs/ozone/")
    {
        let mut refs_to_delete = Vec::new();
        for item in prefixed {
            let Ok(mut reference) = item else {
                continue;
            };
            let full_name = reference.name().as_bstr().to_str_lossy().into_owned();
            let obj_ok = reference.peel_to_id().is_ok_and(|id| {
                let oid = id.detach();
                oid.is_empty_tree() || gix_repo.find_object(oid).is_ok()
            });

            if !obj_ok {
                let mut repaired = false;
                let mut action = None;
                if repair {
                    refs_to_delete.push(full_name.clone());
                    repaired = true;
                    let msg = format!("deleted dangling ozone ref {full_name}");
                    repaired_actions.push(msg.clone());
                    action = Some(msg);
                }
                issues.push(FsckIssue {
                    code: "dangling_checkpoint_ref".to_string(),
                    category: FsckCategory::Snapshot,
                    severity: FsckSeverity::Error,
                    message: format!("ozone ref '{full_name}' points to missing Git object"),
                    path: Some(full_name),
                    op_id: None,
                    repairable: true,
                    repaired,
                    action,
                });
                continue;
            }

            if full_name.starts_with("refs/ozone/op-checkpoints/")
                && let Some(op_id_str) = full_name.rsplit('/').next().map(str::to_string)
                && !valid_entries.contains_key(&op_id_str)
            {
                let mut repaired = false;
                let mut action = None;
                if repair {
                    refs_to_delete.push(full_name.clone());
                    repaired = true;
                    let msg = format!("deleted orphaned op-checkpoint ref {full_name}");
                    repaired_actions.push(msg.clone());
                    action = Some(msg);
                }
                issues.push(FsckIssue {
                    code: "orphaned_checkpoint_ref".to_string(),
                    category: FsckCategory::Snapshot,
                    severity: FsckSeverity::Warning,
                    message: format!(
                        "orphaned op-checkpoint ref '{full_name}' has no matching OpLog entry"
                    ),
                    path: Some(full_name),
                    op_id: Some(op_id_str),
                    repairable: true,
                    repaired,
                    action,
                });
            }
        }

        if repair && !refs_to_delete.is_empty() {
            let mut edits = Vec::with_capacity(refs_to_delete.len());
            for rname in refs_to_delete {
                if let Ok(full) = FullName::try_from(rname.as_str()) {
                    edits.push(RefEdit {
                        change: Change::Delete {
                            expected: PreviousValue::Any,
                            log: RefLog::AndReference,
                        },
                        name: full,
                        deref: false,
                    });
                }
            }
            if !edits.is_empty() {
                gix_repo
                    .edit_references(edits)
                    .map_err(OzError::git_source)?;
            }
        }
    }

    Ok(checked_oids.len())
}

fn verify_tree_exists(
    gix_repo: &gix::Repository,
    hex: &str,
    op_id: &str,
    field: &str,
    issues: &mut Vec<FsckIssue>,
) {
    let ok = ObjectId::from_hex(hex.as_bytes())
        .is_ok_and(|oid| oid.is_empty_tree() || gix_repo.find_tree(oid).is_ok());
    if !ok {
        issues.push(FsckIssue {
            code: "missing_snapshot_object".to_string(),
            category: FsckCategory::Snapshot,
            severity: FsckSeverity::Error,
            message: format!(
                "oplog entry '{op_id}' {field} tree '{hex}' missing from .git/objects/"
            ),
            path: None,
            op_id: Some(op_id.to_string()),
            repairable: false,
            repaired: false,
            action: None,
        });
    }
}

fn verify_commit_exists(
    gix_repo: &gix::Repository,
    hex: &str,
    op_id: &str,
    field: &str,
    issues: &mut Vec<FsckIssue>,
) {
    let ok = ObjectId::from_hex(hex.as_bytes()).is_ok_and(|oid| gix_repo.find_commit(oid).is_ok());
    if !ok {
        issues.push(FsckIssue {
            code: "missing_snapshot_object".to_string(),
            category: FsckCategory::Snapshot,
            severity: FsckSeverity::Error,
            message: format!(
                "oplog entry '{op_id}' {field} commit '{hex}' missing from .git/objects/"
            ),
            path: None,
            op_id: Some(op_id.to_string()),
            repairable: false,
            repaired: false,
            action: None,
        });
    }
}

fn verify_blob_exists(
    gix_repo: &gix::Repository,
    hex: &str,
    op_id: &str,
    file_path: &str,
    issues: &mut Vec<FsckIssue>,
) {
    let ok = ObjectId::from_hex(hex.as_bytes()).is_ok_and(|oid| gix_repo.find_blob(oid).is_ok());
    if !ok {
        issues.push(FsckIssue {
            code: "missing_snapshot_object".to_string(),
            category: FsckCategory::Snapshot,
            severity: FsckSeverity::Error,
            message: format!(
                "oplog entry '{op_id}' dirty snapshot blob '{hex}' ({file_path}) missing from .git/objects/"
            ),
            path: Some(file_path.to_string()),
            op_id: Some(op_id.to_string()),
            repairable: false,
            repaired: false,
            action: None,
        });
    }
}

fn check_and_repair_mounts(
    repo: &OzoneRepo,
    repair: bool,
    issues: &mut Vec<FsckIssue>,
    repaired_actions: &mut Vec<String>,
) -> Result<(usize, HashSet<String>)> {
    let layout = &repo.layout;
    let mut mounts_checked = 0usize;
    let mut active_baseline_ids = HashSet::new();
    let mut known_state_names = HashSet::new();

    // 1. Check `.git/ozone/state/*.json`.
    let state_dir = layout.state_dir();
    if state_dir.is_dir() {
        for item in fs::read_dir(&state_dir)? {
            let item = item?;
            let path = item.path();
            let name = item.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if name_str.starts_with('.') || !has_json_extension(name_str) || !path.is_file() {
                continue;
            }
            mounts_checked += 1;
            let ws_stem = &name_str[..name_str.len() - ".json".len()];
            match fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<WorkspaceState>(&raw).ok())
            {
                None => {
                    let mut repaired = false;
                    let mut action = None;
                    if repair {
                        let quarantine_dir = state_dir.join(".corrupt");
                        if fs::create_dir_all(&quarantine_dir).is_ok() {
                            let dst = quarantine_dir.join(name_str);
                            if fs::rename(&path, &dst).is_ok() {
                                repaired = true;
                                let msg = format!(
                                    "quarantined corrupt workspace state {} -> {}",
                                    path.display(),
                                    dst.display()
                                );
                                repaired_actions.push(msg.clone());
                                action = Some(msg);
                            }
                        }
                    }
                    issues.push(FsckIssue {
                        code: "corrupt_workspace_state".to_string(),
                        category: FsckCategory::Mount,
                        severity: FsckSeverity::Error,
                        message: format!("corrupt workspace state file '{}'", path.display()),
                        path: Some(path.display().to_string()),
                        op_id: None,
                        repairable: true,
                        repaired,
                        action,
                    });
                }
                Some(state) => {
                    known_state_names.insert(state.name.clone());
                    if let Some(ref bid) = state.baseline_id {
                        active_baseline_ids.insert(bid.clone());
                    }

                    let health = probe_mount_health_os(&state.worktree_path);
                    let mount_json = layout.ws_mount_record_path(&state.name);
                    let upper_dir = layout.ws_upper_dir(&state.name);
                    let has_upper_files = dir_has_user_files(&upper_dir);

                    if health == MountHealth::NotMounted
                        && !state.worktree_path.exists()
                        && !mount_json.is_file()
                    {
                        let mut repaired = false;
                        let mut action = None;
                        let repairable = !has_upper_files;
                        if repair && repairable {
                            let _ = fs::remove_file(&path);
                            let _ = fs::remove_dir_all(layout.ws_meta_dir(&state.name));
                            let wt_git = repo.git_common_dir.join("worktrees").join(&state.name);
                            if wt_git.is_dir() {
                                let _ = fs::remove_dir_all(&wt_git);
                            }
                            known_state_names.remove(&state.name);
                            repaired = true;
                            let msg = format!(
                                "cleaned orphaned workspace state '{}' (worktree '{}' missing)",
                                state.name,
                                state.worktree_path.display()
                            );
                            repaired_actions.push(msg.clone());
                            action = Some(msg);
                        }
                        issues.push(FsckIssue {
                            code: "orphaned_workspace_state".to_string(),
                            category: FsckCategory::Mount,
                            severity: FsckSeverity::Warning,
                            message: if has_upper_files {
                                format!(
                                    "orphaned workspace '{}' missing worktree '{}' (preserving upper/ user files)",
                                    state.name,
                                    state.worktree_path.display()
                                )
                            } else {
                                format!(
                                    "orphaned workspace state '{}' references missing worktree '{}'",
                                    state.name,
                                    state.worktree_path.display()
                                )
                            },
                            path: Some(path.display().to_string()),
                            op_id: None,
                            repairable,
                            repaired,
                            action,
                        });
                    }
                    let _ = ws_stem;
                }
            }
        }
    }

    // 2. Check `.git/ozone/ws/<ws>/`.
    let ws_dir = layout.ws_dir();
    if ws_dir.is_dir() {
        for item in fs::read_dir(&ws_dir)? {
            let item = item?;
            let meta_dir = item.path();
            if !meta_dir.is_dir() {
                continue;
            }
            let Some(ws_name) = meta_dir.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if ws_name.starts_with('.') {
                continue;
            }
            mounts_checked += 1;
            let mount_json = meta_dir.join("mount.json");
            let upper_dir = meta_dir.join("upper");
            let has_state = known_state_names.contains(ws_name)
                || layout.workspace_state_path(ws_name).is_file();

            if !mount_json.is_file() {
                if !has_state {
                    let has_files = dir_has_user_files(&upper_dir);
                    let mut repaired = false;
                    let mut action = None;
                    if repair && !has_files && fs::remove_dir_all(&meta_dir).is_ok() {
                        repaired = true;
                        let msg =
                            format!("removed empty orphaned mount dir {}", meta_dir.display());
                        repaired_actions.push(msg.clone());
                        action = Some(msg);
                    }
                    issues.push(FsckIssue {
                        code: "orphaned_mount_dir".to_string(),
                        category: FsckCategory::Mount,
                        severity: FsckSeverity::Warning,
                        message: if has_files {
                            format!(
                                "orphaned mount dir '{}' without state/mount.json (preserving upper/ files)",
                                meta_dir.display()
                            )
                        } else {
                            format!(
                                "empty orphaned mount directory '{}' without state or mount.json",
                                meta_dir.display()
                            )
                        },
                        path: Some(meta_dir.display().to_string()),
                        op_id: None,
                        repairable: !has_files,
                        repaired,
                        action,
                    });
                }
                continue;
            }

            let parsed = fs::read_to_string(&mount_json)
                .ok()
                .and_then(|raw| serde_json::from_str::<MountRecord>(&raw).ok());
            let Some(mut record) = parsed else {
                let mut repaired = false;
                let mut action = None;
                if repair && fs::remove_file(&mount_json).is_ok() {
                    if !has_state && !dir_has_user_files(&upper_dir) {
                        let _ = fs::remove_dir_all(&meta_dir);
                    }
                    repaired = true;
                    let msg = format!("removed corrupt mount record {}", mount_json.display());
                    repaired_actions.push(msg.clone());
                    action = Some(msg);
                }
                issues.push(FsckIssue {
                    code: "corrupt_mount_record".to_string(),
                    category: FsckCategory::Mount,
                    severity: FsckSeverity::Error,
                    message: format!("corrupt mount.json at '{}'", mount_json.display()),
                    path: Some(mount_json.display().to_string()),
                    op_id: None,
                    repairable: true,
                    repaired,
                    action,
                });
                continue;
            };

            let health = probe_mount_health_os(&record.mount_point);
            match health {
                MountHealth::DisconnectedEnotconn => {
                    let mut repaired = false;
                    let mut action = None;
                    if repair {
                        #[cfg(target_os = "linux")]
                        {
                            let _ = Command::new("fusermount3")
                                .args(["-u", "-z", "--"])
                                .arg(&record.mount_point)
                                .stdin(Stdio::null())
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .status();
                        }
                        record.mounted = false;
                        record.daemon_pid = None;
                        record.updated_at = Utc::now();
                        if let Ok(bytes) = serde_json::to_vec_pretty(&record) {
                            let _ = atomic_write_bytes(&mount_json, &bytes);
                        }
                        repaired = true;
                        let msg = format!(
                            "unmounted dead ENOTCONN FUSE endpoint {} and cleared stale daemon state (upper/ preserved)",
                            record.mount_point.display()
                        );
                        repaired_actions.push(msg.clone());
                        action = Some(msg);
                    }
                    issues.push(FsckIssue {
                        code: "disconnected_mount".to_string(),
                        category: FsckCategory::Mount,
                        severity: FsckSeverity::Error,
                        message: format!(
                            "workspace '{}' mount at '{}' is disconnected (ENOTCONN)",
                            record.workspace,
                            record.mount_point.display()
                        ),
                        path: Some(record.mount_point.display().to_string()),
                        op_id: None,
                        repairable: true,
                        repaired,
                        action,
                    });
                }
                MountHealth::NotMounted => {
                    let has_files = dir_has_user_files(&upper_dir);
                    let dead_pid = record.daemon_pid.is_some_and(|pid| !is_pid_alive(pid));
                    if !has_state {
                        let mut repaired = false;
                        let mut action = None;
                        let repairable = !has_files || record.mounted || dead_pid;
                        if repair {
                            if !has_files && !record.mount_point.exists() {
                                if fs::remove_dir_all(&meta_dir).is_ok() {
                                    let wt_git =
                                        repo.git_common_dir.join("worktrees").join(ws_name);
                                    if wt_git.is_dir() {
                                        let _ = fs::remove_dir_all(&wt_git);
                                    }
                                    repaired = true;
                                    let msg = format!(
                                        "removed orphaned mount record '{}' (preserving no-file workspace)",
                                        meta_dir.display()
                                    );
                                    repaired_actions.push(msg.clone());
                                    action = Some(msg);
                                }
                            } else if record.mounted || record.daemon_pid.is_some() {
                                record.mounted = false;
                                record.daemon_pid = None;
                                record.updated_at = Utc::now();
                                if let Ok(bytes) = serde_json::to_vec_pretty(&record)
                                    && atomic_write_bytes(&mount_json, &bytes).is_ok()
                                {
                                    repaired = true;
                                    let msg = format!(
                                        "marked stale orphaned mount '{}' unmounted (preserving upper/ user files)",
                                        record.workspace
                                    );
                                    repaired_actions.push(msg.clone());
                                    action = Some(msg);
                                }
                            }
                        }
                        issues.push(FsckIssue {
                            code: "orphaned_mount_record".to_string(),
                            category: FsckCategory::Mount,
                            severity: FsckSeverity::Warning,
                            message: format!(
                                "orphaned mount record for '{}' (no workspace state; mounted={})",
                                record.workspace, record.mounted
                            ),
                            path: Some(mount_json.display().to_string()),
                            op_id: None,
                            repairable,
                            repaired,
                            action,
                        });
                    } else if record.mounted || dead_pid {
                        let mut repaired = false;
                        let mut action = None;
                        if repair {
                            record.mounted = false;
                            record.daemon_pid = None;
                            record.updated_at = Utc::now();
                            if let Ok(bytes) = serde_json::to_vec_pretty(&record)
                                && atomic_write_bytes(&mount_json, &bytes).is_ok()
                            {
                                repaired = true;
                                let msg = format!(
                                    "cleared stale mounted/pid flags on unmounted workspace '{}'",
                                    record.workspace
                                );
                                repaired_actions.push(msg.clone());
                                action = Some(msg);
                            }
                        }
                        issues.push(FsckIssue {
                            code: "stale_mount_record".to_string(),
                            category: FsckCategory::Mount,
                            severity: FsckSeverity::Warning,
                            message: format!(
                                "mount record for '{}' marks mounted={} / pid={:?} while mountpoint is not mounted",
                                record.workspace, record.mounted, record.daemon_pid
                            ),
                            path: Some(mount_json.display().to_string()),
                            op_id: None,
                            repairable: true,
                            repaired,
                            action,
                        });
                    }
                }
                MountHealth::Healthy | MountHealth::NotApplicable => {
                    if let Some(pid) = record.daemon_pid
                        && record.backend.requires_daemon()
                        && !is_pid_alive(pid)
                    {
                        let mut repaired = false;
                        let mut action = None;
                        if repair {
                            record.daemon_pid = None;
                            record.mounted = false;
                            record.updated_at = Utc::now();
                            if let Ok(bytes) = serde_json::to_vec_pretty(&record)
                                && atomic_write_bytes(&mount_json, &bytes).is_ok()
                            {
                                repaired = true;
                                let msg = format!(
                                    "cleared dead daemon_pid {pid} from mount.json for '{}'",
                                    record.workspace
                                );
                                repaired_actions.push(msg.clone());
                                action = Some(msg);
                            }
                        }
                        issues.push(FsckIssue {
                            code: "stale_mount_record".to_string(),
                            category: FsckCategory::Mount,
                            severity: FsckSeverity::Warning,
                            message: format!(
                                "mount record for '{}' references dead daemon pid {pid}",
                                record.workspace
                            ),
                            path: Some(mount_json.display().to_string()),
                            op_id: None,
                            repairable: true,
                            repaired,
                            action,
                        });
                    }
                }
            }
        }
    }

    // 3. Check stale `vfsd.sock`.
    if layout.is_initialized() {
        let sock = layout.vfsd_socket_path();
        if fs::symlink_metadata(&sock).is_ok() && is_socket_stale(&sock) {
            let mut repaired = false;
            let mut action = None;
            if repair && fs::remove_file(&sock).is_ok() {
                repaired = true;
                let msg = format!("removed stale oz-vfsd socket {}", sock.display());
                repaired_actions.push(msg.clone());
                action = Some(msg);
            }
            issues.push(FsckIssue {
                code: "stale_vfsd_socket".to_string(),
                category: FsckCategory::Mount,
                severity: FsckSeverity::Warning,
                message: format!("stale unresponsive oz-vfsd socket '{}'", sock.display()),
                path: Some(sock.display().to_string()),
                op_id: None,
                repairable: true,
                repaired,
                action,
            });
        }
    }

    Ok((mounts_checked, active_baseline_ids))
}

fn check_and_repair_baselines(
    repo: &OzoneRepo,
    gix_repo_opt: Option<&gix::Repository>,
    active_baseline_ids: &HashSet<String>,
    repair: bool,
    issues: &mut Vec<FsckIssue>,
    repaired_actions: &mut Vec<String>,
) -> Result<usize> {
    let baselines_dir = repo.layout.baselines_dir();
    if !baselines_dir.is_dir() {
        return Ok(0);
    }

    let mut checked = 0usize;
    for item in fs::read_dir(&baselines_dir)? {
        let item = item?;
        let path = item.path();
        let name = item.file_name();
        let Some(id) = name.to_str() else {
            continue;
        };
        if id.starts_with('.') || !path.is_dir() {
            continue;
        }
        checked += 1;

        let meta_path = path.join("metadata.json");
        let root_dir = path.join("root");
        let parsed_meta = if meta_path.is_file() {
            fs::read_to_string(&meta_path)
                .ok()
                .and_then(|s| serde_json::from_str::<MinimalBaselineMeta>(&s).ok())
        } else {
            None
        };

        let Some(meta) = parsed_meta.filter(|_| root_dir.is_dir()) else {
            let mut repaired = false;
            let mut action = None;
            if repair && fs::remove_dir_all(&path).is_ok() {
                repaired = true;
                let msg = format!(
                    "removed incomplete/corrupt baseline cache {}",
                    path.display()
                );
                repaired_actions.push(msg.clone());
                action = Some(msg);
            }
            issues.push(FsckIssue {
                code: "corrupt_baseline".to_string(),
                category: FsckCategory::Baseline,
                severity: FsckSeverity::Warning,
                message: format!(
                    "baseline '{}' is incomplete or has corrupt metadata.json",
                    path.display()
                ),
                path: Some(path.display().to_string()),
                op_id: None,
                repairable: true,
                repaired,
                action,
            });
            continue;
        };

        let referenced_by_ws =
            active_baseline_ids.contains(id) || active_baseline_ids.contains(&meta.commit_oid);
        let commit_exists = gix_repo_opt.is_none_or(|gr| {
            ObjectId::from_hex(meta.commit_oid.as_bytes())
                .is_ok_and(|oid| gr.find_commit(oid).is_ok())
        });

        if !referenced_by_ws && !commit_exists {
            let mut repaired = false;
            let mut action = None;
            if repair && fs::remove_dir_all(&path).is_ok() {
                repaired = true;
                let msg = format!(
                    "removed orphaned baseline '{}' (commit '{}' not in ODB)",
                    id, meta.commit_oid
                );
                repaired_actions.push(msg.clone());
                action = Some(msg);
            }
            issues.push(FsckIssue {
                code: "orphaned_baseline".to_string(),
                category: FsckCategory::Baseline,
                severity: FsckSeverity::Warning,
                message: format!(
                    "orphaned warm baseline '{}' references missing commit '{}'",
                    id, meta.commit_oid
                ),
                path: Some(path.display().to_string()),
                op_id: None,
                repairable: true,
                repaired,
                action,
            });
        }
    }

    Ok(checked)
}

fn dir_has_user_files(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .any(|e| {
            let ft = e.file_type();
            ft.is_file() || ft.is_symlink()
        })
}

fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(raw_pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        if raw_pid <= 0 {
            return false;
        }
        // SAFETY: `raw_pid` is verified strictly positive (> 0), and signal `0` probes process
        // existence/permissions via `kill(2)` without delivering any signal to the target process.
        let ret = unsafe { libc::kill(raw_pid, 0) };
        ret == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn is_socket_stale(sock: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        match UnixStream::connect(sock) {
            Ok(_) => false,
            Err(err) => matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = sock;
        false
    }
}

fn resolve_real_git() -> Option<PathBuf> {
    if let Some(env_git) = std::env::var_os("OZ_REAL_GIT") {
        let p = PathBuf::from(env_git);
        if p.is_file() {
            return Some(p);
        }
    }

    let self_canon = std::env::current_exe()
        .ok()
        .and_then(|p| fs::canonicalize(p).ok());

    if let Some(path_var) = std::env::var_os("PATH") {
        #[cfg(windows)]
        let cands = ["git.exe", "git.cmd", "git"];
        #[cfg(not(windows))]
        let cands = ["git"];

        for dir in std::env::split_paths(&path_var) {
            for cand in cands {
                let full = dir.join(cand);
                if !full.is_file() {
                    continue;
                }
                let fname = full
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if fname.starts_with("oz") || fname.starts_with("git-ozone") {
                    continue;
                }
                let canon = fs::canonicalize(&full).unwrap_or_else(|_| full.clone());
                if self_canon.as_ref().is_some_and(|s| s == &canon) {
                    continue;
                }
                return Some(full);
            }
        }
    }

    let fallback = Path::new("/usr/bin/git");
    if fallback.is_file() {
        Some(fallback.to_path_buf())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BTreeMap, OpKind, OpLogEntry, Utc, build_reverse_children_map, entries_reaching_target,
        is_pid_alive, reachable_from_head,
    };
    use crate::types::{ActorInfo, ActorKind};

    fn dummy_entry(op_id: &str, parents: &[&str]) -> OpLogEntry {
        OpLogEntry {
            op_id: op_id.to_string(),
            parents: parents.iter().map(|s| (*s).to_string()).collect(),
            timestamp: Utc::now(),
            actor: ActorInfo {
                kind: ActorKind::Agent,
                client: None,
                workspace: None,
            },
            kind: OpKind::Mutate,
            command: "test".to_string(),
            message: None,
            inverse_of: None,
            undone_ops: Vec::new(),
            rewound_uncheckpointed: false,
            ref_deltas: BTreeMap::new(),
            ws_snapshots: BTreeMap::new(),
        }
    }

    #[test]
    fn is_pid_alive_rejects_zero_and_overflow_and_detects_current_process() {
        assert!(!is_pid_alive(0));
        assert!(!is_pid_alive(u32::MAX));
        #[cfg(unix)]
        assert!(is_pid_alive(std::process::id()));
    }

    #[test]
    fn reverse_children_precomputation_matches_reachable_from_head() {
        let mut entries = BTreeMap::new();
        for (id, parents) in [
            ("op_1", &[][..]),
            ("op_2", &["op_1"][..]),
            ("op_3", &["op_2"][..]),
            ("op_4_desc", &["op_3"][..]),
            ("op_5_desc2", &["op_4_desc"][..]),
            ("op_6_branch", &["op_1"][..]),
            ("op_7_island", &[][..]),
        ] {
            entries.insert(id.to_string(), dummy_entry(id, parents));
        }

        let children = build_reverse_children_map(&entries);
        for target in entries.keys() {
            let fast_set = entries_reaching_target(&entries, &children, target);
            for candidate in entries.keys() {
                let slow_reaches = reachable_from_head(&entries, candidate).contains(target);
                assert_eq!(
                    fast_set.contains(candidate.as_str()),
                    slow_reaches,
                    "mismatch for candidate={candidate} reaching target={target}"
                );
            }
        }
    }
}
