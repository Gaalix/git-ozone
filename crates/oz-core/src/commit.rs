// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Stage-free working-tree-first commits and one-shot `--pick` / `--exclude` partial
//! commit synthesis using `gix` in-memory ODB & `tree::Editor`, atomic CAS ref edits,
//! and deterministic `Change-Id: I<40-hex>` trailers.

use crate::attributes::{AttributeResolver, clean_blob_bytes};
use crate::diff::{apply_selected_hunks, target_matches_file};
use crate::error::{OzError, Result};
use crate::git_config::GitConfig;
use crate::guardrails::{GuardrailOptions, GuardrailTarget, check_guardrails};
use crate::hooks::{
    CommitHookEnv, run_commit_msg, run_post_commit, run_pre_commit, run_prepare_commit_msg,
};
use crate::repo::OzoneRepo;
use crate::ssh_sign::{
    CommitSignatureVerification, GIT_SSHSIG_NAMESPACE, expand_tilde_path,
    match_allowed_signers_file, resolve_signing_private_key, verify_sshsig,
};
use crate::status::{
    RawDirtyFile, StatusContext, build_repo_attribute_resolver,
    compute_status_context_with_overrides, open_gix_for_workspace, read_blob_bytes,
    read_disk_bytes, read_submodule_head_oid,
};
use crate::types::{ChangeKind, WorkspaceState};
use gix::bstr::ByteSlice;
use gix::objs::WriteTo as _;
use gix::objs::tree::EntryKind;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::{FullName, Target};
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
    /// SSH commit signing override:
    /// - `Some(true)`: force pure-Rust SSH signing
    /// - `Some(false)`: disable signing
    /// - `None`: auto-check `.ozone.toml` `[signing].enabled` or Git `commit.gpgsign = true` when `gpg.format = ssh`
    pub sign: Option<bool>,
    /// Optional SSH signing key path or inline key override (overrides `[signing].key`,
    /// Git `user.signingkey`, and default `~/.ssh/id_ed25519`).
    pub signing_key: Option<String>,
    /// Amend the current `HEAD` commit (`HEAD`'s parent(s) become parent(s), preserving
    /// `HEAD`'s `Change-Id:` trailer unless another is explicitly present).
    pub amend: bool,
    /// Bypass `pre-commit` and `commit-msg` verification hooks (`--no-verify` / `-n`).
    pub no_verify: bool,
    /// Ephemeral Git config overrides (from `-c key=value`) applied over layered Git config.
    pub config_overrides: Vec<(String, String)>,
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

    /// Forces (`true`) or disables (`false`) SSH commit signing.
    #[must_use]
    pub const fn with_sign(mut self, sign: bool) -> Self {
        self.sign = Some(sign);
        self
    }

    /// Overrides the SSH signing key path or inline key specification.
    #[must_use]
    pub fn with_signing_key(mut self, key: impl Into<String>) -> Self {
        self.signing_key = Some(key.into());
        self
    }

    /// Enables or disables `--amend` mode on the current `HEAD` commit.
    #[must_use]
    pub const fn with_amend(mut self, amend: bool) -> Self {
        self.amend = amend;
        self
    }

    /// Enables or disables `--no-verify` hook bypass.
    #[must_use]
    pub const fn with_no_verify(mut self, no_verify: bool) -> Self {
        self.no_verify = no_verify;
        self
    }

    /// Appends `-c key=value` Git configuration overrides.
    #[must_use]
    pub fn with_config_overrides(mut self, overrides: Vec<(String, String)>) -> Self {
        self.config_overrides = overrides;
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
    #[serde(default)]
    pub signed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_fingerprint: Option<String>,
    #[serde(default)]
    pub amended: bool,
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
    UpsertCommit {
        path: String,
        kind: ChangeKind,
        commit_oid: gix::ObjectId,
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

#[derive(Debug, Clone)]
struct AmendedHeadInfo {
    parents: Vec<gix::ObjectId>,
    message: String,
    existing_change_id: Option<String>,
    author: Option<gix::actor::Signature>,
}

fn commit_changes_inner(
    repo: &OzoneRepo,
    opts: &CommitOptions,
    ignore_pinned: bool,
) -> Result<CommitOutcome> {
    if !opts.amend && opts.message.trim().is_empty() {
        return Err(OzError::InvalidUsage {
            message: "commit message cannot be empty".to_string(),
            recovery: vec!["Provide a non-empty message with `-m \"...\"`.".to_string()],
        });
    }

    let ctx = compute_status_context_with_overrides(repo, &opts.config_overrides)?;
    let mut gix_repo = open_gix_for_commit(repo)?;
    let git_cfg = repo.load_git_config(&opts.config_overrides)?;

    let amended_head = if opts.amend {
        let Some(head_oid) = ctx.head.commit_oid else {
            return Err(OzError::InvalidUsage {
                message: "cannot amend on an unborn branch with no commits".to_string(),
                recovery: vec![
                    "Run `oz commit -m \"...\"` without `--amend` to create the initial commit."
                        .to_string(),
                ],
            });
        };
        let head_obj = gix_repo
            .find_commit(head_oid)
            .map_err(OzError::git_source)?;
        let decoded = head_obj.decode().map_err(OzError::git_source)?;
        let parents: Vec<gix::ObjectId> = decoded.parents().collect();
        let raw_msg = decoded.message.to_str_lossy().into_owned();
        let existing_change_id = find_existing_change_id(raw_msg.trim_end_matches(['\r', '\n']));
        let author = decoded.author().ok().and_then(|a| a.to_owned().ok());
        Some(AmendedHeadInfo {
            parents,
            message: raw_msg,
            existing_change_id,
            author,
        })
    } else {
        None
    };

    if ctx.changes.is_empty() && !opts.allow_empty && !opts.amend {
        return Err(OzError::InvalidUsage {
            message: "nothing to commit; working tree is clean".to_string(),
            recovery: vec!["Edit files in the workspace or pass `--allow-empty`.".to_string()],
        });
    }

    let selection = if ctx.changes.is_empty() && opts.pick.is_empty() && opts.exclude.is_empty() {
        BTreeMap::new()
    } else {
        resolve_selection(repo, &ctx, &opts.pick, &opts.exclude, ignore_pinned)?
    };

    if selection.is_empty() && !opts.allow_empty && !opts.amend {
        return Err(OzError::InvalidUsage {
            message: "no changes selected to commit".to_string(),
            recovery: vec!["Run `oz status` to inspect file and hunk IDs.".to_string()],
        });
    }

    let (mut author_sig, committer_sig) = resolve_signatures(&mut gix_repo, opts, &git_cfg)?;
    if let Some(ref head_info) = amended_head
        && opts.author_name.is_none()
        && opts.author_email.is_none()
        && let Some(ref orig_author) = head_info.author
    {
        author_sig = orig_author.clone();
    }

    let hook_env = CommitHookEnv {
        name: Some(author_sig.name.to_str_lossy().into_owned()),
        email: Some(author_sig.email.to_str_lossy().into_owned()),
        date: Some(format_git_author_date(author_sig.time)),
    };

    run_pre_commit(repo, &git_cfg, &hook_env, opts.no_verify)?;

    let attr_resolver = build_repo_attribute_resolver(repo, &gix_repo, ctx.head.tree_id, &git_cfg)?;

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
                    prepared_edits.push(prepare_full_entry_edit(
                        repo,
                        &gix_repo,
                        &ctx,
                        raw,
                        &attr_resolver,
                        &git_cfg,
                        max_blob_bytes,
                        opts.force,
                        out_of_scope,
                        scope_reason,
                    )?);
                }
            }
            FileSelection::Hunks(selected_hunk_indices) => {
                let all_selected =
                    selected_hunk_indices.len() == change.hunks.len() && !change.hunks.is_empty();

                if raw.disk_kind == Some(EntryKind::Commit)
                    || (all_selected && raw.kind != ChangeKind::Deleted)
                {
                    prepared_edits.push(prepare_full_entry_edit(
                        repo,
                        &gix_repo,
                        &ctx,
                        raw,
                        &attr_resolver,
                        &git_cfg,
                        max_blob_bytes,
                        opts.force,
                        out_of_scope,
                        scope_reason,
                    )?);
                } else if raw.kind == ChangeKind::Deleted {
                    prepared_edits.push(PreparedEdit::Remove {
                        path: raw.path.clone(),
                        out_of_scope,
                        scope_reason,
                    });
                } else {
                    let before_bytes = match raw.head_entry {
                        Some(ref he) if he.kind != EntryKind::Commit => {
                            read_blob_bytes(&gix_repo, he.oid)?
                        }
                        _ => Vec::new(),
                    };
                    let disk_path = ctx.source.disk_path(&raw.path);
                    let raw_after = read_disk_bytes(&disk_path, raw.disk_kind)?;
                    let resolved_attrs = attr_resolver.resolve(&raw.path);
                    let after_bytes = clean_blob_bytes(
                        &raw.path,
                        &raw_after,
                        &resolved_attrs,
                        &git_cfg,
                        &repo.workdir_path,
                        &repo.git_dir,
                        true,
                    )?
                    .into_owned();
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
            PreparedEdit::UpsertCommit {
                path,
                kind,
                out_of_scope,
                scope_reason,
                ..
            } => GuardrailTarget {
                path,
                kind: *kind,
                content: &[],
                reported_size_bytes: Some(0),
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

    // Resolve and validate SSH signing key BEFORE mutating any objects or refs.
    let should_sign = resolve_should_sign(repo, &git_cfg, &gix_repo, opts.sign);
    let active_signing_key = if should_sign {
        let key_spec =
            resolve_signing_key_spec(repo, &git_cfg, &gix_repo, opts.signing_key.as_deref());
        let (priv_key, _) = resolve_signing_private_key(&key_spec, Some(&repo.repo_root))?;
        Some(priv_key)
    } else {
        None
    };

    // Build the new tree in memory via gix `tree::Editor`.
    // Apply removals before upserts to cleanly handle file <-> directory replacements.
    let mut editor = gix_repo
        .edit_tree(ctx.head.tree_id)
        .map_err(OzError::git_source)?;

    let mut index_deltas = Vec::with_capacity(prepared_edits.len());
    let mut committed_paths = Vec::with_capacity(prepared_edits.len());

    for edit in &prepared_edits {
        if let PreparedEdit::Remove { path, .. } = edit {
            editor.remove(path.as_str()).map_err(OzError::git_source)?;
            committed_paths.push(path.clone());
            index_deltas.push(IndexDelta::Remove { path: path.clone() });
        }
    }

    for edit in &prepared_edits {
        match edit {
            PreparedEdit::Upsert {
                path,
                entry_kind,
                bytes,
                is_full_file,
                ..
            } => {
                let blob_id = gix_repo
                    .write_blob(bytes)
                    .map_err(OzError::git_source)?
                    .detach();
                editor
                    .upsert(path.as_str(), *entry_kind, blob_id)
                    .map_err(OzError::git_source)?;
                committed_paths.push(path.clone());
                index_deltas.push(IndexDelta::Upsert {
                    path: path.clone(),
                    blob_id,
                    entry_kind: *entry_kind,
                    is_full_file: *is_full_file,
                });
            }
            PreparedEdit::UpsertCommit {
                path, commit_oid, ..
            } => {
                editor
                    .upsert(path.as_str(), EntryKind::Commit, *commit_oid)
                    .map_err(OzError::git_source)?;
                committed_paths.push(path.clone());
                index_deltas.push(IndexDelta::Upsert {
                    path: path.clone(),
                    blob_id: *commit_oid,
                    entry_kind: EntryKind::Commit,
                    is_full_file: false,
                });
            }
            PreparedEdit::Remove { .. } => {}
        }
    }

    committed_paths.sort();

    let new_tree_id = editor.write().map_err(OzError::git_source)?.detach();

    let parents: Vec<gix::ObjectId> = match amended_head {
        Some(ref head_info) => head_info.parents.clone(),
        None => ctx.head.commit_oid.into_iter().collect(),
    };

    let initial_message = if opts.amend && opts.message.trim().is_empty() {
        amended_head.as_ref().map_or_default(|h| h.message.clone())
    } else {
        opts.message.clone()
    };

    let editmsg_path = repo.git_dir.join("COMMIT_EDITMSG");
    fs::write(&editmsg_path, &initial_message)?;
    let (msg_source, msg_sha) = if opts.amend {
        ("commit", Some("HEAD"))
    } else {
        ("message", None)
    };
    run_prepare_commit_msg(
        repo,
        &git_cfg,
        &hook_env,
        &editmsg_path,
        msg_source,
        msg_sha,
    )?;
    run_commit_msg(repo, &git_cfg, &hook_env, &editmsg_path, opts.no_verify)?;
    let raw_message = fs::read_to_string(&editmsg_path)?;

    if raw_message.trim().is_empty() {
        return Err(OzError::InvalidUsage {
            message: "commit message cannot be empty".to_string(),
            recovery: vec!["Provide a non-empty message with `-m \"...\"`.".to_string()],
        });
    }

    let (final_message, change_id) = if let Some(ref head_info) = amended_head
        && find_existing_change_id(raw_message.trim_end_matches(['\r', '\n'])).is_none()
        && let Some(ref preserved_cid) = head_info.existing_change_id
    {
        (
            format_message_with_change_id(&raw_message, preserved_cid),
            preserved_cid.clone(),
        )
    } else {
        ensure_change_id_trailer(&raw_message, new_tree_id, &parents, &author_sig)
    };

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

    let mut commit_obj = gix::objs::Commit {
        message: final_message.as_bytes().into(),
        tree: new_tree_id,
        author: author_sig,
        committer: committer_sig.clone(),
        encoding: None,
        parents: parents.iter().copied().collect(),
        extra_headers: Vec::new(),
    };

    let (signed, signature_fingerprint) = if let Some(ref priv_key) = active_signing_key {
        let mut unsigned_payload =
            Vec::with_capacity(usize::try_from(commit_obj.size()).unwrap_or(0));
        commit_obj
            .write_to(&mut unsigned_payload)
            .map_err(OzError::git_source)?;
        let armored_sig = priv_key.sign_sshsig(&unsigned_payload, GIT_SSHSIG_NAMESPACE)?;
        let fp = priv_key.public_key().fingerprint_sha256();
        let sig_field = gix::objs::commit::signature_field_name(gix_repo.object_hash());
        commit_obj
            .extra_headers
            .push((sig_field.into(), armored_sig.into()));
        (true, Some(fp))
    } else {
        (false, None)
    };

    let commit_id = gix_repo
        .write_object(&commit_obj)
        .map_err(OzError::git_source)?
        .detach();

    let ref_full_name = FullName::try_from(target_ref.as_str()).map_err(OzError::git_source)?;
    let expected = match ctx.head.commit_oid.map(Target::Object) {
        Some(previous) => {
            if ref_full_name.as_bstr() == "HEAD" {
                PreviousValue::MustExistAndMatch(previous)
            } else {
                PreviousValue::ExistingMustMatch(previous)
            }
        }
        None => PreviousValue::MustNotExist,
    };
    let log_op = if opts.amend {
        "commit (amend)"
    } else {
        "commit"
    };
    let log_msg = gix::reference::log::message(
        log_op,
        commit_obj.message.as_bstr(),
        commit_obj.parents.len(),
    );
    let mut committer_buf = gix::date::parse::TimeBuf::default();
    let committer_ref = committer_sig.to_ref(&mut committer_buf);
    gix_repo
        .edit_references_as(
            Some(RefEdit {
                change: Change::Update {
                    log: LogChange {
                        mode: RefLog::AndReference,
                        force_create_reflog: false,
                        message: log_msg,
                    },
                    expected,
                    new: Target::Object(commit_id),
                },
                name: ref_full_name,
                deref: true,
            }),
            Some(committer_ref),
        )
        .map_err(OzError::git_source)?;

    // Update `.git/index` / `.git/worktrees/<ws>/index` if applicable.
    update_git_index(repo, &gix_repo, &ctx, &index_deltas)?;

    // Clear virtual `pinned_paths` (`.git/ozone/state/<ws>.json` or `.git/ozone/pinned_main.json`).
    let clear_all_pinned = (opts.pick.is_empty() && opts.exclude.is_empty()) || ignore_pinned;
    clear_pinned_paths_if_present(
        repo,
        &committed_paths,
        &partial_hunk_paths,
        clear_all_pinned,
    )?;

    let _ = run_post_commit(repo, &git_cfg, &hook_env);

    let remaining_dirty_count = (ctx.changes.len() - selection.len()) + partial_hunk_paths.len();

    Ok(CommitOutcome {
        commit_oid: commit_id.to_hex().to_string(),
        tree_oid: new_tree_id.to_hex().to_string(),
        parent_oid: parents.first().map(|id| id.to_hex().to_string()),
        branch: ctx.head.branch,
        change_id,
        message: final_message,
        committed_paths,
        partial_hunk_paths,
        remaining_dirty_count,
        signed,
        signature_fingerprint,
        amended: opts.amend,
    })
}

fn open_gix_for_commit(repo: &OzoneRepo) -> Result<gix::Repository> {
    let target = if repo.is_linked_worktree && repo.git_dir.exists() {
        &repo.git_dir
    } else {
        &repo.repo_root
    };
    gix::open(target).map_err(OzError::git_source)
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
                    message: format!(
                        "--pick target '{raw_target}' did not match any dirty file or hunk"
                    ),
                    recovery: vec![
                        "Run `oz status` or `oz diff` to list valid file and hunk IDs.".to_string(),
                    ],
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
                    message: format!(
                        "--exclude target '{raw_target}' did not match any dirty file or hunk"
                    ),
                    recovery: vec![
                        "Run `oz status` or `oz diff` to list valid file and hunk IDs.".to_string(),
                    ],
                });
            }
        }
    }

    Ok(selected)
}

