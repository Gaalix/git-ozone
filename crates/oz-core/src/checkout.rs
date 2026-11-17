// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Native `gix` + tree-delta branch switching and path checkout (`oz git switch` / `oz git checkout`).

use crate::commit::remove_pinned_paths;
use crate::diff::normalize_target_path;
use crate::envelope::ConflictEntry;
use crate::error::{OzError, Result};
use crate::repo::OzoneRepo;
use crate::status::{compute_status, open_gix_for_workspace, resolve_workspace_head};
use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::{FullName, Target};
use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Options controlling [`switch_branch`].
#[derive(Debug, Clone, Default)]
pub struct SwitchOptions {
    /// Target branch, tag, or commit revision.
    pub target: Option<String>,
    /// Branch name to create (`-c`/`-b`) or reset (`-C`/`-B`).
    pub create_branch: Option<String>,
    /// Allow resetting an existing branch (`-C`/`-B`).
    pub reset_existing_branch: bool,
    /// Detach `HEAD` at the resolved commit (`--detach`).
    pub detach: bool,
    /// Allow resolving `target` to a commit/tag (detached `HEAD`) when not a local branch.
    pub allow_detached_commit: bool,
    /// Overwrite conflicting dirty working-tree files (`-f` / `--force`).
    pub force: bool,
}

/// Summary returned by [`switch_branch`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    pub created_branch: bool,
    pub detached: bool,
    pub updated_files: usize,
}

/// Options controlling [`checkout_paths`].
#[derive(Debug, Clone, Default)]
pub struct CheckoutPathsOptions {
    /// Optional source revision (defaults to `"HEAD"`).
    pub revision: Option<String>,
    /// Repo-relative pathspecs (exact files, directory prefixes, or globs) to restore.
    pub paths: Vec<String>,
}

/// Summary returned by [`checkout_paths`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutPathsOutcome {
    pub revision: String,
    pub restored_paths: Vec<String>,
}

#[derive(Debug, Clone)]
enum TreeDeltaOp {
    Delete(String),
    Upsert {
        path: String,
        oid: gix::ObjectId,
        kind: EntryKind,
    },
}

impl TreeDeltaOp {
    fn path(&self) -> &str {
        match self {
            Self::Delete(p) | Self::Upsert { path: p, .. } => p,
        }
    }
}

