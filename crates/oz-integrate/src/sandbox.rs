// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared Git/ODB, ephemeral CoW `_integrate/` sandbox, conflict extraction,
//! session persistence, and worktree/index synchronization helpers.

use chrono::{DateTime, Utc};
use gix::bstr::ByteSlice;
use gix::hash::ObjectId;
use gix::merge::blob::builtin_driver::text::Labels;
use gix::merge::plumbing::tree::ConflictIndexEntry;
use gix::merge::tree::{Conflict, Resolution, ResolutionFailure, TreatAsUnresolved};
use gix::object::tree::EntryMode;
use gix::objs::tree::EntryKind;
use oz_core::{
    ConflictEntry, OzError, OzoneRepo, Result, StatusReport, WorkspaceState,
    ensure_change_id_trailer, is_binary_bytes, is_conflict_marker_line,
};
use oz_cow::materialize::{
    TreeDelta, apply_tree_delta_to_dir, clone_dir_with_reflink, compute_tree_delta,
    materialize_cas_workspace, materialize_tree_from_odb, resolve_baseline_tree_id,
};
use oz_cow::{SpawnRequest, detect_filesystem_with_ozone};
use oz_oplog::ActorInfo;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) const MAX_CONFLICT_SNIPPET_BYTES: usize = 4096;

const WARM_CACHE_ROOTS: &[&str] = &[
    "target",
    "node_modules",
    ".next",
    ".turbo",
    ".vite",
    ".venv",
    ".mypy_cache",
    ".ruff_cache",
    ".pytest_cache",
];

/// Active conflict-resolution mode recorded in `.git/ozone/integrate/session.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationMode {
    Integrate,
    StackRebase,
}

/// Which version to take when resolving a conflict via `oz resolve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TakeSide {
    Ours,
    Theirs,
    Base,
}

/// Git object reference and mode for one side of a conflict (`base`, `ours`, `theirs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictSideRef {
    pub oid: String,
    pub mode: u16,
}

/// Persisted metadata for an indexed conflict (`c1`, `c2`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredConflict {
    pub id: String,
    pub path: String,
    pub message: String,
    pub has_text_markers: bool,
    #[serde(default)]
    pub extra_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<ConflictSideRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ours: Option<ConflictSideRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs: Option<ConflictSideRef>,
    #[serde(default)]
    pub resolved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chosen: Option<TakeSide>,
}

/// Resolved source descriptor stored in integration sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedIntegrateSource {
    pub name: String,
    pub branch: String,
    pub commit_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// Continuation state for paused `oz integrate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrateSessionState {
    pub into_branch: String,
    pub original_into_oid: String,
    pub original_into_tree: String,
    pub current_tip_oid: String,
    pub conflict_merged_tree_oid: String,
    pub current_source: ResolvedIntegrateSource,
    #[serde(default)]
    pub merged_sources_so_far: Vec<String>,
    #[serde(default)]
    pub already_integrated: Vec<String>,
    #[serde(default)]
    pub remaining_sources: Vec<ResolvedIntegrateSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_cmd: Option<String>,
    pub allow_out_of_scope: bool,
    pub force: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Branch entry inside a stacked rebase plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackedBranchPlan {
    pub branch: String,
    pub original_oid: String,
    pub original_tree: String,
    pub commits: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_branch: Option<String>,
}

/// Rebased commit record emitted by `oz stack rebase` and persisted across conflicts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebasedCommitInfo {
    pub branch: String,
    pub original_oid: String,
    pub new_oid: String,
    pub change_id: String,
}

/// Continuation state for paused `oz stack rebase`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackRebaseSessionState {
    pub onto_branch: String,
    pub onto_commit_oid: String,
    pub branches: Vec<StackedBranchPlan>,
    pub current_branch_index: usize,
    pub current_commit_index: usize,
    pub current_onto_oid: String,
    pub conflict_merged_tree_oid: String,
    #[serde(default)]
    pub completed_branch_tips: BTreeMap<String, String>,
    #[serde(default)]
    pub rebased_commits: Vec<RebasedCommitInfo>,
}

/// Full conflict session persisted at `.git/ozone/integrate/session.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationSession {
    pub schema_version: u32,
    pub mode: IntegrationMode,
    pub sandbox_path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub conflicts: Vec<StoredConflict>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrate: Option<IntegrateSessionState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack_rebase: Option<StackRebaseSessionState>,
}

impl IntegrationSession {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;
}

/// Returns the main repository [`OzoneRepo`] anchored at `repo.repo_root`.
pub(crate) fn resolve_main_repo(repo: &OzoneRepo) -> Result<OzoneRepo> {
    if repo.workspace.is_none() && !repo.is_linked_worktree {
        Ok(repo.clone())
    } else {
        OzoneRepo::discover(Some(&repo.repo_root))
    }
}

/// Opens `gix::Repository` with a safe fallback committer identity so reflog-enabled
/// operations never fail when `user.name`/`user.email` are unconfigured.
pub(crate) fn open_gix_with_fallback(path: &Path) -> Result<gix::Repository> {
    let mut gix_repo = gix::open(path).map_err(|err| OzError::Git(err.to_string()))?;
    gix_repo
        .committer_or_set_fallback("Ozone", "oz@ozone.dev")
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(gix_repo)
}

pub(crate) fn parse_oid(hex: &str) -> Result<ObjectId> {
    ObjectId::from_hex(hex.as_bytes())
        .map_err(|err| OzError::Git(format!("invalid object id '{hex}': {err}")))
}

pub(crate) fn normalize_branch_short(branch_or_ref: &str) -> &str {
    branch_or_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(branch_or_ref)
}

/// Resolves a local branch (`main` or `refs/heads/main`) to `(full_ref_name, commit_oid)`.
pub(crate) fn resolve_local_branch(
    gix_repo: &gix::Repository,
    branch_or_ref: &str,
) -> Result<Option<(String, ObjectId)>> {
    let short = normalize_branch_short(branch_or_ref);
    let full = format!("refs/heads/{short}");
    match gix_repo
        .try_find_reference(&full)
        .map_err(|err| OzError::Git(err.to_string()))?
    {
        Some(mut r) => {
            let commit = r
                .peel_to_commit()
                .map_err(|err| OzError::Git(err.to_string()))?;
            Ok(Some((full, commit.id)))
        }
        None => Ok(None),
    }
}