fn format_git_author_date(time: gix::date::Time) -> String {
    let sign = if time.offset < 0 { '-' } else { '+' };
    let abs = time.offset.unsigned_abs();
    let hours = abs / 3600;
    let mins = (abs % 3600) / 60;
    format!("@{} {sign}{hours:02}{mins:02}", time.seconds)
}

#[allow(clippy::too_many_arguments)]
fn prepare_full_entry_edit(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    ctx: &StatusContext,
    raw: &RawDirtyFile,
    attr_resolver: &AttributeResolver,
    git_cfg: &GitConfig,
    max_blob_bytes: u64,
    force: bool,
    out_of_scope: bool,
    scope_reason: Option<String>,
) -> Result<PreparedEdit> {
    let disk_path = ctx.source.disk_path(&raw.path);
    if raw.disk_kind == Some(EntryKind::Commit) {
        let commit_oid = read_submodule_head_oid(&disk_path)
            .or_else(|| {
                gix_repo.open_index().ok().and_then(|idx| {
                    idx.entry_by_path(raw.path.as_bytes().as_bstr())
                        .filter(|e| e.mode == gix::index::entry::Mode::COMMIT)
                        .map(|e| e.id)
                })
            })
            .ok_or_else(|| {
                OzError::Git(format!(
                    "submodule '{}' does not have a valid HEAD commit OID",
                    raw.path
                ))
            })?;
        return Ok(PreparedEdit::UpsertCommit {
            path: raw.path.clone(),
            kind: raw.kind,
            commit_oid,
            out_of_scope,
            scope_reason,
        });
    }

    let meta = fs::symlink_metadata(&disk_path)?;
    let file_len = meta.len();
    let entry_kind = raw.disk_kind.unwrap_or(EntryKind::Blob);
    let resolved_attrs = attr_resolver.resolve(&raw.path);

    let (bytes, reported_size_bytes) = if entry_kind == EntryKind::Link {
        (
            read_disk_bytes(&disk_path, Some(EntryKind::Link))?,
            file_len,
        )
    } else if resolved_attrs.filter_driver().is_some() {
        let raw_bytes = read_disk_bytes(&disk_path, Some(entry_kind))?;
        let cleaned = clean_blob_bytes(
            &raw.path,
            &raw_bytes,
            &resolved_attrs,
            git_cfg,
            &repo.workdir_path,
            &repo.git_dir,
            true,
        )?
        .into_owned();
        let cleaned_len = cleaned.len() as u64;
        if !force && cleaned_len > max_blob_bytes {
            (Vec::new(), cleaned_len)
        } else {
            (cleaned, cleaned_len)
        }
    } else if !force && file_len > max_blob_bytes {
        (Vec::new(), file_len)
    } else {
        let raw_bytes = read_disk_bytes(&disk_path, Some(entry_kind))?;
        let cleaned = clean_blob_bytes(
            &raw.path,
            &raw_bytes,
            &resolved_attrs,
            git_cfg,
            &repo.workdir_path,
            &repo.git_dir,
            true,
        )?
        .into_owned();
        let cleaned_len = cleaned.len() as u64;
        (cleaned, cleaned_len)
    };

    Ok(PreparedEdit::Upsert {
        path: raw.path.clone(),
        kind: raw.kind,
        entry_kind,
        bytes,
        reported_size_bytes: Some(reported_size_bytes),
        is_full_file: true,
        out_of_scope,
        scope_reason,
    })
}