/// Switches branches or detaches `HEAD` via `gix` + tree-delta materialization,
/// refusing dirty working-tree overwrites unless `opts.force` is `true`.
pub fn switch_branch(repo: &OzoneRepo, opts: &SwitchOptions) -> Result<SwitchOutcome> {
    let gix_repo = open_gix_for_workspace(repo)?;
    let head = gix_repo
        .head()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let previous_branch = head
        .referent_name()
        .map(|name| String::from_utf8_lossy(name.shorten().as_ref()).into_owned());
    let current_commit = if head.is_unborn() {
        None
    } else {
        Some(
            head.into_peeled_id()
                .map_err(|err| OzError::Git(err.to_string()))?
                .detach(),
        )
    };
    let current_tree = match current_commit {
        Some(oid) => gix_repo
            .find_commit(oid)
            .map_err(|err| OzError::Git(err.to_string()))?
            .tree_id()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach(),
        None => gix_repo.empty_tree().id().detach(),
    };

    // Special case: unborn HEAD with `-b`/`-c`/`-B`/`-C` and no explicit start revision.
    if current_commit.is_none()
        && opts.target.is_none()
        && let Some(ref new_br) = opts.create_branch
        && !opts.detach
    {
        let full_name = branch_full_name(new_br)?;
        if !opts.reset_existing_branch
            && gix_repo
                .try_find_reference(full_name.as_bstr())
                .map_err(|err| OzError::Git(err.to_string()))?
                .is_some()
        {
            return Err(OzError::InvalidUsage {
                message: format!("a branch named '{new_br}' already exists"),
                recovery: vec!["Pass `-C`/`-B` or choose another branch name.".to_string()],
            });
        }
        set_head_symbolic(&gix_repo, &full_name, "oz git switch")?;
        return Ok(SwitchOutcome {
            previous_branch,
            branch: Some(new_br.clone()),
            head_oid: None,
            created_branch: true,
            detached: false,
            updated_files: 0,
        });
    }

    let (desired_branch, target_commit, branch_ref_op, created_branch) =
        if let Some(ref new_br) = opts.create_branch {
            let full_name = branch_full_name(new_br)?;
            let existing = gix_repo
                .try_find_reference(full_name.as_bstr())
                .map_err(|err| OzError::Git(err.to_string()))?;
            if existing.is_some() && !opts.reset_existing_branch {
                return Err(OzError::InvalidUsage {
                    message: format!("a branch named '{new_br}' already exists"),
                    recovery: vec![format!("Use `oz git switch {new_br}` or `-C {new_br}`.")],
                });
            }
            if existing.is_some()
                && let Some(other_wt) = find_worktree_using_branch(repo, &full_name)?
            {
                return Err(OzError::Conflict {
                    message: format!(
                        "branch '{new_br}' is already checked out in worktree '{other_wt}'"
                    ),
                    conflicts: Vec::new(),
                    recovery: vec!["Switch to a different branch name.".to_string()],
                });
            }

            let start_spec = opts.target.as_deref().unwrap_or("HEAD");
            let start_oid = resolve_commit_oid(&gix_repo, start_spec)?;
            let prev = if opts.reset_existing_branch {
                PreviousValue::Any
            } else {
                PreviousValue::MustNotExist
            };
            (
                Some(new_br.clone()),
                start_oid,
                Some((full_name, start_oid, prev)),
                existing.is_none(),
            )
        } else {
            let spec = opts.target.as_deref().ok_or_else(|| OzError::InvalidUsage {
                message: "missing branch or commit argument".to_string(),
                recovery: vec!["Pass a branch name, e.g. `oz git switch <branch>`.".to_string()],
            })?;

            let candidate_ref = format!("refs/heads/{spec}");
            if !opts.detach
                && let Some(mut reference) = gix_repo
                    .try_find_reference(&candidate_ref)
                    .map_err(|err| OzError::Git(err.to_string()))?
            {
                let full_name = branch_full_name(spec)?;
                if let Some(other_wt) = find_worktree_using_branch(repo, &full_name)? {
                    return Err(OzError::Conflict {
                        message: format!(
                            "branch '{spec}' is already checked out in worktree '{other_wt}'"
                        ),
                        conflicts: Vec::new(),
                        recovery: vec![
                            "Work inside that worktree or create a new branch with `-c`."
                                .to_string(),
                        ],
                    });
                }
                let oid = reference
                    .peel_to_commit()
                    .map_err(|err| OzError::Git(err.to_string()))?
                    .id;
                (Some(spec.to_string()), oid, None, false)
            } else {
                let oid = resolve_commit_oid(&gix_repo, spec)?;
                if !opts.detach && !opts.allow_detached_commit {
                    return Err(OzError::InvalidUsage {
                        message: format!(
                            "'{spec}' is not a local branch; pass `--detach` to check out a commit"
                        ),
                        recovery: vec![format!("oz git switch --detach {spec}")],
                    });
                }
                (None, oid, None, false)
            }
        };

    let target_tree = gix_repo
        .find_commit(target_commit)
        .map_err(|err| OzError::Git(err.to_string()))?
        .tree_id()
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();

    let delta = compute_tree_delta(&gix_repo, current_tree, target_tree)?;

    if !opts.force && !delta.is_empty() {
        let status = compute_status(repo)?;
        let mut conflicting_paths = Vec::new();
        for change in &status.changes {
            for op in &delta {
                if paths_collide(&change.path, op.path()) {
                    conflicting_paths.push(change.path.clone());
                    break;
                }
            }
        }
        if !conflicting_paths.is_empty() {
            let conflicts = conflicting_paths
                .iter()
                .enumerate()
                .map(|(i, p)| ConflictEntry {
                    id: format!("c{}", i + 1),
                    path: p.clone(),
                    message: "local uncommitted changes would be overwritten by switch/checkout"
                        .to_string(),
                    base: None,
                    ours: None,
                    theirs: None,
                    suggested_commands: vec![
                        "oz git commit -m \"wip\"".to_string(),
                        "oz git switch -f".to_string(),
                    ],
                })
                .collect();
            return Err(OzError::Conflict {
                message: format!(
                    "local changes to {} would be overwritten by checkout; commit your changes or pass `-f`",
                    conflicting_paths.join(", ")
                ),
                conflicts,
                recovery: vec![
                    "Commit local changes first (`oz git commit -m \"...\"`) or pass `-f`/`--force`."
                        .to_string(),
                ],
            });
        }
    }

    // Apply branch ref creation/update and HEAD update atomically via gix.
    let mut edits = Vec::new();
    if let Some((full_name, oid, prev)) = branch_ref_op {
        edits.push(RefEdit::update(full_name, oid, prev, "oz git switch"));
    }
    let head_target = match desired_branch {
        Some(ref br) => Target::Symbolic(branch_full_name(br)?),
        None => Target::Object(target_commit),
    };
    edits.push(RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: "oz git switch".into(),
            },
            expected: PreviousValue::Any,
            new: head_target,
        },
        name: FullName::try_from("HEAD").expect("HEAD is valid"),
        deref: false,
    });
    gix_repo
        .edit_references(edits)
        .map_err(|err| OzError::Git(err.to_string()))?;

    // Materialize tree delta onto workdir_path (leaving unrelated dirty files untouched).
    let updated_files = materialize_tree_delta(repo, &gix_repo, &delta)?;
    if opts.force && !delta.is_empty() {
        let overwritten: Vec<String> = delta.iter().map(|d| d.path().to_string()).collect();
        let _ = remove_pinned_paths(repo, overwritten)?;
    }

    // Update `.git/index` / `.git/worktrees/<ws>/index` for the changed paths.
    sync_index_for_delta(repo, &gix_repo, target_tree, &delta)?;

    Ok(SwitchOutcome {
        previous_branch,
        branch: desired_branch.clone(),
        head_oid: Some(target_commit.to_hex().to_string()),
        created_branch,
        detached: desired_branch.is_none(),
        updated_files,
    })
}