/// Resolves a local branch, remote-tracking branch (`origin/main`), full ref (`refs/...`),
/// or commit revision spec to `(display_or_ref_name, commit_oid)`.
pub(crate) fn resolve_branch_or_revision(
    gix_repo: &gix::Repository,
    spec: &str,
) -> Result<Option<(String, ObjectId)>> {
    if let Some(local) = resolve_local_branch(gix_repo, spec)? {
        return Ok(Some(local));
    }

    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    if trimmed.starts_with("refs/")
        && let Some(mut r) = gix_repo
            .try_find_reference(trimmed)
            .map_err(|err| OzError::Git(err.to_string()))?
    {
        let commit = r
            .peel_to_commit()
            .map_err(|err| OzError::Git(err.to_string()))?;
        return Ok(Some((trimmed.to_string(), commit.id)));
    }

    let remote_ref = format!("refs/remotes/{trimmed}");
    if let Some(mut r) = gix_repo
        .try_find_reference(&remote_ref)
        .map_err(|err| OzError::Git(err.to_string()))?
    {
        let commit = r
            .peel_to_commit()
            .map_err(|err| OzError::Git(err.to_string()))?;
        return Ok(Some((remote_ref, commit.id)));
    }

    if let Ok(id) = gix_repo.rev_parse_single(trimmed)
        && let Ok(obj) = id.object()
        && let Ok(commit) = obj.try_into_commit()
    {
        return Ok(Some((trimmed.to_string(), commit.id)));
    }

    Ok(None)
}

/// Returns `true` if `ancestor` is an ancestor of `descendant` (including `ancestor == descendant`).
pub(crate) fn is_ancestor(
    gix_repo: &gix::Repository,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    let mb = gix_repo
        .merge_base(ancestor, descendant)
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(mb.is_some_and(|id| id.detach() == ancestor))
}

/// Resolves the merge-base commit OID between `a` and `b`, or `None` if histories are disjoint.
pub(crate) fn find_merge_base_commit(
    gix_repo: &gix::Repository,
    a: ObjectId,
    b: ObjectId,
) -> Result<Option<ObjectId>> {
    let mb = gix_repo
        .merge_base(a, b)
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(mb.map(|id| id.detach()))
}

/// Resolves the merge-base tree OID between `a` and `b`, falling back to empty tree if disjoint.
pub(crate) fn merge_base_tree_id(
    gix_repo: &gix::Repository,
    a: ObjectId,
    b: ObjectId,
) -> Result<ObjectId> {
    if let Some(mb_id) = find_merge_base_commit(gix_repo, a, b)? {
        let commit = gix_repo
            .find_commit(mb_id)
            .map_err(|err| OzError::Git(err.to_string()))?;
        Ok(commit
            .tree_id()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach())
    } else {
        Ok(gix_repo.empty_tree().id().detach())
    }
}

pub(crate) fn commit_tree_id(gix_repo: &gix::Repository, commit_id: ObjectId) -> Result<ObjectId> {
    let commit = gix_repo
        .find_commit(commit_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(commit
        .tree_id()
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach())
}

/// Resolves committer and author signatures using repository config or Ozone actor fallback.
pub(crate) fn resolve_signatures(
    gix_repo: &gix::Repository,
    actor: &ActorInfo,
    author_override: Option<gix::actor::Signature>,
) -> Result<(gix::actor::Signature, gix::actor::Signature)> {
    let committer = match gix_repo.committer() {
        Some(res) => res
            .map_err(|err| OzError::Git(err.to_string()))?
            .to_owned()
            .map_err(|err| OzError::Git(err.to_string()))?,
        None => {
            let name = actor
                .client
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("Ozone");
            gix::actor::Signature {
                name: name.into(),
                email: "oz@ozone.dev".into(),
                time: gix::date::Time::now_local_or_utc(),
            }
        }
    };

    let author = match author_override {
        Some(a) => a,
        None => match gix_repo.author() {
            Some(res) => res
                .map_err(|err| OzError::Git(err.to_string()))?
                .to_owned()
                .map_err(|err| OzError::Git(err.to_string()))?,
            None => committer.clone(),
        },
    };

    Ok((author, committer))
}

/// Preserves an existing `Change-Id:` trailer anywhere in the trailing block (even on single-paragraph
/// messages), or delegates to `oz_core::ensure_change_id_trailer` when absent.
pub(crate) fn preserve_or_add_change_id(
    message: &str,
    tree_id: ObjectId,
    parents: &[ObjectId],
    author: &gix::actor::Signature,
) -> (String, String) {
    let base = message.trim_end_matches(['\r', '\n']);
    for line in base.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':')
            && k.eq_ignore_ascii_case("Change-Id")
        {
            let id = v.trim();
            if !id.is_empty() {
                return (format!("{base}\n"), id.to_string());
            }
        }
    }
    ensure_change_id_trailer(message, tree_id, parents, author)
}

/// Creates a commit object in the ODB without updating any references.
pub(crate) fn write_commit_in_odb(
    gix_repo: &gix::Repository,
    author: &gix::actor::Signature,
    committer: &gix::actor::Signature,
    message: &str,
    tree_id: ObjectId,
    parents: &[ObjectId],
) -> Result<ObjectId> {
    let mut author_buf = gix::date::parse::TimeBuf::default();
    let mut committer_buf = gix::date::parse::TimeBuf::default();
    let author_ref = author.to_ref(&mut author_buf);
    let committer_ref = committer.to_ref(&mut committer_buf);
    let commit = gix_repo
        .new_commit_as(
            committer_ref,
            author_ref,
            message,
            tree_id,
            parents.iter().copied(),
        )
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(commit.id)
}

/// Path to `.git/ozone/integrate/session.json`.
#[must_use]
pub(crate) fn session_file_path(repo: &OzoneRepo) -> PathBuf {
    repo.layout.ozone_dir.join("integrate").join("session.json")
}