fn resolve_signatures(
    gix_repo: &mut gix::Repository,
    opts: &CommitOptions,
    git_cfg: &GitConfig,
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

    let mut committer = match gix_repo.committer() {
        Some(res) => res
            .map_err(OzError::git_source)?
            .to_owned()
            .map_err(OzError::git_source)?,
        None => gix_repo
            .committer_or_set_fallback("Ozone", "oz@ozone.dev")
            .map_err(OzError::git_source)?
            .to_owned()
            .map_err(OzError::git_source)?,
    };

    let mut author = match gix_repo.author() {
        Some(res) => res
            .map_err(OzError::git_source)?
            .to_owned()
            .map_err(OzError::git_source)?,
        None => committer.clone(),
    };

    if std::env::var_os("GIT_COMMITTER_NAME").is_none()
        && let Some(cfg_name) = git_cfg.user_name()
    {
        committer.name = cfg_name.into();
    }
    if std::env::var_os("GIT_COMMITTER_EMAIL").is_none()
        && let Some(cfg_email) = git_cfg.user_email()
    {
        committer.email = cfg_email.into();
    }
    if std::env::var_os("GIT_AUTHOR_NAME").is_none()
        && let Some(cfg_name) = git_cfg.user_name()
    {
        author.name = cfg_name.into();
    }
    if std::env::var_os("GIT_AUTHOR_EMAIL").is_none()
        && let Some(cfg_email) = git_cfg.user_email()
    {
        author.email = cfg_email.into();
    }

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
    (format_message_with_change_id(base, &change_id), change_id)
}

