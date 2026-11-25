// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Path scope (`--scope`/`--deny`) and content guardrails enforced before integration.

use crate::sandbox::{
    commit_tree_id, merge_base_tree_id, normalize_branch_short, open_gix_with_fallback, parse_oid,
    resolve_local_branch, resolve_main_repo,
};
use gix::bstr::ByteSlice;
use gix::hash::ObjectId;
use gix::object::tree::diff::Change;
use globset::{Glob, GlobSet, GlobSetBuilder};
use oz_core::{
    ChangeKind, GuardrailOptions, GuardrailTarget, OzError, OzoneRepo, Result, WorkspaceState,
    check_guardrails, compute_status,
};
use std::collections::BTreeMap;
use std::fs;
use std::ops::ControlFlow;
use std::path::PathBuf;

/// Options controlling pre-integration guardrail checks.
#[derive(Debug, Clone, Default)]
pub struct IntegrationGuardrailOptions {
    /// Override workspace and repository `--scope` / `--deny` path rules (`--allow-out-of-scope`).
    pub allow_out_of_scope: bool,
    /// Override content guardrails (secrets, conflict markers, oversized blobs) (`--force`).
    pub force: bool,
    /// Target branch (`--into`) against which branch divergence is computed (defaults to workspace base or `"main"`).
    pub into_branch: Option<String>,
}

/// Combined repository + workspace `--scope` and `--deny` glob matcher.
#[derive(Debug, Clone)]
pub(crate) struct CombinedScopePolicy {
    repo_scope: GlobSet,
    has_repo_scope: bool,
    ws_scope: GlobSet,
    has_ws_scope: bool,
    deny: GlobSet,
}

impl CombinedScopePolicy {
    /// Compiles combined scope and deny policies from `repo.config` and an optional `WorkspaceState`.
    pub(crate) fn from_repo_and_workspace(
        repo: &OzoneRepo,
        workspace: Option<&WorkspaceState>,
    ) -> Result<Self> {
        let repo_patterns = &repo.config.guardrails.scope;
        let ws_patterns: &[String] = workspace.map_or(&[], |ws| ws.scope.as_slice());

        let mut deny_patterns = repo.config.guardrails.deny.clone();
        if let Some(ws) = workspace {
            for pat in &ws.deny {
                if !deny_patterns.contains(pat) {
                    deny_patterns.push(pat.clone());
                }
            }
        }

        Ok(Self {
            repo_scope: compile_globset(repo_patterns, "scope")?,
            has_repo_scope: !repo_patterns.is_empty(),
            ws_scope: compile_globset(ws_patterns, "scope")?,
            has_ws_scope: !ws_patterns.is_empty(),
            deny: compile_globset(&deny_patterns, "deny")?,
        })
    }

    /// Returns `true` when `rel_path` violates any scope or deny pattern.
    #[must_use]
    pub(crate) fn is_out_of_scope(&self, rel_path: &str) -> bool {
        self.violation_reason(rel_path).is_some()
    }

    /// Returns a human-readable violation reason when `rel_path` violates scope/deny rules.
    #[must_use]
    pub(crate) fn violation_reason(&self, rel_path: &str) -> Option<String> {
        let norm = normalize_rel_path(rel_path);
        if self.deny.is_match(&norm) {
            return Some(format!("'{norm}' matches deny guardrail pattern"));
        }
        if self.has_repo_scope && !self.repo_scope.is_match(&norm) {
            return Some(format!("'{norm}' is outside repository allowed scope"));
        }
        if self.has_ws_scope && !self.ws_scope.is_match(&norm) {
            return Some(format!("'{norm}' is outside workspace allowed scope"));
        }
        None
    }
}

pub(crate) fn normalize_rel_path(path: &str) -> String {
    let replaced = path.replace('\\', "/");
    replaced
        .strip_prefix("./")
        .unwrap_or(&replaced)
        .trim_start_matches('/')
        .to_string()
}

