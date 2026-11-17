// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Stage-free working-tree-first commits and one-shot `--pick` / `--exclude` partial
//! commit synthesis using `gix` in-memory ODB & `tree::Editor`, atomic CAS ref edits,
//! and deterministic `Change-Id: I<40-hex>` trailers.

use crate::diff::{apply_selected_hunks, target_matches_file};
use crate::error::{OzError, Result};
use crate::guardrails::{GuardrailOptions, GuardrailTarget, check_guardrails};
use crate::repo::OzoneRepo;
use crate::status::{
    StatusContext, compute_status_context, read_blob_bytes, read_disk_bytes,
};
use crate::types::{ChangeKind, WorkspaceState};
use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Options controlling [`commit_changes`].
#[derive(Debug, Clone, Default)]
pub struct CommitOptions {
    /// Commit message (subject/body). A deterministic `Change-Id:` trailer is appended
    /// unless one already exists.
    pub message: String,
    /// Optional `--pick` targets (file IDs like `f1`, hunk IDs like `h1a`, or paths).
    pub pick: Vec<String>,
    /// Optional `--exclude` targets (file IDs like `f3`, hunk IDs like `h1b`, or paths).
    pub exclude: Vec<String>,
    /// Override secret, conflict-marker, and oversized-blob guardrails (`--force`).
    pub force: bool,
    /// Override workspace `--scope` and `--deny` path guardrails (`--allow-out-of-scope`).
    pub allow_out_of_scope: bool,
    /// Allow creating a commit even when no file changes are selected.
    pub allow_empty: bool,
    /// Optional override for author/committer name.
    pub author_name: Option<String>,
    /// Optional override for author/committer email.
    pub author_email: Option<String>,
}

impl CommitOptions {
    /// Creates commit options with the given commit message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }

    /// Sets `--pick` targets.
    #[must_use]
    pub fn with_pick<I, S>(mut self, pick: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.pick = pick.into_iter().map(Into::into).collect();
        self
    }

    /// Sets `--exclude` targets.
    #[must_use]
    pub fn with_exclude<I, S>(mut self, exclude: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.exclude = exclude.into_iter().map(Into::into).collect();
        self
    }
}

/// Summary returned by [`commit_changes`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitOutcome {
    pub commit_oid: String,
    pub tree_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub change_id: String,
    pub message: String,
    pub committed_paths: Vec<String>,
    #[serde(default)]
    pub partial_hunk_paths: Vec<String>,
    pub remaining_dirty_count: usize,
}

#[derive(Debug, Clone)]
enum FileSelection {
    Full,
    Hunks(BTreeSet<usize>),
}

enum PreparedEdit {
    Remove {
        path: String,
        out_of_scope: bool,
        scope_reason: Option<String>,
    },
    Upsert {
        path: String,
        kind: ChangeKind,
        entry_kind: EntryKind,
        bytes: Vec<u8>,
        reported_size_bytes: Option<u64>,
        is_full_file: bool,
        out_of_scope: bool,
        scope_reason: Option<String>,
    },
}

enum IndexDelta {
    Remove {
        path: String,
    },
    Upsert {
        path: String,
        blob_id: gix::ObjectId,
        entry_kind: EntryKind,
        is_full_file: bool,
    },
}

/// Executes a stage-free (or `--pick` / `--exclude` partial) commit directly into the
/// repository ODB, runs pre-commit guardrails, advances the active branch ref atomically
/// via `gix` CAS ref edit, updates `.git/index` / `.git/worktrees/<ws>/index`, and clears
/// virtual `pinned_paths` if present (`.git/ozone/state/<ws>.json` or `.git/ozone/pinned_main.json`).
///
/// Unselected hunks and unselected files remain 100% untouched on disk.
pub fn commit_changes(repo: &OzoneRepo, opts: &CommitOptions) -> Result<CommitOutcome> {
    commit_changes_inner(repo, opts, false)
}

/// Executes [`commit_changes`] while ignoring any virtual `pinned` paths (equivalent to `git commit -a`),
/// committing all working-tree changes (subject to `opts.pick` / `opts.exclude`) and clearing any pinned set.
pub fn commit_changes_ignoring_pinned(
    repo: &OzoneRepo,
    opts: &CommitOptions,
) -> Result<CommitOutcome> {
    commit_changes_inner(repo, opts, true)
}