fn format_message_with_change_id(message: &str, change_id: &str) -> String {
    let base = message.trim_end_matches(['\r', '\n']);
    if last_paragraph_is_trailers(base) {
        format!("{base}\nChange-Id: {change_id}\n")
    } else {
        format!("{base}\n\nChange-Id: {change_id}\n")
    }
}

fn resolve_should_sign(
    repo: &OzoneRepo,
    git_cfg: &GitConfig,
    gix_repo: &gix::Repository,
    explicit_sign: Option<bool>,
) -> bool {
    if let Some(sign) = explicit_sign {
        return sign;
    }
    if let Some(enabled) = repo.config.signing.enabled {
        return enabled;
    }
    let snap = gix_repo.config_snapshot();
    let gpgsign = git_cfg
        .commit_gpgsign()
        .or_else(|| snap.boolean("commit.gpgsign"))
        .unwrap_or(false);
    let gpg_format_matches = git_cfg
        .gpg_format()
        .is_some_and(|f| f.trim().eq_ignore_ascii_case("ssh"))
        || snap
            .string("gpg.format")
            .as_deref()
            .is_some_and(|f| f.to_str_lossy().trim().eq_ignore_ascii_case("ssh"));
    gpgsign && gpg_format_matches
}

fn resolve_signing_key_spec(
    repo: &OzoneRepo,
    git_cfg: &GitConfig,
    gix_repo: &gix::Repository,
    explicit_key: Option<&str>,
) -> String {
    if let Some(k) = explicit_key.map(str::trim).filter(|k| !k.is_empty()) {
        return k.to_string();
    }
    if let Some(k) = repo
        .config
        .signing
        .key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        return k.to_string();
    }
    if let Some(k) = git_cfg
        .signing_key()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        return k.to_string();
    }
    if let Some(k) = gix_repo
        .config_snapshot()
        .string("user.signingkey")
        .map(|v| v.to_str_lossy().trim().to_string())
        .filter(|k| !k.is_empty())
    {
        return k;
    }
    "~/.ssh/id_ed25519".to_string()
}