/// Restores specific paths from `<revision>` (default `"HEAD"`) into the working directory
/// (`oz git checkout [<rev>] -- <paths...>`).
pub fn checkout_paths(
    repo: &OzoneRepo,
    opts: &CheckoutPathsOptions,
) -> Result<CheckoutPathsOutcome> {
    if opts.paths.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "no pathspecs specified for checkout".to_string(),
            recovery: vec!["Pass `oz git checkout -- <path>...`.".to_string()],
        });
    }

    let gix_repo = open_gix_for_workspace(repo)?;
    let rev_spec = opts.revision.as_deref().unwrap_or("HEAD");
    let tree_id = if rev_spec == "HEAD"
        && gix_repo
            .head()
            .map_err(|err| OzError::Git(err.to_string()))?
            .is_unborn()
    {
        gix_repo.empty_tree().id().detach()
    } else {
        let oid = resolve_commit_oid(&gix_repo, rev_spec)?;
        gix_repo
            .find_commit(oid)
            .map_err(|err| OzError::Git(err.to_string()))?
            .tree_id()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach()
    };

    let tree = gix_repo
        .find_tree(tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let files = tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|err| OzError::Git(err.to_string()))?;

    let mut tree_entries: Vec<(String, gix::ObjectId, EntryKind)> = Vec::new();
    for entry in files {
        if entry.mode.is_tree() || entry.mode.kind() == EntryKind::Commit {
            continue;
        }
        tree_entries.push((
            entry.filepath.to_str_lossy().into_owned(),
            entry.oid,
            entry.mode.kind(),
        ));
    }

    let mut ops = Vec::new();
    let mut restored_paths = Vec::new();
    let mut seen = BTreeSet::new();

    for raw_spec in &opts.paths {
        let matcher = PathspecMatcher::compile(raw_spec)?;
        let mut matched_any = false;
        for (rel, oid, kind) in &tree_entries {
            if matcher.matches(rel) {
                matched_any = true;
                if seen.insert(rel.clone()) {
                    restored_paths.push(rel.clone());
                    ops.push(TreeDeltaOp::Upsert {
                        path: rel.clone(),
                        oid: *oid,
                        kind: *kind,
                    });
                }
            }
        }
        if !matched_any {
            return Err(OzError::InvalidUsage {
                message: format!("pathspec '{raw_spec}' did not match any file(s) known to git"),
                recovery: vec!["Check the path relative to the repository root.".to_string()],
            });
        }
    }

    materialize_tree_delta(repo, &gix_repo, &ops)?;
    restored_paths.sort();

    if rev_spec == "HEAD" {
        let _ = remove_pinned_paths(repo, &restored_paths)?;
    }

    Ok(CheckoutPathsOutcome {
        revision: rev_spec.to_string(),
        restored_paths,
    })
}