fn commit_changes_inner(
    repo: &OzoneRepo,
    opts: &CommitOptions,
    ignore_pinned: bool,
) -> Result<CommitOutcome> {
    if opts.message.trim().is_empty() {
        return Err(OzError::InvalidUsage {
            message: "commit message cannot be empty".to_string(),
            recovery: vec!["Provide a non-empty message with `-m \"...\"`.".to_string()],
        });
    }

    let ctx = compute_status_context(repo)?;
    let mut gix_repo = open_gix_for_commit(repo)?;

    if ctx.changes.is_empty() && !opts.allow_empty {
        return Err(OzError::InvalidUsage {
            message: "nothing to commit; working tree is clean".to_string(),
            recovery: vec!["Edit files in the workspace or pass `--allow-empty`.".to_string()],
        });
    }

    let selection = if ctx.changes.is_empty() && opts.allow_empty {
        BTreeMap::new()
    } else {
        resolve_selection(repo, &ctx, &opts.pick, &opts.exclude, ignore_pinned)?
    };

    if selection.is_empty() && !opts.allow_empty {
        return Err(OzError::InvalidUsage {
            message: "no changes selected to commit".to_string(),
            recovery: vec!["Run `oz status` to inspect file and hunk IDs.".to_string()],
        });
    }

    let mut prepared_edits = Vec::with_capacity(selection.len());
    let mut partial_hunk_paths = Vec::new();
    let max_blob_bytes = repo.config.max_blob_mb.saturating_mul(1024 * 1024);

    for (file_idx, sel) in &selection {
        let change = &ctx.changes[*file_idx];
        let raw = &ctx.dirty_files[*file_idx];
        let out_of_scope = change.out_of_scope;
        let scope_reason = ctx.scope_policy.violation_reason(&change.path);

        match sel {
            FileSelection::Full => {
                if raw.kind == ChangeKind::Deleted {
                    prepared_edits.push(PreparedEdit::Remove {
                        path: raw.path.clone(),
                        out_of_scope,
                        scope_reason,
                    });
                } else {
                    let disk_path = ctx.source.disk_path(&raw.path);
                    let meta = fs::symlink_metadata(&disk_path)?;
                    let file_len = meta.len();
                    let entry_kind = raw.disk_kind.unwrap_or(EntryKind::Blob);
                    let bytes = if !opts.force
                        && file_len > max_blob_bytes
                        && entry_kind != EntryKind::Link
                    {
                        Vec::new()
                    } else {
                        read_disk_bytes(&disk_path, Some(entry_kind))?
                    };
                    prepared_edits.push(PreparedEdit::Upsert {
                        path: raw.path.clone(),
                        kind: raw.kind,
                        entry_kind,
                        bytes,
                        reported_size_bytes: Some(file_len),
                        is_full_file: true,
                        out_of_scope,
                        scope_reason,
                    });
                }
            }
            FileSelection::Hunks(selected_hunk_indices) => {
                let all_selected = selected_hunk_indices.len() == change.hunks.len()
                    && !change.hunks.is_empty();

                if all_selected && raw.kind != ChangeKind::Deleted {
                    let disk_path = ctx.source.disk_path(&raw.path);
                    let meta = fs::symlink_metadata(&disk_path)?;
                    let file_len = meta.len();
                    let entry_kind = raw.disk_kind.unwrap_or(EntryKind::Blob);
                    let bytes = if !opts.force
                        && file_len > max_blob_bytes
                        && entry_kind != EntryKind::Link
                    {
                        Vec::new()
                    } else {
                        read_disk_bytes(&disk_path, Some(entry_kind))?
                    };
                    prepared_edits.push(PreparedEdit::Upsert {
                        path: raw.path.clone(),
                        kind: raw.kind,
                        entry_kind,
                        bytes,
                        reported_size_bytes: Some(file_len),
                        is_full_file: true,
                        out_of_scope,
                        scope_reason,
                    });
                } else if raw.kind == ChangeKind::Deleted {
                    prepared_edits.push(PreparedEdit::Remove {
                        path: raw.path.clone(),
                        out_of_scope,
                        scope_reason,
                    });
                } else {
                    let before_bytes = match raw.head_entry {
                        Some(ref he) => read_blob_bytes(&gix_repo, he.oid)?,
                        None => Vec::new(),
                    };
                    let disk_path = ctx.source.disk_path(&raw.path);
                    let after_bytes = read_disk_bytes(&disk_path, raw.disk_kind)?;
                    let partial_bytes = apply_selected_hunks(
                        &raw.path,
                        &before_bytes,
                        &after_bytes,
                        selected_hunk_indices,
                    )?;
                    let entry_kind = raw
                        .head_entry
                        .as_ref()
                        .map(|h| h.kind)
                        .or(raw.disk_kind)
                        .unwrap_or(EntryKind::Blob);

                    partial_hunk_paths.push(raw.path.clone());
                    prepared_edits.push(PreparedEdit::Upsert {
                        path: raw.path.clone(),
                        kind: raw.kind,
                        entry_kind,
                        reported_size_bytes: Some(partial_bytes.len() as u64),
                        bytes: partial_bytes,
                        is_full_file: false,
                        out_of_scope,
                        scope_reason,
                    });
                }
            }
        }
    }

    // Run pre-commit guardrails BEFORE writing any blobs, trees, commits, or ref updates.
    let guardrail_targets: Vec<GuardrailTarget<'_>> = prepared_edits
        .iter()
        .map(|edit| match edit {
            PreparedEdit::Remove {
                path,
                out_of_scope,
                scope_reason,
            } => GuardrailTarget {
                path,
                kind: ChangeKind::Deleted,
                content: &[],
                reported_size_bytes: Some(0),
                out_of_scope: *out_of_scope,
                scope_reason: scope_reason.clone(),
            },
            PreparedEdit::Upsert {
                path,
                kind,
                bytes,
                reported_size_bytes,
                out_of_scope,
                scope_reason,
                ..
            } => GuardrailTarget {
                path,
                kind: *kind,
                content: bytes,
                reported_size_bytes: *reported_size_bytes,
                out_of_scope: *out_of_scope,
                scope_reason: scope_reason.clone(),
            },
        })
        .collect();

    check_guardrails(
        repo,
        &guardrail_targets,
        &GuardrailOptions {
            force: opts.force,
            allow_out_of_scope: opts.allow_out_of_scope,
        },
    )?;

    // Build the new tree in memory via gix `tree::Editor`.
    // Apply removals before upserts to cleanly handle file <-> directory replacements.
    let mut editor = gix_repo
        .edit_tree(ctx.head.tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;

    let mut index_deltas = Vec::with_capacity(prepared_edits.len());
    let mut committed_paths = Vec::with_capacity(prepared_edits.len());

    for edit in &prepared_edits {
        if let PreparedEdit::Remove { path, .. } = edit {
            editor
                .remove(path.as_str())
                .map_err(|err| OzError::Git(err.to_string()))?;
            committed_paths.push(path.clone());
            index_deltas.push(IndexDelta::Remove { path: path.clone() });
        }
    }

    for edit in &prepared_edits {
        if let PreparedEdit::Upsert {
            path,
            entry_kind,
            bytes,
            is_full_file,
            ..
        } = edit
        {
            let blob_id = gix_repo
                .write_blob(bytes)
                .map_err(|err| OzError::Git(err.to_string()))?
                .detach();
            editor
                .upsert(path.as_str(), *entry_kind, blob_id)
                .map_err(|err| OzError::Git(err.to_string()))?;
            committed_paths.push(path.clone());
            index_deltas.push(IndexDelta::Upsert {
                path: path.clone(),
                blob_id,
                entry_kind: *entry_kind,
                is_full_file: *is_full_file,
            });
        }
    }

    committed_paths.sort();

    let new_tree_id = editor
        .write()
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();

    let (author_sig, committer_sig) = resolve_signatures(&mut gix_repo, opts)?;
    let parents: Vec<gix::ObjectId> = ctx.head.commit_oid.into_iter().collect();

    let (final_message, change_id) = ensure_change_id_trailer(
        &opts.message,
        new_tree_id,
        &parents,
        &author_sig,
    );

    let target_ref = if repo.workspace.is_some() && !repo.is_linked_worktree {
        if let Some(ref branch) = ctx.head.branch {
            format!("refs/heads/{branch}")
        } else {
            return Err(OzError::InvalidUsage {
                message: "cannot advance detached HEAD in non-linked workspace without branch"
                    .to_string(),
                recovery: vec!["Check out or spawn a named workspace branch.".to_string()],
            });
        }
    } else {
        "HEAD".to_string()
    };

    let mut author_buf = gix::date::parse::TimeBuf::default();
    let mut committer_buf = gix::date::parse::TimeBuf::default();
    let author_ref = author_sig.to_ref(&mut author_buf);
    let committer_ref = committer_sig.to_ref(&mut committer_buf);

    let commit_id = gix_repo
        .commit_as(
            committer_ref,
            author_ref,
            target_ref.as_str(),
            &final_message,
            new_tree_id,
            parents,
        )
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();

    // Update `.git/index` / `.git/worktrees/<ws>/index` if applicable.
    update_git_index(repo, &gix_repo, &ctx, &index_deltas)?;

    // Clear virtual `pinned_paths` (`.git/ozone/state/<ws>.json` or `.git/ozone/pinned_main.json`).
    let clear_all_pinned = (opts.pick.is_empty() && opts.exclude.is_empty()) || ignore_pinned;
    clear_pinned_paths_if_present(repo, &committed_paths, &partial_hunk_paths, clear_all_pinned)?;

    let remaining_dirty_count =
        (ctx.changes.len() - selection.len()) + partial_hunk_paths.len();

    Ok(CommitOutcome {
        commit_oid: commit_id.to_hex().to_string(),
        tree_oid: new_tree_id.to_hex().to_string(),
        parent_oid: ctx.head.commit_oid.map(|id| id.to_hex().to_string()),
        branch: ctx.head.branch,
        change_id,
        message: final_message,
        committed_paths,
        partial_hunk_paths,
        remaining_dirty_count,
    })
}

fn open_gix_for_commit(repo: &OzoneRepo) -> Result<gix::Repository> {
    let target = if repo.is_linked_worktree && repo.git_dir.exists() {
        &repo.git_dir
    } else {
        &repo.repo_root
    };
    gix::open(target).map_err(|err| OzError::Git(err.to_string()))
}

fn resolve_selection(
    repo: &OzoneRepo,
    ctx: &StatusContext,
    pick: &[String],
    exclude: &[String],
    ignore_pinned: bool,
) -> Result<BTreeMap<usize, FileSelection>> {
    let mut selected: BTreeMap<usize, FileSelection> = BTreeMap::new();
    let pinned_paths = if pick.is_empty() && exclude.is_empty() && !ignore_pinned {
        read_pinned_paths(repo)?
    } else {
        BTreeSet::new()
    };

    if !pick.is_empty() {
        for raw_target in pick {
            let mut matched = false;
            for (file_idx, change) in ctx.changes.iter().enumerate() {
                if target_matches_file(raw_target, change) {
                    selected.insert(file_idx, FileSelection::Full);
                    matched = true;
                    continue;
                }
                if let Some(hunk_idx) = change.hunks.iter().position(|h| h.id == *raw_target) {
                    matched = true;
                    match selected.get_mut(&file_idx) {
                        Some(FileSelection::Full) => {}
                        Some(FileSelection::Hunks(set)) => {
                            set.insert(hunk_idx);
                        }
                        None => {
                            let mut set = BTreeSet::new();
                            set.insert(hunk_idx);
                            selected.insert(file_idx, FileSelection::Hunks(set));
                        }
                    }
                }
            }
            if !matched {
                return Err(OzError::InvalidUsage {
                    message: format!("--pick target '{raw_target}' did not match any dirty file or hunk"),
                    recovery: vec!["Run `oz status` or `oz diff` to list valid file and hunk IDs.".to_string()],
                });
            }
        }
    } else if !ignore_pinned && exclude.is_empty() && !pinned_paths.is_empty() {
        for (file_idx, change) in ctx.changes.iter().enumerate() {
            if pinned_paths.contains(&change.path) {
                selected.insert(file_idx, FileSelection::Full);
            }
        }
        if selected.is_empty() {
            return Err(OzError::InvalidUsage {
                message: "none of the pinned paths have uncommitted changes".to_string(),
                recovery: vec![
                    "Run `oz status` or pass `--pick` / `oz git commit -a` explicitly.".to_string(),
                ],
            });
        }
    } else {
        for file_idx in 0..ctx.changes.len() {
            selected.insert(file_idx, FileSelection::Full);
        }
    }

    if !exclude.is_empty() {
        for raw_target in exclude {
            let mut matched_any_in_status = false;
            for (file_idx, change) in ctx.changes.iter().enumerate() {
                if target_matches_file(raw_target, change) {
                    matched_any_in_status = true;
                    selected.remove(&file_idx);
                    continue;
                }
                if let Some(hunk_idx) = change.hunks.iter().position(|h| h.id == *raw_target) {
                    matched_any_in_status = true;
                    let mut remove_file = false;
                    if let Some(sel) = selected.get_mut(&file_idx) {
                        match sel {
                            FileSelection::Full => {
                                let mut set: BTreeSet<usize> = (0..change.hunks.len()).collect();
                                set.remove(&hunk_idx);
                                if set.is_empty() {
                                    remove_file = true;
                                } else {
                                    *sel = FileSelection::Hunks(set);
                                }
                            }
                            FileSelection::Hunks(set) => {
                                set.remove(&hunk_idx);
                                if set.is_empty() {
                                    remove_file = true;
                                }
                            }
                        }
                    }
                    if remove_file {
                        selected.remove(&file_idx);
                    }
                }
            }
            if !matched_any_in_status {
                return Err(OzError::InvalidUsage {
                    message: format!("--exclude target '{raw_target}' did not match any dirty file or hunk"),
                    recovery: vec!["Run `oz status` or `oz diff` to list valid file and hunk IDs.".to_string()],
                });
            }
        }
    }

    Ok(selected)
}

fn resolve_signatures(
    gix_repo: &mut gix::Repository,
    opts: &CommitOptions,
) -> Result<(gix::actor::Signature, gix::actor::Signature)> {
    let now = gix::date::Time::now_local_or_utc();

    if let (Some(name), Some(email)) = (&opts.author_name, &opts.author_email) {
        let sig = gix::actor::Signature {
            name: name.as_str().into(),
            email: email.as_str().into(),
            time: now,
        };
        return Ok((sig.clone(), sig));
    }

    let committer = match gix_repo.committer() {
        Some(res) => res
            .map_err(|err| OzError::Git(err.to_string()))?
            .to_owned()
            .map_err(|err| OzError::Git(err.to_string()))?,
        None => gix_repo
            .committer_or_set_fallback("Ozone", "oz@ozone.dev")
            .map_err(|err| OzError::Git(err.to_string()))?
            .to_owned()
            .map_err(|err| OzError::Git(err.to_string()))?,
    };

    let mut author = match gix_repo.author() {
        Some(res) => res
            .map_err(|err| OzError::Git(err.to_string()))?
            .to_owned()
            .map_err(|err| OzError::Git(err.to_string()))?,
        None => committer.clone(),
    };

    if let Some(ref name) = opts.author_name {
        author.name = name.as_str().into();
    }
    if let Some(ref email) = opts.author_email {
        author.email = email.as_str().into();
    }

    Ok((author, committer))
}

/// Computes deterministic `I<40-hex>` `Change-Id` value for commit inputs.
#[must_use]
pub fn compute_change_id(
    tree_id: gix::ObjectId,
    parents: &[gix::ObjectId],
    author: &gix::actor::Signature,
    message: &str,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"oz-change-id-v1\0");
    hasher.update(tree_id.as_bytes());
    for parent in parents {
        hasher.update(parent.as_bytes());
    }
    hasher.update(author.name.as_ref());
    hasher.update(b"\0");
    hasher.update(author.email.as_ref());
    hasher.update(b"\0");
    hasher.update(&author.time.seconds.to_le_bytes());
    hasher.update(b"\0");
    hasher.update(message.trim_end().as_bytes());

    let digest = hasher.finalize();
    let bytes = digest.as_bytes();
    let mut hex = String::with_capacity(41);
    hex.push('I');
    for byte in &bytes[..20] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Ensures the commit message ends with a `Change-Id: I<40-hex>` trailer unless one
/// is already present in the trailing trailer block.
#[must_use]
pub fn ensure_change_id_trailer(
    message: &str,
    tree_id: gix::ObjectId,
    parents: &[gix::ObjectId],
    author: &gix::actor::Signature,
) -> (String, String) {
    let base = message.trim_end_matches(['\r', '\n']);
    if let Some(existing) = find_existing_change_id(base) {
        return (format!("{base}\n"), existing);
    }

    let change_id = compute_change_id(tree_id, parents, author, base);
    let has_trailing_trailer_block = last_paragraph_is_trailers(base);

    let formatted = if has_trailing_trailer_block {
        format!("{base}\nChange-Id: {change_id}\n")
    } else {
        format!("{base}\n\nChange-Id: {change_id}\n")
    };
    (formatted, change_id)
}

fn find_existing_change_id(message: &str) -> Option<String> {
    let last_para = message.rsplit("\n\n").next().unwrap_or(message);
    for line in last_para.lines() {
        let trimmed = line.trim();
        if let Some((key, val)) = trimmed.split_once(':')
            && key.eq_ignore_ascii_case("Change-Id")
        {
            let value = val.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn last_paragraph_is_trailers(message: &str) -> bool {
    // Only treat the final paragraph as a trailer block if there are at least 2 paragraphs
    // (a subject line followed by a blank line and trailer lines).
    if !message.contains("\n\n") {
        return false;
    }
    let Some(last_para) = message.rsplit("\n\n").next() else {
        return false;
    };
    let lines: Vec<&str> = last_para.lines().filter(|l| !l.trim().is_empty()).collect();
    !lines.is_empty() && lines.iter().all(|l| is_trailer_line(l))
}

fn is_trailer_line(line: &str) -> bool {
    let Some((key, _val)) = line.split_once(": ") else {
        return false;
    };
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn update_git_index(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    ctx: &StatusContext,
    deltas: &[IndexDelta],
) -> Result<()> {
    // Do not overwrite main `.git/index` when committing on a workspace branch that does not
    // have its own `.git/worktrees/<ws>` index.
    if repo.workspace.is_some() && !repo.is_linked_worktree {
        return Ok(());
    }
    if deltas.is_empty() {
        return Ok(());
    }

    let index_path = repo.git_dir.join("index");
    let mut index = if index_path.is_file() {
        gix_repo
            .open_index()
            .map_err(|err| OzError::Git(err.to_string()))?
    } else if ctx.head.commit_oid.is_some() {
        gix_repo
            .index_from_tree(&ctx.head.tree_id)
            .map_err(|err| OzError::Git(err.to_string()))?
    } else {
        gix::index::File::from_state(
            gix::index::State::new(gix_repo.object_hash()),
            index_path,
        )
    };

    index.remove_tree();

    let touched: BTreeSet<&str> = deltas
        .iter()
        .map(|d| match d {
            IndexDelta::Remove { path } | IndexDelta::Upsert { path, .. } => path.as_str(),
        })
        .collect();

    index.remove_entries(|_, path, _| touched.contains(path.to_str_lossy().as_ref()));

    for delta in deltas {
        if let IndexDelta::Upsert {
            path,
            blob_id,
            entry_kind,
            is_full_file,
        } = delta
        {
            let stat = if *is_full_file {
                let disk_path = ctx.source.disk_path(path);
                gix::index::fs::Metadata::from_path_no_follow(&disk_path)
                    .ok()
                    .and_then(|meta| gix::index::entry::Stat::from_fs(&meta).ok())
                    .unwrap_or_default()
            } else {
                // Partial hunk commit leaves remaining hunks dirty on disk, so keep stat zeroed.
                gix::index::entry::Stat::default()
            };

            index.dangerously_push_entry(
                stat,
                *blob_id,
                gix::index::entry::Flags::empty(),
                (*entry_kind).into(),
                path.as_bytes().as_bstr(),
            );
        }
    }

    index.sort_entries();
    index
        .write(gix::index::write::Options::default())
        .map_err(|err| OzError::Git(err.to_string()))?;

    Ok(())
}

const PINNED_SCHEMA_VERSION: u32 = 1;

const fn default_pinned_schema_version() -> u32 {
    PINNED_SCHEMA_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PinnedPathsDoc {
    #[serde(default = "default_pinned_schema_version")]
    schema_version: u32,
    #[serde(default)]
    pinned_paths: BTreeSet<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PinnedPathsFile {
    Doc(PinnedPathsDoc),
    Set(BTreeSet<String>),
}

fn normalize_pinned_path(raw: &str) -> String {
    crate::diff::normalize_target_path(raw)
}

fn external_pinned_file_path(repo: &OzoneRepo) -> PathBuf {
    match repo.workspace {
        Some(ref ws_name) => repo
            .layout
            .ozone_dir
            .join(format!("pinned_wt_{ws_name}.json")),
        None => repo.layout.main_pinned_path(),
    }
}

fn read_pinned_doc(path: &Path) -> Result<BTreeSet<String>> {
    if !path.is_file() {
        return Ok(BTreeSet::new());
    }
    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(BTreeSet::new());
    }
    let parsed: PinnedPathsFile = serde_json::from_str(&raw)?;
    let set = match parsed {
        PinnedPathsFile::Doc(doc) => doc.pinned_paths,
        PinnedPathsFile::Set(set) => set,
    };
    Ok(set
        .into_iter()
        .map(|p| normalize_pinned_path(&p))
        .filter(|p| !p.is_empty())
        .collect())
}

/// Reads the virtual `pinned` path set for `repo`.
///
/// Named workspaces store `pinned_paths` inside `.git/ozone/state/<ws>.json`;
/// the main repository stores them at `.git/ozone/pinned_main.json` (`OzoneLayout::main_pinned_path()`)
/// so `.git/ozone/state/` contains strictly `WorkspaceState` files.
pub fn read_pinned_paths(repo: &OzoneRepo) -> Result<BTreeSet<String>> {
    if let Some(ref ws_name) = repo.workspace {
        let state_path = repo.layout.workspace_state_path(ws_name);
        if state_path.is_file() {
            let raw = fs::read_to_string(&state_path)?;
            let state: WorkspaceState = serde_json::from_str(&raw)?;
            return Ok(state
                .pinned_paths
                .into_iter()
                .map(|p| normalize_pinned_path(&p))
                .filter(|p| !p.is_empty())
                .collect());
        }
        let wt_path = external_pinned_file_path(repo);
        if wt_path.is_file() {
            return read_pinned_doc(&wt_path);
        }
        if let Some(ref st) = repo.workspace_state {
            return Ok(st
                .pinned_paths
                .iter()
                .map(|p| normalize_pinned_path(p))
                .filter(|p| !p.is_empty())
                .collect());
        }
        return Ok(BTreeSet::new());
    }

    read_pinned_doc(&repo.layout.main_pinned_path())
}

/// Atomically writes the complete virtual `pinned` path set for `repo`.
pub fn write_pinned_paths(repo: &OzoneRepo, paths: &BTreeSet<String>) -> Result<()> {
    let normalized: BTreeSet<String> = paths
        .iter()
        .map(|p| normalize_pinned_path(p))
        .filter(|p| !p.is_empty())
        .collect();

    if let Some(ref ws_name) = repo.workspace {
        let state_path = repo.layout.workspace_state_path(ws_name);
        if state_path.is_file() {
            let raw = fs::read_to_string(&state_path)?;
            let mut state: WorkspaceState = serde_json::from_str(&raw)?;
            if state.pinned_paths == normalized {
                return Ok(());
            }
            state.pinned_paths = normalized;
            state.updated_at = chrono::Utc::now();
            return write_json_atomic(&state_path, &state);
        }
    }

    let target_path = external_pinned_file_path(repo);
    if normalized.is_empty() && !target_path.is_file() {
        return Ok(());
    }
    repo.layout.ensure_initialized()?;
    let doc = PinnedPathsDoc {
        schema_version: PINNED_SCHEMA_VERSION,
        pinned_paths: normalized,
    };
    write_json_atomic(&target_path, &doc)
}

/// Inserts `paths` into the virtual `pinned` path set for `repo`, returning the updated set.
pub fn add_pinned_paths<I, S>(repo: &OzoneRepo, paths: I) -> Result<BTreeSet<String>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut set = read_pinned_paths(repo)?;
    for p in paths {
        let norm = normalize_pinned_path(p.as_ref());
        if !norm.is_empty() {
            set.insert(norm);
        }
    }
    write_pinned_paths(repo, &set)?;
    Ok(set)
}

/// Clears all paths from the virtual `pinned` path set for `repo`.
pub fn clear_pinned_paths(repo: &OzoneRepo) -> Result<()> {
    write_pinned_paths(repo, &BTreeSet::new())
}

/// Removes specific `paths` from the virtual `pinned` path set for `repo`, returning the remaining set.
pub fn remove_pinned_paths<I, S>(repo: &OzoneRepo, paths: I) -> Result<BTreeSet<String>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut set = read_pinned_paths(repo)?;
    if set.is_empty() {
        return Ok(set);
    }
    let mut changed = false;
    for p in paths {
        let norm = normalize_pinned_path(p.as_ref());
        if set.remove(&norm) {
            changed = true;
        }
    }
    if changed {
        write_pinned_paths(repo, &set)?;
    }
    Ok(set)
}

fn clear_pinned_paths_if_present(
    repo: &OzoneRepo,
    committed_paths: &[String],
    partial_hunk_paths: &[String],
    clear_all_pinned: bool,
) -> Result<()> {
    if let Some(ref ws_name) = repo.workspace {
        let state_path = repo.layout.workspace_state_path(ws_name);
        if state_path.is_file() {
            let raw = fs::read_to_string(&state_path)?;
            let mut state: WorkspaceState = serde_json::from_str(&raw)?;
            if state.pinned_paths.is_empty() && state.dirty_paths.is_empty() {
                return Ok(());
            }

            if clear_all_pinned {
                state.pinned_paths.clear();
            } else {
                for path in committed_paths {
                    state.pinned_paths.remove(path);
                }
            }

            let partial_set: BTreeSet<&str> =
                partial_hunk_paths.iter().map(String::as_str).collect();
            for path in committed_paths {
                if !partial_set.contains(path.as_str()) {
                    state.dirty_paths.remove(path);
                }
            }
            state.updated_at = chrono::Utc::now();

            return write_json_atomic(&state_path, &state);
        }
    }

    if clear_all_pinned {
        clear_pinned_paths(repo)?;
    } else if !committed_paths.is_empty() {
        let _ = remove_pinned_paths(repo, committed_paths)?;
    }
    Ok(())
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("state"),
        ulid::Ulid::generate()
    ));
    let json = serde_json::to_string_pretty(value)?;
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_deterministic_change_id_and_preserves_existing() {
        let sig = gix::actor::Signature {
            name: "Tester".into(),
            email: "tester@ozone.dev".into(),
            time: gix::date::Time::new(1_700_000_000, 0),
        };
        let tree = gix::ObjectId::empty_tree(gix::hash::Kind::Sha1);

        let (msg1, cid1) = ensure_change_id_trailer("feat(core): add status", tree, &[], &sig);
        let (msg2, cid2) = ensure_change_id_trailer("feat(core): add status", tree, &[], &sig);
        assert_eq!(cid1, cid2);
        assert_eq!(cid1.len(), 41);
        assert!(cid1.starts_with('I'));
        assert!(msg1.ends_with(&format!("\n\nChange-Id: {cid1}\n")));
        assert_eq!(msg1, msg2);

        let with_trailer = "feat: existing\n\nSigned-off-by: Bot <bot@oz.dev>";
        let (msg3, cid3) = ensure_change_id_trailer(with_trailer, tree, &[], &sig);
        assert!(msg3.contains(&format!("Signed-off-by: Bot <bot@oz.dev>\nChange-Id: {cid3}\n")));

        let already = "feat: keep\n\nChange-Id: I0123456789abcdef0123456789abcdef01234567\n";
        let (msg4, cid4) = ensure_change_id_trailer(already, tree, &[], &sig);
        assert_eq!(cid4, "I0123456789abcdef0123456789abcdef01234567");
        assert_eq!(msg4, already);
    }
}