fn compile_globset(patterns: &[String], label: &str) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for raw in patterns {
        let trimmed = raw.trim().replace('\\', "/");
        let trimmed = trimmed.strip_prefix("./").unwrap_or(&trimmed);
        let trimmed = trimmed.trim_start_matches('/');
        if trimmed.is_empty() {
            continue;
        }
        let expanded = if let Some(dir) = trimmed.strip_suffix('/') {
            format!("{dir}/**")
        } else {
            trimmed.to_string()
        };
        let glob = Glob::new(&expanded).map_err(|err| OzError::InvalidConfig {
            path: PathBuf::from(".ozone.toml"),
            message: format!("invalid {label} glob '{raw}': {err}"),
        })?;
        builder.add(glob);
    }
    builder.build().map_err(|err| OzError::InvalidConfig {
        path: PathBuf::from(".ozone.toml"),
        message: format!("failed to build {label} globset: {err}"),
    })
}

/// Validates all changed paths (committed branch divergence + any dirty worktree files) of
/// `workspace` against combined workspace + repository `--scope`/`--deny` rules (`kind: "out_of_scope"`,
/// exit code `2` unless `opts.allow_out_of_scope == true`) and content guardrails via
/// `oz_core::check_guardrails` (unless `opts.force == true`).
pub fn check_workspace_scope_for_integration(
    repo: &OzoneRepo,
    workspace: &WorkspaceState,
    opts: &IntegrationGuardrailOptions,
) -> Result<()> {
    let main_repo = resolve_main_repo(repo)?;
    let gix_repo = open_gix_with_fallback(&main_repo.repo_root)?;

    let target_branch = opts
        .into_branch
        .as_deref()
        .or(workspace.base_branch.as_deref())
        .unwrap_or("main");
    let target_commit = resolve_local_branch(&gix_repo, target_branch)?.map(|(_, oid)| oid);

    let source_commit = match resolve_local_branch(&gix_repo, &workspace.branch)? {
        Some((_, oid)) => oid,
        None => match workspace.base_commit.as_deref() {
            Some(hex) => parse_oid(hex)?,
            None => {
                // Unborn branch: only inspect dirty worktree changes if any.
                return check_collected_changes(
                    &main_repo,
                    &gix_repo,
                    &workspace.name,
                    target_branch,
                    Some(workspace),
                    BTreeMap::new(),
                    opts,
                );
            }
        },
    };

    check_branch_guardrails_for_integration(
        &main_repo,
        &workspace.name,
        source_commit,
        target_commit,
        Some(workspace),
        opts,
    )
}

/// Validates changed paths and blob contents for `source_commit` (and optional `workspace`)
/// prior to integration into `target_commit`.
pub fn check_branch_guardrails_for_integration(
    repo: &OzoneRepo,
    source_label: &str,
    source_commit: ObjectId,
    target_commit: Option<ObjectId>,
    workspace: Option<&WorkspaceState>,
    opts: &IntegrationGuardrailOptions,
) -> Result<()> {
    let main_repo = resolve_main_repo(repo)?;
    let gix_repo = open_gix_with_fallback(&main_repo.repo_root)?;

    let into_branch = opts
        .into_branch
        .as_deref()
        .map(normalize_branch_short)
        .or_else(|| workspace.and_then(|ws| ws.base_branch.as_deref()))
        .unwrap_or("main");

    let base_tree_id = match target_commit {
        Some(tgt) => merge_base_tree_id(&gix_repo, tgt, source_commit)?,
        None => match workspace.and_then(|ws| ws.base_commit.as_deref()) {
            Some(hex) => commit_tree_id(&gix_repo, parse_oid(hex)?)?,
            None => gix_repo.empty_tree().id().detach(),
        },
    };
    let source_tree_id = commit_tree_id(&gix_repo, source_commit)?;

    let changed = collect_tree_changes_for_guardrails(&gix_repo, base_tree_id, source_tree_id)?;
    check_collected_changes(
        &main_repo,
        &gix_repo,
        source_label,
        into_branch,
        workspace,
        changed,
        opts,
    )
}