/// Path to `../<repo>.oz/_integrate`.
pub(crate) fn integrate_sandbox_path(repo: &OzoneRepo) -> Result<PathBuf> {
    repo.config
        .render_workspace_path(&repo.repo_root, "_integrate")
}

pub(crate) fn load_session(repo: &OzoneRepo) -> Result<Option<IntegrationSession>> {
    let path = session_file_path(repo);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)?;
    let session: IntegrationSession = serde_json::from_str(&raw)?;
    Ok(Some(session))
}

pub(crate) fn save_session(repo: &OzoneRepo, session: &IntegrationSession) -> Result<()> {
    let path = session_file_path(repo);
    let parent = path
        .parent()
        .ok_or_else(|| OzError::Other(format!("path {} has no parent", path.display())))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".tmp_session_{}.json", std::process::id()));
    let bytes = serde_json::to_vec_pretty(session)?;
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Removes `_integrate` and clears any readonly permissions on Windows before deleting.
pub(crate) fn remove_sandbox_dir(path: &Path) -> Result<bool> {
    if !path.exists() && fs::symlink_metadata(path).is_err() {
        return Ok(false);
    }
    clear_readonly_tree(path);
    fs::remove_dir_all(path)?;
    Ok(true)
}

#[cfg(windows)]
fn clear_readonly_tree(path: &Path) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if let Ok(meta) = fs::symlink_metadata(&p) {
            let mut perms = meta.permissions();
            if perms.readonly() {
                perms.set_readonly(false);
                let _ = fs::set_permissions(&p, perms);
            }
            if meta.is_dir() {
                clear_readonly_tree(&p);
            }
        }
    }
}

#[cfg(not(windows))]
fn clear_readonly_tree(_path: &Path) {}

pub(crate) fn cleanup_session_and_sandbox(
    repo: &OzoneRepo,
    sandbox_hint: Option<&Path>,
) -> Result<(bool, bool)> {
    let default_sandbox = integrate_sandbox_path(repo)?;
    let target_sandbox = sandbox_hint.unwrap_or(&default_sandbox);
    let sandbox_removed = remove_sandbox_dir(target_sandbox)?;

    let session_path = session_file_path(repo);
    let session_removed = if session_path.is_file() {
        fs::remove_file(&session_path)?;
        true
    } else {
        false
    };
    if let Some(dir) = session_path.parent()
        && dir.is_dir()
    {
        let _ = fs::remove_dir(dir);
    }
    Ok((sandbox_removed, session_removed))
}

/// Finds a matching or nearest warm baseline `root/` directory in `.git/ozone/baselines/`.
fn find_warm_baseline_root(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    hint_commit: Option<ObjectId>,
) -> Option<PathBuf> {
    let baselines_dir = repo.layout.baselines_dir();
    if !baselines_dir.is_dir() {
        return None;
    }

    if let Some(commit_id) = hint_commit {
        let hex = commit_id.to_hex().to_string();
        let exact = baselines_dir.join(&hex);
        if exact.join("metadata.json").is_file() && exact.join("root").is_dir() {
            return Some(exact.join("root"));
        }
    }

    let mut available: BTreeMap<String, PathBuf> = BTreeMap::new();
    let entries = fs::read_dir(&baselines_dir).ok()?;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let dir = entry.path();
        if dir.join("metadata.json").is_file() && dir.join("root").is_dir() {
            available.insert(name, dir.join("root"));
        }
    }
    if available.is_empty() {
        return None;
    }

    if let Some(start) = hint_commit
        && let Ok(walk) = gix_repo.rev_walk([start]).all()
    {
        for item in walk.flatten() {
            let hex = item.id.to_hex().to_string();
            if let Some(root) = available.remove(&hex) {
                return Some(root);
            }
        }
    }

    available.into_values().next_back()
}

/// Materializes `candidate_tree_id` into `../<repo>.oz/_integrate`, linking or cloning warm
/// baseline caches when present in `.git/ozone/baselines/`.
pub(crate) fn materialize_integrate_sandbox(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    sandbox_dir: &Path,
    candidate_tree_id: ObjectId,
    hint_commit: Option<ObjectId>,
) -> Result<()> {
    let _ = remove_sandbox_dir(sandbox_dir);
    if let Some(parent) = sandbox_dir.parent() {
        fs::create_dir_all(parent)?;
    }

    let baseline_root = find_warm_baseline_root(repo, gix_repo, hint_commit);
    if let Some(ref b_root) = baseline_root {
        let probe =
            detect_filesystem_with_ozone(&repo.repo_root, Some(&repo.layout.ozone_dir))?;
        if probe.reflink_supported {
            clone_dir_with_reflink(b_root, sandbox_dir)?;
            if let Some(base_tree_id) = resolve_baseline_tree_id(gix_repo, b_root) {
                apply_tree_delta_with_cleanup(gix_repo, base_tree_id, candidate_tree_id, sandbox_dir)?;
            } else {
                materialize_tree_from_odb(gix_repo, candidate_tree_id, sandbox_dir)?;
            }
            return Ok(());
        }

        let req = SpawnRequest {
            workspace_name: "_integrate".to_string(),
            branch: "HEAD".to_string(),
            base_commit: hint_commit.map(|id| id.to_hex().to_string()),
            base_tree: Some(candidate_tree_id.to_hex().to_string()),
            repo_root: repo.repo_root.clone(),
            git_dir: repo.git_common_dir.clone(),
            target_path: sandbox_dir.to_path_buf(),
            upper_dir: None,
            meta_dir: None,
            baseline_dir: Some(b_root.clone()),
            scope: Vec::new(),
            deny: Vec::new(),
            materialize_files: true,
        };
        let cas_dir = repo.layout.ozone_dir.join("cas");
        materialize_cas_workspace(&req, &cas_dir, &repo.git_common_dir)?;
        return Ok(());
    }

    fs::create_dir_all(sandbox_dir)?;
    materialize_tree_from_odb(gix_repo, candidate_tree_id, sandbox_dir)?;
    Ok(())
}