/// Trait allowing [`verify_commit_signature`] to accept `&str` hex OIDs / revisions,
/// `&String`, `String`, or [`gix::ObjectId`].
pub trait CommitOidArg {
    fn resolve_commit_oid(&self, gix_repo: &gix::Repository) -> Result<gix::ObjectId>;
}

impl CommitOidArg for gix::ObjectId {
    fn resolve_commit_oid(&self, _gix_repo: &gix::Repository) -> Result<gix::ObjectId> {
        Ok(*self)
    }
}

impl CommitOidArg for &gix::ObjectId {
    fn resolve_commit_oid(&self, _gix_repo: &gix::Repository) -> Result<gix::ObjectId> {
        Ok(**self)
    }
}

impl CommitOidArg for str {
    fn resolve_commit_oid(&self, gix_repo: &gix::Repository) -> Result<gix::ObjectId> {
        let trimmed = self.trim();
        if let Ok(oid) = gix::ObjectId::from_hex(trimmed.as_bytes()) {
            return Ok(oid);
        }
        gix_repo
            .rev_parse_single(trimmed)
            .map(gix::Id::detach)
            .map_err(|err| OzError::InvalidUsage {
                message: format!("cannot resolve commit '{trimmed}': {err}"),
                recovery: Vec::new(),
            })
    }
}