/// Resets an existing local branch `branch` to `from_ref` (or `HEAD`), or creates it if absent,
/// refusing if `branch` is checked out in any worktree. Used by `oz git worktree add -B`.
pub fn create_or_reset_local_branch(
    repo: &OzoneRepo,
    branch: &str,
    from_ref: Option<&str>,
) -> Result<String> {
    let gix_repo = repo.open_gix()?;
    let full_name = branch_full_name(branch)?;
    if let Some(other_wt) = find_worktree_using_branch(repo, &full_name)? {
        return Err(OzError::Conflict {
            message: format!("branch '{branch}' is already checked out in worktree '{other_wt}'"),
            conflicts: Vec::new(),
            recovery: vec!["Choose a different branch name.".to_string()],
        });
    }
    let spec = from_ref.unwrap_or("HEAD");
    let oid = resolve_commit_oid(&gix_repo, spec)?;
    gix_repo
        .reference(
            full_name.as_bstr().to_str_lossy().as_ref(),
            oid,
            PreviousValue::Any,
            "oz git worktree add -B",
        )
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(oid.to_hex().to_string())
}

/// Returns `true` if `refs/heads/<name>` exists in `repo`.
pub fn is_local_branch(repo: &OzoneRepo, name: &str) -> Result<bool> {
    let gix_repo = repo.open_gix()?;
    let full = format!("refs/heads/{name}");
    Ok(gix_repo
        .try_find_reference(&full)
        .map_err(|err| OzError::Git(err.to_string()))?
        .is_some())
}

/// Returns `true` if `spec` resolves to a Git revision in `repo`.
pub fn resolves_as_revision(repo: &OzoneRepo, spec: &str) -> bool {
    let Ok(gix_repo) = repo.open_gix() else {
        return false;
    };
    gix_repo.rev_parse_single(spec).is_ok()
}

/// Returns all tracked file paths in `HEAD` tree (empty set if `HEAD` is unborn).
pub fn list_tracked_paths_in_head(repo: &OzoneRepo) -> Result<BTreeSet<String>> {
    let gix_repo = open_gix_for_workspace(repo)?;
    let head = resolve_workspace_head(repo, &gix_repo)?;
    let tree = gix_repo
        .find_tree(head.tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let mut out = BTreeSet::new();
    for entry in tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|err| OzError::Git(err.to_string()))?
    {
        if !entry.mode.is_tree() && entry.mode.kind() != EntryKind::Commit {
            out.insert(entry.filepath.to_str_lossy().into_owned());
        }
    }
    Ok(out)
}

/// Compiles a Git-style pathspec (exact path, `.`, directory prefix, or glob with `*`/`?`/`[...]`)
/// and matches normalized repo-relative paths.
pub struct PathspecMatcher {
    norm: String,
    glob: Option<GlobMatcher>,
}