/// Applies `old_tree_id -> new_tree_id` to `target_dir`, safely handling file/directory transitions
/// and pruning empty directories left by deleted files.
pub(crate) fn apply_tree_delta_with_cleanup(
    gix_repo: &gix::Repository,
    old_tree_id: ObjectId,
    new_tree_id: ObjectId,
    target_dir: &Path,
) -> Result<TreeDelta> {
    let delta = compute_tree_delta(gix_repo, old_tree_id, new_tree_id)?;
    if delta.deletions.is_empty() && delta.upserts.is_empty() {
        return Ok(delta);
    }

    for rel in &delta.deletions {
        let p = target_dir.join(rel);
        if let Ok(meta) = fs::symlink_metadata(&p) {
            if meta.is_dir() {
                fs::remove_dir_all(&p)?;
            } else {
                fs::remove_file(&p)?;
            }
            prune_empty_parents(target_dir, &p);
        }
    }

    for (rel, _, _) in &delta.upserts {
        let dst = target_dir.join(rel);
        remove_conflicting_non_dir_ancestors(target_dir, &dst)?;
        if let Ok(meta) = fs::symlink_metadata(&dst)
            && meta.is_dir()
        {
            fs::remove_dir_all(&dst)?;
        }
    }

    let upsert_only = TreeDelta {
        upserts: delta.upserts.clone(),
        deletions: Vec::new(),
    };
    apply_tree_delta_to_dir(gix_repo, &upsert_only, target_dir)?;
    Ok(delta)
}

pub(crate) fn prune_empty_parents(root: &Path, file_path: &Path) {
    let mut cur = file_path.parent();
    while let Some(dir) = cur {
        if dir == root || !dir.starts_with(root) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}

fn remove_conflicting_non_dir_ancestors(root: &Path, target_file: &Path) -> Result<()> {
    let Ok(rel) = target_file.strip_prefix(root) else {
        return Ok(());
    };
    let comps: Vec<_> = rel.components().collect();
    if comps.len() <= 1 {
        return Ok(());
    }
    let mut acc = root.to_path_buf();
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

/// Syncs `.git/index` (or `.git/worktrees/<ws>/index`) for the changed paths in `delta`.
pub(crate) fn sync_git_index_for_delta(
    git_dir: &Path,
    worktree_root: &Path,
    old_tree_id: ObjectId,
    new_tree_id: ObjectId,
    delta: &TreeDelta,
) -> Result<()> {
    if delta.upserts.is_empty() && delta.deletions.is_empty() {
        return Ok(());
    }

    let idx_repo = open_gix_with_fallback(git_dir)?;
    let index_path = idx_repo.index_path().to_path_buf();
    if !index_path.is_file() {
        let mut index = idx_repo
            .index_from_tree(&new_tree_id)
            .map_err(|err| OzError::Git(err.to_string()))?;
        index
            .write(gix::index::write::Options::default())
            .map_err(|err| OzError::Git(err.to_string()))?;
        return Ok(());
    }

    let mut index = idx_repo
        .open_index()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let _ = old_tree_id;
    index.remove_tree();

    let mut touched: BTreeSet<&str> = BTreeSet::new();
    for rel in &delta.deletions {
        touched.insert(rel.as_str());
    }
    for (rel, _, _) in &delta.upserts {
        touched.insert(rel.as_str());
    }

    index.remove_entries(|_, path, _| {
        let p = path.to_str_lossy();
        touched.contains(p.as_ref())
    });

    for (rel, blob_id, entry_mode) in &delta.upserts {
        let disk_path = worktree_root.join(rel);
        let stat = gix::index::fs::Metadata::from_path_no_follow(&disk_path)
            .ok()
            .and_then(|meta| gix::index::entry::Stat::from_fs(&meta).ok())
            .unwrap_or_default();
        index.dangerously_push_entry(
            stat,
            *blob_id,
            gix::index::entry::Flags::empty(),
            entry_mode.kind().into(),
            rel.as_bytes().as_bstr(),
        );
    }

    index.sort_entries();
    index
        .write(gix::index::write::Options::default())
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(())
}

/// Returns any dirty paths from `status` that conflict with `delta`.
pub(crate) fn conflicting_dirty_paths(status: &StatusReport, delta: &TreeDelta) -> Vec<String> {
    if status.changes.is_empty() || (delta.deletions.is_empty() && delta.upserts.is_empty()) {
        return Vec::new();
    }

    let mut delta_paths: BTreeSet<&str> = BTreeSet::new();
    for rel in &delta.deletions {
        delta_paths.insert(rel.as_str());
    }
    for (rel, _, _) in &delta.upserts {
        delta_paths.insert(rel.as_str());
    }

    let mut conflicts = Vec::new();
    for change in &status.changes {
        let mut hit = paths_conflict(&change.path, &delta_paths);
        if !hit && let Some(ref old_path) = change.old_path {
            hit = paths_conflict(old_path, &delta_paths);
        }
        if hit {
            conflicts.push(change.path.clone());
        }
    }
    conflicts.sort();
    conflicts.dedup();
    conflicts
}

fn paths_conflict(dirty_path: &str, delta_paths: &BTreeSet<&str>) -> bool {
    if delta_paths.contains(dirty_path) {
        return true;
    }
    for dp in delta_paths {
        if is_path_prefix(dirty_path, dp) || is_path_prefix(dp, dirty_path) {
            return true;
        }
    }
    false
}

fn is_path_prefix(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Builds `gix::merge::blob::builtin_driver::text::Labels` from static/borrowed strings.
pub(crate) fn merge_labels<'a>(
    ancestor: &'a str,
    ours: &'a str,
    theirs: &'a str,
) -> Labels<'a> {
    Labels {
        ancestor: Some(ancestor.as_bytes().as_bstr()),
        current: Some(ours.as_bytes().as_bstr()),
        other: Some(theirs.as_bytes().as_bstr()),
    }
}

/// Extracts unresolved conflicts from a `gix` tree merge outcome, ALWAYS using
/// `conflict.entries()` (`[0]=Base`, `[1]=Ours`, `[2]=Theirs`) and
/// `conflict.changes_in_resolution()`, sorted deterministically and indexed `c1`, `c2`, ...
pub(crate) fn extract_unresolved_conflicts(
    gix_repo: &gix::Repository,
    conflicts: &[Conflict],
    merged_tree_id: ObjectId,
) -> Result<(Vec<StoredConflict>, Vec<ConflictEntry>)> {
    struct RawExtracted {
        path: String,
        message: String,
        has_text_markers: bool,
        extra_paths: Vec<String>,
        base: Option<ConflictSideRef>,
        ours: Option<ConflictSideRef>,
        theirs: Option<ConflictSideRef>,
        base_snippet: Option<String>,
        ours_snippet: Option<String>,
        theirs_snippet: Option<String>,
    }

    let merged_tree = gix_repo
        .find_tree(merged_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;

    let mut extracted = Vec::new();
    for conflict in conflicts {
        if !conflict.is_unresolved(TreatAsUnresolved::git()) {
            continue;
        }

        // CRITICAL: always access `conflict.entries()` and `conflict.changes_in_resolution()`
        // so `ConflictMapping::Swapped` is compensated accurately!
        let entries = conflict.entries();
        let (res_ours, res_theirs) = conflict.changes_in_resolution();
        let (path, extra_paths, message) = describe_conflict(conflict, res_ours, res_theirs);

        let has_text_markers = check_tree_path_has_markers(gix_repo, &merged_tree, &path)?;

        let base = side_ref_from_entry(entries[0]);
        let ours = side_ref_from_entry(entries[1]);
        let theirs = side_ref_from_entry(entries[2]);

        let base_snippet = snippet_from_entry(gix_repo, entries[0])?;
        let ours_snippet = snippet_from_entry(gix_repo, entries[1])?;
        let theirs_snippet = snippet_from_entry(gix_repo, entries[2])?;

        extracted.push(RawExtracted {
            path,
            message,
            has_text_markers,
            extra_paths,
            base,
            ours,
            theirs,
            base_snippet,
            ours_snippet,
            theirs_snippet,
        });
    }

    extracted.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.message.cmp(&b.message)));

    let mut stored = Vec::with_capacity(extracted.len());
    let mut entries_out = Vec::with_capacity(extracted.len());
    for (idx, item) in extracted.into_iter().enumerate() {
        let id = format!("c{}", idx + 1);
        let mut suggested_commands = Vec::new();
        suggested_commands.push(format!("oz resolve {id} --take ours"));
        suggested_commands.push(format!("oz resolve {id} --take theirs"));
        if item.base.is_some() {
            suggested_commands.push(format!("oz resolve {id} --take base"));
        }

        entries_out.push(ConflictEntry {
            id: id.clone(),
            path: item.path.clone(),
            message: item.message.clone(),
            base: item.base_snippet,
            ours: item.ours_snippet,
            theirs: item.theirs_snippet,
            suggested_commands,
        });

        stored.push(StoredConflict {
            id,
            path: item.path,
            message: item.message,
            has_text_markers: item.has_text_markers,
            extra_paths: item.extra_paths,
            base: item.base,
            ours: item.ours,
            theirs: item.theirs,
            resolved: false,
            chosen: None,
        });
    }

    Ok((stored, entries_out))
}