impl CommitOidArg for &str {
    fn resolve_commit_oid(&self, gix_repo: &gix::Repository) -> Result<gix::ObjectId> {
        (*self).resolve_commit_oid(gix_repo)
    }
}

impl CommitOidArg for String {
    fn resolve_commit_oid(&self, gix_repo: &gix::Repository) -> Result<gix::ObjectId> {
        self.as_str().resolve_commit_oid(gix_repo)
    }
}

impl CommitOidArg for &String {
    fn resolve_commit_oid(&self, gix_repo: &gix::Repository) -> Result<gix::ObjectId> {
        self.as_str().resolve_commit_oid(gix_repo)
    }
}

/// Verifies the OpenSSH `SSHSIG` v1 signature (`gpgsig` header, `namespace = "git"`) on a commit.
pub fn verify_commit_signature<C: CommitOidArg + ?Sized>(
    repo: &OzoneRepo,
    commit_oid: &C,
) -> Result<CommitSignatureVerification> {
    verify_commit_signature_inner(repo, commit_oid, None)
}

/// Verifies a commit's OpenSSH `SSHSIG` v1 signature AND enforces that the signing key
/// matches an entry in `allowed_signers_path`.
pub fn verify_commit_signature_with_allowed_signers<C: CommitOidArg + ?Sized>(
    repo: &OzoneRepo,
    commit_oid: &C,
    allowed_signers_path: &Path,
) -> Result<CommitSignatureVerification> {
    verify_commit_signature_inner(repo, commit_oid, Some(allowed_signers_path))
}