#[derive(Debug, Clone)]
enum ChangedPayload {
    Deleted,
    Present {
        kind: ChangeKind,
        oid: Option<ObjectId>,
        disk_path: Option<PathBuf>,
    },
}

fn collect_tree_changes_for_guardrails(
    gix_repo: &gix::Repository,
    base_tree_id: ObjectId,
    source_tree_id: ObjectId,
) -> Result<BTreeMap<String, ChangedPayload>> {
    let mut map = BTreeMap::new();
    if base_tree_id == source_tree_id {
        return Ok(map);
    }

    let base_tree = gix_repo
        .find_tree(base_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let source_tree = gix_repo
        .find_tree(source_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;

    let mut changes = base_tree
        .changes()
        .map_err(|err| OzError::Git(err.to_string()))?;
    changes.options(|o| {
        o.track_rewrites(None);
    });
    changes
        .for_each_to_obtain_tree(&source_tree, |change| {
            match change {
                Change::Addition {
                    location,
                    entry_mode,
                    id,
                    ..
                } => {
                    if !entry_mode.is_tree() && !entry_mode.is_commit() {
                        map.insert(
                            location.to_str_lossy().into_owned(),
                            ChangedPayload::Present {
                                kind: ChangeKind::Added,
                                oid: Some(id.detach()),
                                disk_path: None,
                            },
                        );
                    }
                }
                Change::Modification {
                    location,
                    entry_mode,
                    id,
                    ..
                } => {
                    if !entry_mode.is_tree() && !entry_mode.is_commit() {
                        map.insert(
                            location.to_str_lossy().into_owned(),
                            ChangedPayload::Present {
                                kind: ChangeKind::Modified,
                                oid: Some(id.detach()),
                                disk_path: None,
                            },
                        );
                    }
                }
                Change::Deletion {
                    location,
                    entry_mode,
                    ..
                } => {
                    if !entry_mode.is_tree() && !entry_mode.is_commit() {
                        map.insert(
                            location.to_str_lossy().into_owned(),
                            ChangedPayload::Deleted,
                        );
                    }
                }
                Change::Rewrite {
                    source_location,
                    source_entry_mode,
                    location,
                    entry_mode,
                    id,
                    copy,
                    ..
                } => {
                    if !copy && !source_entry_mode.is_tree() && !source_entry_mode.is_commit() {
                        map.insert(
                            source_location.to_str_lossy().into_owned(),
                            ChangedPayload::Deleted,
                        );
                    }
                    if !entry_mode.is_tree() && !entry_mode.is_commit() {
                        map.insert(
                            location.to_str_lossy().into_owned(),
                            ChangedPayload::Present {
                                kind: if copy {
                                    ChangeKind::Copied
                                } else {
                                    ChangeKind::Renamed
                                },
                                oid: Some(id.detach()),
                                disk_path: None,
                            },
                        );
                    }
                }
            }
            Ok::<_, gix::Error>(ControlFlow::Continue(()))
        })
        .map_err(|err| OzError::Git(err.to_string()))?;

    Ok(map)
}

fn check_collected_changes(
    main_repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    source_label: &str,
    into_branch: &str,
    workspace: Option<&WorkspaceState>,
    mut changed: BTreeMap<String, ChangedPayload>,
    opts: &IntegrationGuardrailOptions,
) -> Result<()> {
    // Also incorporate any live uncommitted working-tree edits if `workspace` is accessible.
    if let Some(ws) = workspace
        && (ws.worktree_path.is_dir() || ws.backend.requires_daemon())
        && let Ok(ws_repo) = OzoneRepo::discover(Some(&ws.worktree_path))
        && let Ok(status) = compute_status(&ws_repo)
    {
        for dirty in status.changes {
            if dirty.kind == ChangeKind::Deleted {
                changed.insert(dirty.path, ChangedPayload::Deleted);
            } else {
                let disk_path = if ws.backend.requires_daemon() {
                    let upper = main_repo.layout.ws_upper_dir(&ws.name).join(&dirty.path);
                    if fs::symlink_metadata(&upper).is_ok() {
                        upper
                    } else {
                        ws.worktree_path.join(&dirty.path)
                    }
                } else {
                    ws.worktree_path.join(&dirty.path)
                };
                changed.insert(
                    dirty.path,
                    ChangedPayload::Present {
                        kind: dirty.kind,
                        oid: None,
                        disk_path: Some(disk_path),
                    },
                );
            }
        }
    }

    if changed.is_empty() {
        return Ok(());
    }

    let scope_policy = CombinedScopePolicy::from_repo_and_workspace(main_repo, workspace)?;

    if !opts.allow_out_of_scope {
        let mut violations = Vec::new();
        for path in changed.keys() {
            if let Some(reason) = scope_policy.violation_reason(path) {
                violations.push(reason);
            }
        }
        if !violations.is_empty() {
            let preview: Vec<&str> = violations.iter().take(5).map(String::as_str).collect();
            let extra = if violations.len() > 5 {
                format!(" (+{} more)", violations.len() - 5)
            } else {
                String::new()
            };
            return Err(OzError::GuardrailViolation {
                kind: "out_of_scope".to_string(),
                message: format!(
                    "integration of '{source_label}' blocked by path guardrails: {}{extra}",
                    preview.join("; ")
                ),
                recovery: vec![
                    format!(
                        "oz integrate {source_label} --into {into_branch} --allow-out-of-scope"
                    ),
                    "Adjust changes inside the workspace to match --scope/--deny rules."
                        .to_string(),
                ],
            });
        }
    }

    if opts.force {
        return Ok(());
    }

    let max_blob_bytes = main_repo.config.max_blob_mb.saturating_mul(1024 * 1024);

    struct OwnedTarget {
        path: String,
        kind: ChangeKind,
        content: Vec<u8>,
        reported_size_bytes: Option<u64>,
    }

    let mut owned = Vec::with_capacity(changed.len());
    for (path, payload) in changed {
        match payload {
            ChangedPayload::Deleted => {
                owned.push(OwnedTarget {
                    path,
                    kind: ChangeKind::Deleted,
                    content: Vec::new(),
                    reported_size_bytes: Some(0),
                });
            }
            ChangedPayload::Present {
                kind,
                oid,
                disk_path,
            } => {
                let (content, size) = if let Some(ref p) = disk_path {
                    match fs::symlink_metadata(p) {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            let target = fs::read_link(p)?;
                            let bytes = target.as_os_str().as_encoded_bytes().to_vec();
                            let len = bytes.len() as u64;
                            (bytes, len)
                        }
                        Ok(meta) => {
                            let len = meta.len();
                            if len > max_blob_bytes {
                                (Vec::new(), len)
                            } else {
                                (fs::read(p)?, len)
                            }
                        }
                        Err(_) => (Vec::new(), 0),
                    }
                } else if let Some(blob_oid) = oid {
                    let header = gix_repo
                        .find_header(blob_oid)
                        .map_err(|err| OzError::Git(err.to_string()))?;
                    let len = header.size();
                    if len > max_blob_bytes {
                        (Vec::new(), len)
                    } else {
                        let blob = gix_repo
                            .find_blob(blob_oid)
                            .map_err(|err| OzError::Git(err.to_string()))?;
                        (blob.data.clone(), len)
                    }
                } else {
                    (Vec::new(), 0)
                };

                owned.push(OwnedTarget {
                    path,
                    kind,
                    content,
                    reported_size_bytes: Some(size),
                });
            }
        }
    }

    let borrowed: Vec<GuardrailTarget<'_>> = owned
        .iter()
        .map(|o| GuardrailTarget {
            path: &o.path,
            kind: o.kind,
            content: &o.content,
            reported_size_bytes: o.reported_size_bytes,
            out_of_scope: false,
            scope_reason: None,
        })
        .collect();

    check_guardrails(
        main_repo,
        &borrowed,
        &GuardrailOptions {
            force: opts.force,
            allow_out_of_scope: true,
        },
    )
}