fn describe_conflict(
    conflict: &Conflict,
    res_ours: &gix::diff::tree_with_rewrites::Change,
    res_theirs: &gix::diff::tree_with_rewrites::Change,
) -> (String, Vec<String>, String) {
    let ours_loc = res_ours.location().to_str_lossy().into_owned();
    let theirs_loc = res_theirs.location().to_str_lossy().into_owned();
    let mut extra_paths = Vec::new();

    let message = match &conflict.resolution {
        Ok(res) => match res {
            Resolution::OursModifiedTheirsModifiedThenBlobContentMerge { .. } => {
                if ours_loc != theirs_loc {
                    extra_paths.push(theirs_loc);
                }
                "overlapping content modification".to_string()
            }
            Resolution::OursModifiedTheirsRenamedAndChangedThenRename { final_location, .. } => {
                let p = final_location
                    .as_ref()
                    .map(|b| b.to_str_lossy().into_owned())
                    .unwrap_or(theirs_loc);
                return (p, extra_paths, "content conflict after rename".to_string());
            }
            Resolution::SourceLocationAffectedByRename { final_location } => {
                let p = final_location.to_str_lossy().into_owned();
                return (p, extra_paths, "directory rename conflict".to_string());
            }
            Resolution::Forced(_) => "forced conflict resolution".to_string(),
        },
        Err(failure) => match failure {
            ResolutionFailure::OursModifiedTheirsDeleted => "modify/delete conflict".to_string(),
            ResolutionFailure::OursDeletedTheirsRenamed => {
                if ours_loc != theirs_loc {
                    extra_paths.push(ours_loc);
                }
                return (
                    theirs_loc,
                    extra_paths,
                    "delete/rename conflict".to_string(),
                );
            }
            ResolutionFailure::OursRenamedTheirsRenamedDifferently { .. } => {
                if theirs_loc != ours_loc {
                    extra_paths.push(theirs_loc);
                }
                "rename/rename conflict".to_string()
            }
            ResolutionFailure::OursRenamedTheirsRenamedToSameLocation => {
                "rename/rename collision at same destination".to_string()
            }
            ResolutionFailure::OursModifiedTheirsDirectoryThenOursRenamed {
                renamed_unique_path_to_modified_blob,
            } => {
                extra_paths.push(
                    renamed_unique_path_to_modified_blob
                        .to_str_lossy()
                        .into_owned(),
                );
                "file/directory conflict".to_string()
            }
            ResolutionFailure::OursDirectoryTheirsNonDirectoryTheirsRenamed {
                renamed_unique_path_of_theirs,
            } => {
                extra_paths.push(renamed_unique_path_of_theirs.to_str_lossy().into_owned());
                "directory/file conflict".to_string()
            }
            ResolutionFailure::OursAddedTheirsAddedTypeMismatch {
                their_unique_location,
            } => {
                extra_paths.push(their_unique_location.to_str_lossy().into_owned());
                "add/add type mismatch conflict".to_string()
            }
            ResolutionFailure::OursModifiedTheirsRenamedTypeMismatch => {
                if theirs_loc != ours_loc {
                    extra_paths.push(theirs_loc);
                }
                "modify/rename type mismatch conflict".to_string()
            }
            ResolutionFailure::SubmoduleMerge | ResolutionFailure::SubmoduleAddAdd => {
                "submodule conflict".to_string()
            }
            ResolutionFailure::Unknown => "tree merge conflict".to_string(),
        },
    };

    (ours_loc, extra_paths, message)
}