impl PathspecMatcher {
    pub fn compile(raw: &str) -> Result<Self> {
        let norm = normalize_target_path(raw);
        let has_glob = norm.contains('*') || norm.contains('?') || norm.contains('[');
        let glob = if has_glob {
            Some(
                Glob::new(&norm)
                    .map_err(|err| OzError::InvalidUsage {
                        message: format!("invalid pathspec '{raw}': {err}"),
                        recovery: Vec::new(),
                    })?
                    .compile_matcher(),
            )
        } else {
            None
        };
        Ok(Self { norm, glob })
    }

    #[must_use]
    pub fn matches(&self, rel_path: &str) -> bool {
        if self.norm.is_empty() || self.norm == "." {
            return true;
        }
        if let Some(ref g) = self.glob {
            return g.is_match(rel_path);
        }
        if rel_path == self.norm {
            return true;
        }
        rel_path
            .strip_prefix(&self.norm)
            .is_some_and(|rest| rest.starts_with('/'))
    }
}

fn branch_full_name(branch: &str) -> Result<FullName> {
    let full_str = format!("refs/heads/{branch}");
    FullName::try_from(full_str.as_str()).map_err(|err| OzError::InvalidUsage {
        message: format!("invalid branch name '{branch}': {err}"),
        recovery: Vec::new(),
    })
}

fn resolve_commit_oid(gix_repo: &gix::Repository, spec: &str) -> Result<gix::ObjectId> {
    let obj_id = gix_repo
        .rev_parse_single(spec)
        .map_err(|err| OzError::InvalidUsage {
            message: format!("invalid reference '{spec}': {err}"),
            recovery: vec!["Check `git branch` or `git log --oneline`.".to_string()],
        })?;
    let commit = obj_id
        .object()
        .map_err(|err| OzError::Git(err.to_string()))?
        .peel_to_commit()
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(commit.id)
}

fn set_head_symbolic(
    gix_repo: &gix::Repository,
    target_ref: &FullName,
    reflog_msg: &str,
) -> Result<()> {
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: reflog_msg.into(),
            },
            expected: PreviousValue::Any,
            new: Target::Symbolic(target_ref.clone()),
        },
        name: FullName::try_from("HEAD").expect("HEAD is valid"),
        deref: false,
    };
    gix_repo
        .edit_references([edit])
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(())
}

fn find_worktree_using_branch(repo: &OzoneRepo, target_ref: &FullName) -> Result<Option<String>> {
    let target_str = target_ref.as_bstr().to_str_lossy();
    let self_git_dir = fs::canonicalize(&repo.git_dir).unwrap_or_else(|_| repo.git_dir.clone());

    let main_head = repo.git_common_dir.join("HEAD");
    let main_canon =
        fs::canonicalize(&repo.git_common_dir).unwrap_or_else(|_| repo.git_common_dir.clone());
    if main_canon != self_git_dir
        && let Ok(content) = fs::read_to_string(&main_head)
        && let Some(sym) = content.trim().strip_prefix("ref:")
        && sym.trim() == target_str.as_ref()
    {
        return Ok(Some("main".to_string()));
    }

    let wt_dir = repo.git_common_dir.join("worktrees");
    if wt_dir.is_dir() {
        for entry in fs::read_dir(&wt_dir)? {
            let entry = entry?;
            let path = entry.path();
            let canon = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if canon == self_git_dir {
                continue;
            }
            if let Ok(content) = fs::read_to_string(path.join("HEAD"))
                && let Some(sym) = content.trim().strip_prefix("ref:")
                && sym.trim() == target_str.as_ref()
            {
                let name = entry.file_name().to_string_lossy().into_owned();
                return Ok(Some(name));
            }
        }
    }
    Ok(None)
}

