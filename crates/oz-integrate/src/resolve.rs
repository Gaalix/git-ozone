// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Conflict resolution (`oz resolve`), continuation (`oz integrate --continue` /
//! `oz stack rebase --continue`), and clean sandbox abort (`oz integrate --abort`).

use crate::merge::{IntegrateOutcome, continue_integration};
use crate::sandbox::{
    IntegrationMode, IntegrationSession, TakeSide, bytes_contain_conflict_markers,
    cleanup_session_and_sandbox, load_session, open_gix_with_fallback, prune_empty_parents,
    resolve_main_repo, save_session, write_entry_to_sandbox,
};
use crate::stack::{StackRebaseOutcome, continue_stack_rebase};
use chrono::Utc;
use oz_core::{OzError, OzoneRepo, Result};
use oz_oplog::ActorInfo;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Options controlling [`resolve_conflict`].
#[derive(Debug, Clone)]
pub struct ResolveOptions {
    /// Target conflict ID (e.g., `"c1"`). Required unless `all == true`.
    pub conflict_id: Option<String>,
    /// Resolve every unresolved conflict with `take`.
    pub all: bool,
    /// Version to apply (`ours`, `theirs`, or `base`).
    pub take: TakeSide,
    /// Automatically invoke `continue_integration` or `continue_stack_rebase` after resolving.
    pub auto_continue: bool,
}

/// Result of automatically continuing a session after conflict resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContinueOutcome {
    Integrate(IntegrateOutcome),
    StackRebase(StackRebaseOutcome),
}

/// Summary returned by [`resolve_conflict`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveOutcome {
    pub mode: IntegrationMode,
    pub resolved_ids: Vec<String>,
    pub remaining_unresolved: Vec<String>,
    pub sandbox_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continued: Option<ContinueOutcome>,
}

/// Summary returned by [`abort_integration`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbortOutcome {
    pub aborted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<IntegrationMode>,
    pub sandbox_removed: bool,
    pub session_removed: bool,
}

/// Returns the active conflict session from `.git/ozone/integrate/session.json`, if any.
pub fn active_session(repo: &OzoneRepo) -> Result<Option<IntegrationSession>> {
    let main_repo = resolve_main_repo(repo)?;
    load_session(&main_repo)
}