fn verify_commit_signature_inner<C: CommitOidArg + ?Sized>(
    repo: &OzoneRepo,
    commit_oid: &C,
    explicit_allowed_signers: Option<&Path>,
) -> Result<CommitSignatureVerification> {
    let gix_repo = open_gix_for_workspace(repo)?;
    let oid = commit_oid.resolve_commit_oid(&gix_repo)?;
    let commit_obj = gix_repo.find_commit(oid).map_err(OzError::git_source)?;

    let hash_kind = gix_repo.object_hash();
    let mut commit_ref = gix::objs::CommitRef::from_bytes(&commit_obj.data, hash_kind)
        .map_err(OzError::git_source)?;

    let sig_field = gix::objs::commit::signature_field_name(hash_kind);
    let Some(sig_bstr) = commit_ref.extra_headers().find(sig_field) else {
        return Err(OzError::CheckFailed {
            message: format!("commit {oid} is not signed (missing '{sig_field}' header)"),
            recovery: vec!["oz commit --amend --sign -m \"...\"".to_string()],
        });
    };
    let armored_sig = sig_bstr.to_str_lossy().into_owned();

    let committer_email = commit_ref
        .committer()
        .ok()
        .map(|c| c.email.to_str_lossy().into_owned());

    // Strip the signature header and reconstruct the exact unsigned commit payload bytes.
    commit_ref
        .extra_headers
        .retain(|(k, _)| *k != sig_field.as_bytes().as_bstr());
    let mut unsigned_payload = Vec::with_capacity(commit_obj.data.len());
    commit_ref
        .write_to(&mut unsigned_payload)
        .map_err(OzError::git_source)?;

    let parsed = verify_sshsig(&unsigned_payload, &armored_sig, GIT_SSHSIG_NAMESPACE)?;

    let configured_allowed = explicit_allowed_signers.map(Path::to_path_buf).or_else(|| {
        gix_repo
            .config_snapshot()
            .string("gpg.ssh.allowedSignersFile")
            .map(|v| expand_tilde_path(&v.to_str_lossy()))
            .filter(|p| p.is_file())
    });

    let (allowed_signer_matched, matched_principal) = if let Some(ref as_path) = configured_allowed
    {
        let (matched, principal) =
            match_allowed_signers_file(as_path, &parsed.public_key, committer_email.as_deref())?;
        if explicit_allowed_signers.is_some() && !matched {
            return Err(OzError::CheckFailed {
                message: format!(
                    "valid SSH signature on commit {oid} ({}) is not trusted by {}",
                    parsed.public_key.fingerprint_sha256(),
                    as_path.display()
                ),
                recovery: Vec::new(),
            });
        }
        (Some(matched), principal)
    } else {
        (None, None)
    };

    Ok(CommitSignatureVerification {
        commit_oid: oid.to_hex().to_string(),
        valid: true,
        namespace: parsed.namespace,
        hash_algorithm: parsed.hash_algorithm,
        key_algorithm: "ssh-ed25519".to_string(),
        fingerprint: parsed.public_key.fingerprint_sha256(),
        public_key: parsed.public_key.to_openssh_line(),
        committer_email,
        allowed_signer_matched,
        matched_principal,
    })
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
    !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
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
        gix_repo.open_index().map_err(OzError::git_source)?
    } else if ctx.head.commit_oid.is_some() {
        gix_repo
            .index_from_tree(&ctx.head.tree_id)
            .map_err(OzError::git_source)?
    } else {
        gix::index::File::from_state(gix::index::State::new(gix_repo.object_hash()), index_path)
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
        .map_err(OzError::git_source)?;

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
pub(crate) fn write_pinned_paths(repo: &OzoneRepo, paths: &BTreeSet<String>) -> Result<()> {
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
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state"),
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
        assert!(msg3.contains(&format!(
            "Signed-off-by: Bot <bot@oz.dev>\nChange-Id: {cid3}\n"
        )));

        let already = "feat: keep\n\nChange-Id: I0123456789abcdef0123456789abcdef01234567\n";
        let (msg4, cid4) = ensure_change_id_trailer(already, tree, &[], &sig);
        assert_eq!(cid4, "I0123456789abcdef0123456789abcdef01234567");
        assert_eq!(msg4, already);
    }
}