fn check_tree_path_has_markers(
    gix_repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    rel_path: &str,
) -> Result<bool> {
    let Some(entry) = tree
        .lookup_entry_by_path(rel_path)
        .map_err(|err| OzError::Git(err.to_string()))?
    else {
        return Ok(false);
    };
    if entry.mode().is_tree() || entry.mode().is_commit() || entry.mode().is_link() {
        return Ok(false);
    }
    let blob = gix_repo
        .find_blob(entry.object_id())
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(bytes_contain_conflict_markers(&blob.data))
}

fn side_ref_from_entry(entry: Option<ConflictIndexEntry>) -> Option<ConflictSideRef> {
    entry.map(|e| ConflictSideRef {
        oid: e.id.to_hex().to_string(),
        mode: e.mode.value(),
    })
}

fn snippet_from_entry(
    gix_repo: &gix::Repository,
    entry: Option<ConflictIndexEntry>,
) -> Result<Option<String>> {
    let Some(e) = entry else {
        return Ok(None);
    };
    if e.mode.is_tree() {
        return Ok(Some("<directory>".to_string()));
    }
    if e.mode.is_commit() {
        return Ok(Some(format!("<submodule {}>", e.id.to_hex())));
    }
    let blob = gix_repo
        .find_blob(e.id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    if e.mode.is_link() {
        let target = String::from_utf8_lossy(&blob.data);
        return Ok(Some(format!("<symlink -> {target}>")));
    }
    if is_binary_bytes(&blob.data) {
        return Ok(Some(format!("<binary {} bytes>", blob.data.len())));
    }
    let lossy = String::from_utf8_lossy(&blob.data);
    if lossy.len() <= MAX_CONFLICT_SNIPPET_BYTES {
        Ok(Some(lossy.into_owned()))
    } else {
        let mut end = MAX_CONFLICT_SNIPPET_BYTES;
        while end > 0 && !lossy.is_char_boundary(end) {
            end -= 1;
        }
        Ok(Some(format!("{}...<truncated>", &lossy[..end])))
    }
}

pub(crate) fn bytes_contain_conflict_markers(bytes: &[u8]) -> bool {
    if bytes.is_empty() || is_binary_bytes(bytes) {
        return false;
    }
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .any(|raw| is_conflict_marker_line(raw.trim_end_matches('\r')))
}

pub(crate) fn entry_kind_from_u16_mode(mode: u16) -> EntryKind {
    match mode {
        0o120000 => EntryKind::Link,
        0o100755 => EntryKind::BlobExecutable,
        _ => EntryKind::Blob,
    }
}

pub(crate) fn write_entry_to_sandbox(
    gix_repo: &gix::Repository,
    sandbox_dir: &Path,
    rel_path: &str,
    side: Option<&ConflictSideRef>,
) -> Result<()> {
    let dst = sandbox_dir.join(rel_path);
    match side {
        None => {
            if let Ok(meta) = fs::symlink_metadata(&dst) {
                if meta.is_dir() {
                    fs::remove_dir_all(&dst)?;
                } else {
                    fs::remove_file(&dst)?;
                }
                prune_empty_parents(sandbox_dir, &dst);
            }
            Ok(())
        }
        Some(side_ref) => {
            let oid = parse_oid(&side_ref.oid)?;
            let kind = entry_kind_from_u16_mode(side_ref.mode);
            let blob = gix_repo
                .find_blob(oid)
                .map_err(|err| OzError::Git(err.to_string()))?;
            remove_conflicting_non_dir_ancestors(sandbox_dir, &dst)?;
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            if let Ok(meta) = fs::symlink_metadata(&dst) {
                if meta.is_dir() {
                    fs::remove_dir_all(&dst)?;
                } else if meta.file_type().is_symlink() || kind == EntryKind::Link {
                    fs::remove_file(&dst)?;
                }
            }
            if kind == EntryKind::Link {
                write_symlink_bytes(&dst, &blob.data)?;
            } else {
                fs::write(&dst, &blob.data)?;
                apply_unix_exec_mode(&dst, kind == EntryKind::BlobExecutable)?;
            }
            Ok(())
        }
    }
}

fn write_symlink_bytes(dst: &Path, target_bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let target = Path::new(OsStr::from_bytes(target_bytes));
        std::os::unix::fs::symlink(target, dst)?;
    }
    #[cfg(not(unix))]
    {
        fs::write(dst, target_bytes)?;
    }
    Ok(())
}

fn apply_unix_exec_mode(path: &Path, executable: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, executable);
    }
    Ok(())
}