/// Resolves one (`conflict_id`) or all (`all: true`) conflicts inside `../<repo>.oz/_integrate`
/// using `--take ours|theirs|base`, updates `.git/ozone/integrate/session.json`, and
/// optionally continues the paused integration or stack rebase.
pub fn resolve_conflict(
    repo: &OzoneRepo,
    opts: &ResolveOptions,
    actor: ActorInfo,
) -> Result<ResolveOutcome> {
    if !opts.all && opts.conflict_id.is_none() {
        return Err(OzError::InvalidUsage {
            message: "must specify a conflict ID (e.g., `c1`) or `--all`".to_string(),
            recovery: vec![
                "oz resolve c1 --take ours".to_string(),
                "oz resolve --all --take ours".to_string(),
            ],
        });
    }

    let main_repo = resolve_main_repo(repo)?;
    let Some(mut session) = load_session(&main_repo)? else {
        return Err(OzError::InvalidUsage {
            message: "no active integration or rebase conflict session found".to_string(),
            recovery: vec!["Run `oz integrate` or `oz stack rebase` first.".to_string()],
        });
    };

    let gix_repo = open_gix_with_fallback(&main_repo.repo_root)?;
    let sandbox_dir = session.sandbox_path.clone();

    let target_indices: Vec<usize> = if opts.all {
        let unresolved: Vec<usize> = session
            .conflicts
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.resolved)
            .map(|(i, _)| i)
            .collect();
        if unresolved.is_empty() {
            (0..session.conflicts.len()).collect()
        } else {
            unresolved
        }
    } else {
        let wanted = opts.conflict_id.as_deref().unwrap_or_default();
        let Some(pos) = session.conflicts.iter().position(|c| c.id == wanted) else {
            let valid_ids: Vec<&str> = session.conflicts.iter().map(|c| c.id.as_str()).collect();
            return Err(OzError::InvalidUsage {
                message: format!(
                    "conflict id '{wanted}' not found in active session (available: {})",
                    valid_ids.join(", ")
                ),
                recovery: valid_ids
                    .iter()
                    .map(|id| format!("oz resolve {id} --take ours"))
                    .collect(),
            });
        };
        vec![pos]
    };

    let mut resolved_ids = Vec::with_capacity(target_indices.len());
    for idx in target_indices {
        let conflict = &mut session.conflicts[idx];
        let side_ref = match opts.take {
            TakeSide::Ours => conflict.ours.as_ref(),
            TakeSide::Theirs => conflict.theirs.as_ref(),
            TakeSide::Base => conflict.base.as_ref(),
        };

        write_entry_to_sandbox(&gix_repo, &sandbox_dir, &conflict.path, side_ref)?;

        for extra in &conflict.extra_paths {
            if extra == &conflict.path {
                continue;
            }
            let extra_path = sandbox_dir.join(extra);
            if let Ok(meta) = fs::symlink_metadata(&extra_path) {
                if meta.is_dir() {
                    fs::remove_dir_all(&extra_path)?;
                } else {
                    fs::remove_file(&extra_path)?;
                }
                prune_empty_parents(&sandbox_dir, &extra_path);
            }
        }

        conflict.resolved = true;
        conflict.chosen = Some(opts.take);
        resolved_ids.push(conflict.id.clone());
    }

    session.updated_at = Utc::now();
    save_session(&main_repo, &session)?;

    let mut remaining_unresolved = Vec::new();
    for c in &session.conflicts {
        if !c.resolved {
            let disk_path = sandbox_dir.join(&c.path);
            let still_marked = match fs::symlink_metadata(&disk_path) {
                Ok(meta) if meta.is_file() => {
                    let bytes = fs::read(&disk_path)?;
                    bytes_contain_conflict_markers(&bytes)
                }
                _ => false,
            };
            if still_marked || !c.has_text_markers {
                remaining_unresolved.push(c.id.clone());
            }
        }
    }

    let continued = if opts.auto_continue {
        Some(continue_session(repo, actor)?)
    } else {
        None
    };

    Ok(ResolveOutcome {
        mode: session.mode,
        resolved_ids,
        remaining_unresolved,
        sandbox_path: sandbox_dir,
        continued,
    })
}

/// Continues whichever conflict session (`Integrate` or `StackRebase`) is currently active.
pub fn continue_session(repo: &OzoneRepo, actor: ActorInfo) -> Result<ContinueOutcome> {
    let main_repo = resolve_main_repo(repo)?;
    let Some(session) = load_session(&main_repo)? else {
        return Err(OzError::InvalidUsage {
            message: "no active integration or stack rebase conflict session to continue"
                .to_string(),
            recovery: vec!["Run `oz integrate` or `oz stack rebase` first.".to_string()],
        });
    };

    match session.mode {
        IntegrationMode::Integrate => Ok(ContinueOutcome::Integrate(continue_integration(
            repo, actor,
        )?)),
        IntegrationMode::StackRebase => Ok(ContinueOutcome::StackRebase(continue_stack_rebase(
            repo, actor,
        )?)),
    }
}

/// Aborts an active `oz integrate` or `oz stack rebase` conflict session, removing
/// `../<repo>.oz/_integrate` and `.git/ozone/integrate/session.json`.
pub fn abort_integration(repo: &OzoneRepo) -> Result<AbortOutcome> {
    let main_repo = resolve_main_repo(repo)?;
    let session = load_session(&main_repo)?;
    let mode = session.as_ref().map(|s| s.mode);
    let sandbox_hint = session.as_ref().map(|s| s.sandbox_path.as_path());

    let (sandbox_removed, session_removed) = cleanup_session_and_sandbox(&main_repo, sandbox_hint)?;

    Ok(AbortOutcome {
        aborted: sandbox_removed || session_removed,
        mode,
        sandbox_removed,
        session_removed,
    })
}