fn compute_tree_delta(
    gix_repo: &gix::Repository,
    lhs_tree_id: gix::ObjectId,
    rhs_tree_id: gix::ObjectId,
) -> Result<Vec<TreeDeltaOp>> {
    if lhs_tree_id == rhs_tree_id {
        return Ok(Vec::new());
    }
    let lhs_tree = gix_repo
        .find_tree(lhs_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let rhs_tree = gix_repo
        .find_tree(rhs_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;

    let mut state = gix::diff::tree::State::default();
    let mut recorder = gix::diff::tree::Recorder::default()
        .track_location(Some(gix::diff::tree::recorder::Location::Path));
    let lhs_iter = gix::objs::TreeRefIter::from_bytes(&lhs_tree.data, lhs_tree.id.kind());
    let rhs_iter = gix::objs::TreeRefIter::from_bytes(&rhs_tree.data, rhs_tree.id.kind());

    gix::diff::tree(lhs_iter, rhs_iter, &mut state, &gix_repo.objects, &mut recorder)
        .map_err(|err| OzError::Git(err.to_string()))?;

    let mut ops = Vec::new();
    for change in recorder.records {
        match change {
            gix::diff::tree::recorder::Change::Deletion {
                entry_mode, path, ..
            } => {
                if !entry_mode.is_tree() && entry_mode.kind() != EntryKind::Commit {
                    ops.push(TreeDeltaOp::Delete(path.to_str_lossy().into_owned()));
                }
            }
            gix::diff::tree::recorder::Change::Addition {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if !entry_mode.is_tree() && entry_mode.kind() != EntryKind::Commit {
                    ops.push(TreeDeltaOp::Upsert {
                        path: path.to_str_lossy().into_owned(),
                        oid,
                        kind: entry_mode.kind(),
                    });
                }
            }
            gix::diff::tree::recorder::Change::Modification {
                previous_entry_mode,
                entry_mode,
                oid,
                path,
                ..
            } => {
                let path_str = path.to_str_lossy().into_owned();
                if entry_mode.is_tree() {
                    if !previous_entry_mode.is_tree() {
                        ops.push(TreeDeltaOp::Delete(path_str));
                    }
                } else if entry_mode.kind() != EntryKind::Commit {
                    ops.push(TreeDeltaOp::Upsert {
                        path: path_str,
                        oid,
                        kind: entry_mode.kind(),
                    });
                }
            }
        }
    }
    Ok(ops)
}

fn paths_collide(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
}

fn materialize_tree_delta(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    ops: &[TreeDeltaOp],
) -> Result<usize> {
    let mut updated = 0usize;

    for op in ops {
        if let TreeDeltaOp::Delete(rel_path) = op {
            let disk_path = repo.workdir_path.join(rel_path);
            match fs::symlink_metadata(&disk_path) {
                Ok(meta) => {
                    if meta.is_dir() {
                        fs::remove_dir_all(&disk_path)?;
                    } else {
                        fs::remove_file(&disk_path)?;
                    }
                    prune_empty_parents(&repo.workdir_path, &disk_path);
                    updated += 1;
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::NotFound
                        || err.kind() == std::io::ErrorKind::NotADirectory => {}
                Err(err) => return Err(OzError::Io(err)),
            }
        }
    }

    for op in ops {
        if let TreeDeltaOp::Upsert {
            path: rel_path,
            oid,
            kind,
        } = op
        {
            let disk_path = repo.workdir_path.join(rel_path);
            remove_conflicting_non_dir_ancestors(&repo.workdir_path, &disk_path)?;
            if let Some(parent) = disk_path.parent() {
                fs::create_dir_all(parent)?;
            }
            if let Ok(meta) = fs::symlink_metadata(&disk_path) {
                if meta.is_dir() {
                    fs::remove_dir_all(&disk_path)?;
                } else if meta.file_type().is_symlink() || *kind == EntryKind::Link {
                    fs::remove_file(&disk_path)?;
                }
            }

            let blob = gix_repo
                .find_blob(*oid)
                .map_err(|err| OzError::Git(err.to_string()))?;

            if *kind == EntryKind::Link {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStrExt;
                    let target = std::ffi::OsStr::from_bytes(&blob.data);
                    std::os::unix::fs::symlink(target, &disk_path)?;
                }
                #[cfg(not(unix))]
                {
                    let target = String::from_utf8_lossy(&blob.data);
                    fs::write(&disk_path, target.as_bytes())?;
                }
            } else {
                fs::write(&disk_path, &blob.data)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let perm = if *kind == EntryKind::BlobExecutable {
                        0o755
                    } else {
                        0o644
                    };
                    fs::set_permissions(&disk_path, fs::Permissions::from_mode(perm))?;
                }
            }
            updated += 1;
        }
    }

    Ok(updated)
}

fn sync_index_for_delta(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    target_tree_id: gix::ObjectId,
    delta: &[TreeDeltaOp],
) -> Result<()> {
    if delta.is_empty() || (repo.workspace.is_some() && !repo.is_linked_worktree) {
        return Ok(());
    }
    let index_path = repo.git_dir.join("index");
    if !index_path.is_file() {
        let mut index = gix_repo
            .index_from_tree(&target_tree_id)
            .map_err(|err| OzError::Git(err.to_string()))?;
        index
            .write(gix::index::write::Options {
                extensions: gix::index::write::Extensions::None,
                skip_hash: true,
            })
            .map_err(|err| OzError::Git(err.to_string()))?;
        return Ok(());
    }

    let mut index = match gix::index::File::at(
        &index_path,
        gix_repo.object_hash(),
        true,
        gix::index::decode::Options::default(),
    ) {
        Ok(idx) => idx,
        Err(_) => {
            let mut rebuilt = gix_repo
                .index_from_tree(&target_tree_id)
                .map_err(|err| OzError::Git(err.to_string()))?;
            rebuilt
                .write(gix::index::write::Options {
                    extensions: gix::index::write::Extensions::None,
                    skip_hash: true,
                })
                .map_err(|err| OzError::Git(err.to_string()))?;
            return Ok(());
        }
    };

    let touched: BTreeSet<&str> = delta.iter().map(TreeDeltaOp::path).collect();
    index.remove_tree();
    index.remove_entries(|_, path, _| touched.contains(path.to_str_lossy().as_ref()));

    for op in delta {
        if let TreeDeltaOp::Upsert { path, oid, kind } = op {
            let disk_path = repo.workdir_path.join(path);
            let stat = gix::index::fs::Metadata::from_path_no_follow(&disk_path)
                .ok()
                .and_then(|meta| gix::index::entry::Stat::from_fs(&meta).ok())
                .unwrap_or_default();
            index.dangerously_push_entry(
                stat,
                *oid,
                gix::index::entry::Flags::empty(),
                (*kind).into(),
                path.as_bytes().as_bstr(),
            );
        }
    }
    index.sort_entries();
    index
        .write(gix::index::write::Options {
            extensions: gix::index::write::Extensions::None,
            skip_hash: true,
        })
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(())
}

fn prune_empty_parents(workdir: &Path, file_path: &Path) {
    let mut cur = file_path.parent();
    while let Some(dir) = cur {
        if dir == workdir || !dir.starts_with(workdir) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}

fn remove_conflicting_non_dir_ancestors(workdir: &Path, target_file: &Path) -> Result<()> {
    let Ok(rel) = target_file.strip_prefix(workdir) else {
        return Ok(());
    };
    let mut acc = workdir.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    if comps.len() <= 1 {
        return Ok(());
    }
    for comp in &comps[..comps.len() - 1] {
        acc.push(comp.as_os_str());
        if let Ok(meta) = fs::symlink_metadata(&acc)
            && !meta.is_dir()
        {
            fs::remove_file(&acc)?;
        }
    }
    Ok(())
}

/// Lists non-Ozone or all linked Git worktrees registered under `.git/worktrees/`.
#[must_use]
pub fn list_git_worktree_entries(repo: &OzoneRepo) -> Vec<(String, PathBuf)> {
    let wt_dir = repo.git_common_dir.join("worktrees");
    let Ok(entries) = fs::read_dir(&wt_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let gitdir_file = entry.path().join("gitdir");
        if let Ok(raw) = fs::read_to_string(&gitdir_file) {
            let p = PathBuf::from(raw.trim());
            let worktree_path = p.parent().map(Path::to_path_buf).unwrap_or(p);
            out.push((name, worktree_path));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