/// Validates that all conflicts in `session` are resolved (either marked resolved via `oz resolve`
/// or, for text-marker conflicts, edited on disk to remove all conflict markers) and that no file
/// in `_integrate/` still contains conflict markers, then builds the resulting tree in the ODB.
pub(crate) fn verify_and_build_resolved_tree(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    session: &IntegrationSession,
) -> Result<ObjectId> {
    let sandbox_dir = &session.sandbox_path;
    if !sandbox_dir.is_dir() {
        return Err(OzError::InvalidUsage {
            message: format!(
                "integration sandbox {} does not exist",
                sandbox_dir.display()
            ),
            recovery: vec!["oz integrate --abort".to_string()],
        });
    }

    let base_tree_hex = if let Some(ref st) = session.integrate {
        &st.conflict_merged_tree_oid
    } else if let Some(ref st) = session.stack_rebase {
        &st.conflict_merged_tree_oid
    } else {
        return Err(OzError::Other("corrupt integration session state".to_string()));
    };
    let base_merged_tree_id = parse_oid(base_tree_hex)?;

    // 1. Check all recorded conflicts.
    let mut still_unresolved: Vec<&StoredConflict> = Vec::new();
    for c in &session.conflicts {
        let disk_path = sandbox_dir.join(&c.path);
        let disk_has_markers = match fs::symlink_metadata(&disk_path) {
            Ok(meta) if meta.is_file() => {
                let bytes = fs::read(&disk_path)?;
                bytes_contain_conflict_markers(&bytes)
            }
            _ => false,
        };

        if disk_has_markers {
            still_unresolved.push(c);
            continue;
        }
        if !c.resolved && !c.has_text_markers {
            still_unresolved.push(c);
        }
    }

    if !still_unresolved.is_empty() {
        let mut entries = Vec::with_capacity(still_unresolved.len());
        let mut recovery = Vec::new();
        for c in &still_unresolved {
            let mut suggested = vec![
                format!("oz resolve {} --take ours", c.id),
                format!("oz resolve {} --take theirs", c.id),
            ];
            if c.base.is_some() {
                suggested.push(format!("oz resolve {} --take base", c.id));
            }
            recovery.extend(suggested.iter().cloned());
            entries.push(ConflictEntry {
                id: c.id.clone(),
                path: c.path.clone(),
                message: format!("unresolved conflict: {}", c.message),
                base: None,
                ours: None,
                theirs: None,
                suggested_commands: suggested,
            });
        }
        return Err(OzError::Conflict {
            message: format!(
                "{} conflict(s) remain unresolved in {}",
                still_unresolved.len(),
                sandbox_dir.display()
            ),
            conflicts: entries,
            recovery,
        });
    }

    // 2. Build resolved tree from sandbox files over `base_merged_tree_id`.
    let base_tree = gix_repo
        .find_tree(base_merged_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let mut tracked_entries: BTreeMap<String, (ObjectId, EntryKind)> = BTreeMap::new();
    let files = base_tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|err| OzError::Git(err.to_string()))?;
    for f in files {
        if f.mode.is_tree() || f.mode.is_commit() {
            continue;
        }
        let rel = f.filepath.to_str_lossy().into_owned();
        tracked_entries.insert(rel, (f.oid, normalize_entry_kind(f.mode.kind())));
    }

    let mut editor = gix_repo
        .edit_tree(base_merged_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let mut marker_conflicts = Vec::new();
    let hash_kind = gix_repo.object_hash();

    // Remove deleted files first to handle file<->directory transitions cleanly.
    for rel in tracked_entries.keys() {
        let disk_path = sandbox_dir.join(rel);
        match fs::symlink_metadata(&disk_path) {
            Ok(meta) if !meta.is_dir() => {}
            _ => {
                editor
                    .remove(rel.as_str())
                    .map_err(|err| OzError::Git(err.to_string()))?;
            }
        }
    }

    // Upsert modified tracked files.
    for (rel, (orig_oid, orig_kind)) in &tracked_entries {
        let disk_path = sandbox_dir.join(rel);
        let Ok(meta) = fs::symlink_metadata(&disk_path) else {
            continue;
        };
        if meta.is_dir() {
            continue;
        }
        let (bytes, disk_kind) = read_disk_bytes_and_kind(&disk_path, &meta)?;
        if disk_kind != EntryKind::Link && bytes_contain_conflict_markers(&bytes) {
            marker_conflicts.push(rel.clone());
            continue;
        }
        let disk_oid = gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, &bytes)
            .map_err(|err| OzError::Git(err.to_string()))?;
        if disk_oid != *orig_oid || disk_kind != *orig_kind {
            let written = gix_repo
                .write_blob(&bytes)
                .map_err(|err| OzError::Git(err.to_string()))?
                .detach();
            editor
                .upsert(rel.as_str(), disk_kind, written)
                .map_err(|err| OzError::Git(err.to_string()))?;
        }
    }

    // Walk sandbox for any newly created non-ignored, non-warm-cache files.
    let mut warm_roots: BTreeSet<String> =
        WARM_CACHE_ROOTS.iter().map(|s| (*s).to_string()).collect();
    for p in &repo.config.warm.paths {
        warm_roots.insert(p.trim_end_matches('/').to_string());
    }
    let mut ignore_buf = Vec::new();
    let mut ignore_search = gix::ignore::Search::from_git_dir(
        &repo.git_common_dir,
        None,
        &mut ignore_buf,
        Default::default(),
    )
    .unwrap_or_default();
    let root_gi = sandbox_dir.join(".gitignore");
    if root_gi.is_file()
        && let Ok(bytes) = fs::read(&root_gi)
    {
        let _ = ignore_search.add_patterns_buffer(
            &bytes,
            &root_gi,
            Some(sandbox_dir),
            Default::default(),
        );
    }

    let mut new_files = Vec::new();
    collect_new_sandbox_files(
        sandbox_dir,
        sandbox_dir,
        &warm_roots,
        &tracked_entries,
        &mut ignore_search,
        &mut new_files,
    )?;

    for (rel, disk_path) in new_files {
        let meta = fs::symlink_metadata(&disk_path)?;
        let (bytes, disk_kind) = read_disk_bytes_and_kind(&disk_path, &meta)?;
        if disk_kind != EntryKind::Link && bytes_contain_conflict_markers(&bytes) {
            marker_conflicts.push(rel);
            continue;
        }
        let blob_id = gix_repo
            .write_blob(&bytes)
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach();
        editor
            .upsert(rel.as_str(), disk_kind, blob_id)
            .map_err(|err| OzError::Git(err.to_string()))?;
    }

    if !marker_conflicts.is_empty() {
        marker_conflicts.sort();
        marker_conflicts.dedup();
        let conflict_entries: Vec<ConflictEntry> = marker_conflicts
            .iter()
            .enumerate()
            .map(|(i, path)| ConflictEntry {
                id: format!("m{}", i + 1),
                path: path.clone(),
                message: "unresolved conflict markers remain in file".to_string(),
                base: None,
                ours: None,
                theirs: None,
                suggested_commands: vec![format!("Edit {}/{}", sandbox_dir.display(), path)],
            })
            .collect();
        return Err(OzError::Conflict {
            message: format!(
                "unresolved conflict markers remain in: {}",
                marker_conflicts.join(", ")
            ),
            conflicts: conflict_entries,
            recovery: vec![
                "Remove all conflict markers (<<<<<<<, =======, >>>>>>>) or run `oz resolve <id> --take ours|theirs`."
                    .to_string(),
            ],
        });
    }

    let resolved_tree_id = editor
        .write()
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();
    Ok(resolved_tree_id)
}

fn collect_new_sandbox_files(
    sandbox_root: &Path,
    dir: &Path,
    warm_roots: &BTreeSet<String>,
    tracked_entries: &BTreeMap<String, (ObjectId, EntryKind)>,
    ignore_search: &mut gix::ignore::Search,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    let prev_patterns_len = ignore_search.patterns.len();
    if dir != sandbox_root {
        let sub_gi = dir.join(".gitignore");
        if sub_gi.is_file()
            && let Ok(bytes) = fs::read(&sub_gi)
        {
            let _ = ignore_search.add_patterns_buffer(
                &bytes,
                &sub_gi,
                Some(sandbox_root),
                Default::default(),
            );
        }
    }

    let res = (|| -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name_os = entry.file_name();
            let Some(name) = name_os.to_str() else {
                continue;
            };
            if name == ".git" {
                continue;
            }
            let path = entry.path();
            let Ok(rel_path) = path.strip_prefix(sandbox_root) else {
                continue;
            };
            let Some(rel) = path_to_rel_string(rel_path) else {
                continue;
            };
            let meta = fs::symlink_metadata(&path)?;
            let is_dir = meta.is_dir();

            if is_dir && dir == sandbox_root && warm_roots.contains(name) {
                continue;
            }
            if is_ignored_in_sandbox(ignore_search, &rel, is_dir)
                && !tracked_entries.contains_key(&rel)
            {
                continue;
            }

            if is_dir {
                collect_new_sandbox_files(
                    sandbox_root,
                    &path,
                    warm_roots,
                    tracked_entries,
                    ignore_search,
                    out,
                )?;
            } else if !tracked_entries.contains_key(&rel) {
                out.push((rel, path));
            }
        }
        Ok(())
    })();

    ignore_search.patterns.truncate(prev_patterns_len);
    res
}

fn is_ignored_in_sandbox(search: &gix::ignore::Search, rel: &str, is_dir: bool) -> bool {
    search
        .pattern_matching_relative_path(
            rel.as_bytes().as_bstr(),
            Some(is_dir),
            gix::glob::pattern::Case::Sensitive,
        )
        .is_some_and(|m| !m.pattern.is_negative())
}

fn read_disk_bytes_and_kind(path: &Path, meta: &fs::Metadata) -> Result<(Vec<u8>, EntryKind)> {
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        return Ok((target.as_os_str().as_encoded_bytes().to_vec(), EntryKind::Link));
    }
    let bytes = fs::read(path)?;
    #[cfg(unix)]
    let kind = {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            EntryKind::BlobExecutable
        } else {
            EntryKind::Blob
        }
    };
    #[cfg(not(unix))]
    let kind = EntryKind::Blob;
    Ok((bytes, kind))
}

fn normalize_entry_kind(kind: EntryKind) -> EntryKind {
    match kind {
        EntryKind::BlobExecutable => EntryKind::BlobExecutable,
        EntryKind::Link => EntryKind::Link,
        _ => EntryKind::Blob,
    }
}

fn path_to_rel_string(rel: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(s) => {
                parts.push(s.to_str()?);
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// Executes `--verify "<cmd>"` non-interactively inside `sandbox_dir`.
pub(crate) fn run_verify_command(sandbox_dir: &Path, verify_cmd: &str) -> Result<()> {
    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("sh");
        c.args(["-c", verify_cmd]);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt as _;
        let mut c = Command::new("cmd.exe");
        c.raw_arg("/C");
        c.raw_arg(format!("\"{verify_cmd}\""));
        c
    };

    cmd.current_dir(sandbox_dir)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GCM_INTERACTIVE", "never")
        .env("OZ_AGENT", "1");

    let output = cmd.output().map_err(|err| OzError::CheckFailed {
        message: format!("failed to execute --verify command '{verify_cmd}': {err}"),
        recovery: vec!["Check the --verify command syntax and executable availability.".to_string()],
    })?;

    if !output.status.success() {
        let code = output
            .status
            .code()
            .map_or_else(|| "signal".to_string(), |c| c.to_string());
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let snippet = if !stderr.trim().is_empty() {
            truncate_excerpt(stderr.trim(), 512)
        } else {
            truncate_excerpt(stdout.trim(), 512)
        };
        let detail = if snippet.is_empty() {
            String::new()
        } else {
            format!(": {snippet}")
        };
        return Err(OzError::CheckFailed {
            message: format!(
                "--verify command '{verify_cmd}' failed with exit code {code}{detail}"
            ),
            recovery: vec![
                "Fix the failing verification check in the source workspace and retry `oz integrate`."
                    .to_string(),
            ],
        });
    }

    Ok(())
}

fn truncate_excerpt(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// Collects leaf file entries (`path -> (oid, EntryMode)`) from `tree_id`.
pub(crate) fn collect_tree_leaf_map(
    gix_repo: &gix::Repository,
    tree_id: ObjectId,
) -> Result<BTreeMap<String, (ObjectId, EntryMode)>> {
    let mut out = BTreeMap::new();
    if tree_id == gix_repo.empty_tree().id().detach() {
        return Ok(out);
    }
    let tree = gix_repo
        .find_tree(tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let files = tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|err| OzError::Git(err.to_string()))?;
    for f in files {
        if f.mode.is_tree() || f.mode.is_commit() {
            continue;
        }
        out.insert(f.filepath.to_str_lossy().into_owned(), (f.oid, f.mode));
    }
    Ok(out)
}

/// Atomically writes a [`WorkspaceState`] JSON file via temp file + rename.
pub(crate) fn write_workspace_state_atomic(path: &Path, state: &WorkspaceState) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| OzError::Other(format!("path {} has no parent", path.display())))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".tmp_{}_{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("ws.json"),
        std::process::id()
    ));
    let bytes = serde_json::to_vec_pretty(state)?;
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}
