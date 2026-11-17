// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Indexless `O(dirty_files)` working-tree status, `ScopePolicy` (`globset`), and
//! FUSE3 `upper/` + `meta/{whiteouts,opaque}` fast path.

use crate::diff::{compute_file_diff, file_id};
use crate::error::{OzError, Result};
use crate::repo::OzoneRepo;
use crate::types::{ChangeKind, FileChange};
use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Structured working-tree status summary returned by [`compute_status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub workspace: Option<String>,
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub base_tree: Option<String>,
    pub used_fuse_fast_path: bool,
    pub changes: Vec<FileChange>,
}

impl StatusReport {
    /// Returns `true` if there are no uncommitted changes in the workspace.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.changes.is_empty()
    }

    /// Finds a file change by deterministic file ID (`f1`) or relative path.
    #[must_use]
    pub fn find_by_target(&self, target: &str) -> Option<&FileChange> {
        let norm = crate::diff::normalize_target_path(target);
        self.changes
            .iter()
            .find(|c| c.id == target || (!norm.is_empty() && c.path == norm))
    }
}

/// Compiled workspace `--scope` and `--deny` path matcher (`globset`).
#[derive(Debug, Clone)]
pub struct ScopePolicy {
    scope: GlobSet,
    has_scope: bool,
    deny: GlobSet,
}

impl ScopePolicy {
    /// Builds a [`ScopePolicy`] combining `WorkspaceState` and `OzoneConfig`.
    pub fn from_repo(repo: &OzoneRepo) -> Result<Self> {
        let scope_patterns: &[String] = match repo.workspace_state {
            Some(ref ws) if !ws.scope.is_empty() => &ws.scope,
            _ => &repo.config.guardrails.scope,
        };

        let mut deny_patterns: Vec<String> = repo.config.guardrails.deny.clone();
        if let Some(ref ws) = repo.workspace_state {
            deny_patterns.extend(ws.deny.iter().cloned());
        }

        let scope = compile_globset(scope_patterns, "scope")?;
        let deny = compile_globset(&deny_patterns, "deny")?;

        Ok(Self {
            scope,
            has_scope: !scope_patterns.is_empty(),
            deny,
        })
    }

    /// Returns `true` if `rel_path` violates workspace `scope` or `deny` rules.
    #[must_use]
    pub fn is_out_of_scope(&self, rel_path: &str) -> bool {
        self.violation_reason(rel_path).is_some()
    }

    /// Returns a human-readable violation reason if `rel_path` is out of scope or denied.
    #[must_use]
    pub fn violation_reason(&self, rel_path: &str) -> Option<String> {
        let norm = rel_path.replace('\\', "/");
        if self.deny.is_match(&norm) {
            return Some(format!("'{norm}' matches deny guardrail pattern"));
        }
        if self.has_scope && !self.scope.is_match(&norm) {
            return Some(format!("'{norm}' is outside workspace allowed scope"));
        }
        None
    }
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

/// Resolved HEAD state for the active workspace or repository.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceHead {
    pub branch: Option<String>,
    pub commit_oid: Option<gix::ObjectId>,
    pub tree_id: gix::ObjectId,
}

/// Source root on host disk where active workspace file edits live.
#[derive(Debug, Clone)]
pub(crate) enum WorktreeSource {
    FuseOverlay {
        upper_dir: PathBuf,
    },
    Directory {
        workdir_path: PathBuf,
    },
}

impl WorktreeSource {
    /// Resolves the host path where a modified/added path resides on disk.
    pub fn disk_path(&self, rel_path: &str) -> PathBuf {
        match self {
            Self::FuseOverlay { upper_dir } => upper_dir.join(rel_path),
            Self::Directory { workdir_path } => workdir_path.join(rel_path),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct HeadTreeEntry {
    pub oid: gix::ObjectId,
    pub kind: EntryKind,
}

#[derive(Debug, Clone)]
pub(crate) struct RawDirtyFile {
    pub path: String,
    pub kind: ChangeKind,
    pub disk_kind: Option<EntryKind>,
    pub head_entry: Option<HeadTreeEntry>,
}

struct DirtyWithPayload {
    raw: RawDirtyFile,
    old_bytes: Vec<u8>,
    new_bytes: Vec<u8>,
    known_binary: bool,
}

/// Internal status state shared with `diff`, `guardrails`, and `commit`.
pub(crate) struct StatusContext {
    pub head: WorkspaceHead,
    pub source: WorktreeSource,
    pub scope_policy: ScopePolicy,
    pub dirty_files: Vec<RawDirtyFile>,
    pub changes: Vec<FileChange>,
}

/// Opens a `gix::Repository` suitable for reading objects and refs of `repo`.
pub(crate) fn open_gix_for_workspace(repo: &OzoneRepo) -> Result<gix::Repository> {
    let target = if repo.is_linked_worktree && repo.git_dir.exists() {
        &repo.git_dir
    } else {
        &repo.repo_root
    };
    gix::open(target).map_err(|err| OzError::Git(err.to_string()))
}

/// Resolves the current commit and root tree for the active workspace branch (handling unborn HEAD).
pub(crate) fn resolve_workspace_head(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
) -> Result<WorkspaceHead> {
    if let Some(ref branch_name) = repo.branch {
        let ref_name = format!("refs/heads/{branch_name}");
        if let Some(mut reference) = gix_repo
            .try_find_reference(&ref_name)
            .map_err(|err| OzError::Git(err.to_string()))?
        {
            let commit = reference
                .peel_to_commit()
                .map_err(|err| OzError::Git(err.to_string()))?;
            let tree_id = commit
                .tree_id()
                .map_err(|err| OzError::Git(err.to_string()))?
                .detach();
            return Ok(WorkspaceHead {
                branch: Some(branch_name.clone()),
                commit_oid: Some(commit.id),
                tree_id,
            });
        }
        return Ok(WorkspaceHead {
            branch: Some(branch_name.clone()),
            commit_oid: None,
            tree_id: gix_repo.empty_tree().id().detach(),
        });
    }

    if repo.is_linked_worktree || repo.workspace.is_none() {
        let head = gix_repo
            .head()
            .map_err(|err| OzError::Git(err.to_string()))?;
        if head.is_unborn() {
            return Ok(WorkspaceHead {
                branch: None,
                commit_oid: None,
                tree_id: gix_repo.empty_tree().id().detach(),
            });
        }
        let commit = head
            .into_peeled_id()
            .map_err(|err| OzError::Git(err.to_string()))?
            .object()
            .map_err(|err| OzError::Git(err.to_string()))?
            .try_into_commit()
            .map_err(|err| OzError::Git(err.to_string()))?;
        let tree_id = commit
            .tree_id()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach();
        return Ok(WorkspaceHead {
            branch: None,
            commit_oid: Some(commit.id),
            tree_id,
        });
    }

    if let Some(ref oid_hex) = repo.head_oid {
        let oid = gix::ObjectId::from_hex(oid_hex.as_bytes())
            .map_err(|err| OzError::Git(err.to_string()))?;
        let commit = gix_repo
            .find_commit(oid)
            .map_err(|err| OzError::Git(err.to_string()))?;
        let tree_id = commit
            .tree_id()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach();
        return Ok(WorkspaceHead {
            branch: None,
            commit_oid: Some(commit.id),
            tree_id,
        });
    }

    Ok(WorkspaceHead {
        branch: None,
        commit_oid: None,
        tree_id: gix_repo.empty_tree().id().detach(),
    })
}

/// Computes indexless `O(dirty_files)` working-tree status for `repo`.
pub fn compute_status(repo: &OzoneRepo) -> Result<StatusReport> {
    let ctx = compute_status_context(repo)?;
    Ok(StatusReport {
        workspace: repo.workspace.clone(),
        branch: ctx.head.branch,
        head_oid: ctx.head.commit_oid.map(|id| id.to_hex().to_string()),
        base_tree: Some(ctx.head.tree_id.to_hex().to_string()),
        used_fuse_fast_path: matches!(ctx.source, WorktreeSource::FuseOverlay { .. }),
        changes: ctx.changes,
    })
}

pub(crate) fn compute_status_context(repo: &OzoneRepo) -> Result<StatusContext> {
    let gix_repo = open_gix_for_workspace(repo)?;
    let head = resolve_workspace_head(repo, &gix_repo)?;
    let scope_policy = ScopePolicy::from_repo(repo)?;

    let (source, mut dirty_items) = if let Some((upper_dir, meta_dir)) = detect_fuse_dirs(repo) {
        let dirty = scan_fuse_fast_path(repo, &gix_repo, &head, &upper_dir, &meta_dir)?;
        (WorktreeSource::FuseOverlay { upper_dir }, dirty)
    } else {
        let dirty = scan_general_workdir(repo, &gix_repo, &head)?;
        (
            WorktreeSource::Directory {
                workdir_path: repo.workdir_path.clone(),
            },
            dirty,
        )
    };

    dirty_items.sort_by(|a, b| a.raw.path.cmp(&b.raw.path));

    // Fetch any missing HEAD blobs sequentially on the single gix_repo before parallel diffing.
    for item in &mut dirty_items {
        if !item.known_binary && item.old_bytes.is_empty()
            && let Some(ref he) = item.raw.head_entry
        {
            item.old_bytes = read_blob_bytes(&gix_repo, he.oid)?;
        }
    }

    let pinned_set = crate::commit::read_pinned_paths(repo)?;

    let changes: Vec<FileChange> = dirty_items
        .par_iter()
        .enumerate()
        .map(|(idx, item)| {
            let file_num = idx + 1;
            let fid = file_id(file_num);
            let pinned = pinned_set.contains(&item.raw.path);
            let out_of_scope = scope_policy.is_out_of_scope(&item.raw.path);

            if item.known_binary {
                return FileChange {
                    id: fid,
                    path: item.raw.path.clone(),
                    old_path: None,
                    kind: item.raw.kind,
                    additions: 0,
                    deletions: 0,
                    is_binary: true,
                    pinned,
                    out_of_scope,
                    hunks: Vec::new(),
                };
            }

            let diff = compute_file_diff(
                &fid,
                file_num,
                &item.raw.path,
                &item.old_bytes,
                &item.new_bytes,
            );
            FileChange {
                id: fid,
                path: item.raw.path.clone(),
                old_path: None,
                kind: item.raw.kind,
                additions: diff.additions,
                deletions: diff.deletions,
                is_binary: diff.is_binary,
                pinned,
                out_of_scope,
                hunks: diff.hunks,
            }
        })
        .collect();

    let dirty_files = dirty_items.into_iter().map(|d| d.raw).collect();

    Ok(StatusContext {
        head,
        source,
        scope_policy,
        dirty_files,
        changes,
    })
}

fn detect_fuse_dirs(repo: &OzoneRepo) -> Option<(PathBuf, PathBuf)> {
    if let Some(ref mr) = repo.mount_record
        && (mr.backend.requires_daemon()
            || (mr.upper_dir.is_some() && mr.meta_dir.is_some()))
    {
        let upper = mr
            .upper_dir
            .clone()
            .unwrap_or_else(|| repo.layout.ws_upper_dir(&mr.workspace));
        let meta = mr
            .meta_dir
            .clone()
            .unwrap_or_else(|| repo.layout.ws_tombstone_dir(&mr.workspace));
        return Some((upper, meta));
    }
    if let Some(ref ws) = repo.workspace_state
        && ws.backend.requires_daemon()
    {
        return Some((
            repo.layout.ws_upper_dir(&ws.name),
            repo.layout.ws_tombstone_dir(&ws.name),
        ));
    }
    None
}

// ============================================================================
// FUSE3 O(dirty_files) Fast Path
// ============================================================================

fn scan_fuse_fast_path(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    head: &WorkspaceHead,
    upper_dir: &Path,
    meta_dir: &Path,
) -> Result<Vec<DirtyWithPayload>> {
    let head_tree = gix_repo
        .find_tree(head.tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;

    let whiteouts_dir = meta_dir.join("whiteouts");
    let opaque_dir = meta_dir.join("opaque");

    let whiteout_paths = collect_whiteout_paths(&whiteouts_dir)?;
    let opaque_dirs = collect_opaque_dirs(&opaque_dir)?;

    let mut ignore_eval = FuseIgnoreEvaluator::new(
        repo,
        upper_dir,
        &head_tree,
        &whiteout_paths,
        &opaque_dirs,
    );

    let mut upper_files: BTreeMap<String, (PathBuf, EntryKind)> = BTreeMap::new();
    let mut upper_dirs: Vec<String> = Vec::new();
    if upper_dir.is_dir() {
        walk_upper_dir(
            upper_dir,
            "",
            &head_tree,
            &mut ignore_eval,
            &mut upper_files,
            &mut upper_dirs,
        )?;
    }

    let mut dirty_map: BTreeMap<String, DirtyWithPayload> = BTreeMap::new();

    // Check if any file in HEAD was replaced by a directory in upper_dir.
    for dir_rel in &upper_dirs {
        if let Some(entry) = lookup_tree_entry(&head_tree, dir_rel)?
            && !entry.mode().is_tree()
        {
            dirty_map.insert(
                dir_rel.clone(),
                DirtyWithPayload {
                    raw: RawDirtyFile {
                        path: dir_rel.clone(),
                        kind: ChangeKind::Deleted,
                        disk_kind: None,
                        head_entry: Some(HeadTreeEntry {
                            oid: entry.object_id(),
                            kind: entry.mode().kind(),
                        }),
                    },
                    old_bytes: Vec::new(),
                    new_bytes: Vec::new(),
                    known_binary: false,
                },
            );
        }
    }

    // Compare each upper file against HEAD tree (O(dirty_files) — never scanning untouched baseline!).
    for (rel_path, (abs_path, disk_kind)) in &upper_files {
        match lookup_tree_entry(&head_tree, rel_path)? {
            Some(entry) if entry.mode().is_tree() => {
                // Directory in HEAD was replaced by a file/symlink in upper_dir.
                let subtree = entry
                    .object()
                    .map_err(|err| OzError::Git(err.to_string()))?
                    .try_into_tree()
                    .map_err(|err| OzError::Git(err.to_string()))?;
                collect_deleted_from_subtree(&subtree, rel_path, &upper_files, &mut dirty_map)?;
                let (new_bytes, known_binary) = read_added_payload(abs_path, *disk_kind)?;
                dirty_map.insert(
                    rel_path.clone(),
                    DirtyWithPayload {
                        raw: RawDirtyFile {
                            path: rel_path.clone(),
                            kind: ChangeKind::Added,
                            disk_kind: Some(*disk_kind),
                            head_entry: None,
                        },
                        old_bytes: Vec::new(),
                        new_bytes,
                        known_binary,
                    },
                );
            }
            Some(entry) => {
                let head_kind = normalize_entry_kind(entry.mode().kind());
                let disk_bytes = read_disk_bytes(abs_path, Some(*disk_kind))?;
                let head_bytes = read_blob_bytes(gix_repo, entry.object_id())?;
                let eff_disk_kind = effective_disk_kind(*disk_kind, Some(head_kind));

                if disk_bytes == head_bytes && eff_disk_kind == head_kind {
                    // Committed file still in upper_dir — identical to HEAD, so CLEAN!
                    continue;
                }

                let kind = if (eff_disk_kind == EntryKind::Link) != (head_kind == EntryKind::Link) {
                    ChangeKind::TypeChanged
                } else {
                    ChangeKind::Modified
                };
                dirty_map.insert(
                    rel_path.clone(),
                    DirtyWithPayload {
                        raw: RawDirtyFile {
                            path: rel_path.clone(),
                            kind,
                            disk_kind: Some(eff_disk_kind),
                            head_entry: Some(HeadTreeEntry {
                                oid: entry.object_id(),
                                kind: head_kind,
                            }),
                        },
                        old_bytes: head_bytes,
                        new_bytes: disk_bytes,
                        known_binary: false,
                    },
                );
            }
            None => {
                if ignore_eval.is_ignored(rel_path, false) {
                    continue;
                }
                let (new_bytes, known_binary) = read_added_payload(abs_path, *disk_kind)?;
                dirty_map.insert(
                    rel_path.clone(),
                    DirtyWithPayload {
                        raw: RawDirtyFile {
                            path: rel_path.clone(),
                            kind: ChangeKind::Added,
                            disk_kind: Some(*disk_kind),
                            head_entry: None,
                        },
                        old_bytes: Vec::new(),
                        new_bytes,
                        known_binary,
                    },
                );
            }
        }
    }

    // Check whiteouts (`meta/whiteouts/<rel_path>.oz_wh`).
    for wh_path in &whiteout_paths {
        if upper_files.contains_key(wh_path) {
            continue;
        }
        let Some(entry) = lookup_tree_entry(&head_tree, wh_path)? else {
            continue;
        };
        if entry.mode().is_tree() {
            let subtree = entry
                .object()
                .map_err(|err| OzError::Git(err.to_string()))?
                .try_into_tree()
                .map_err(|err| OzError::Git(err.to_string()))?;
            collect_deleted_from_subtree(&subtree, wh_path, &upper_files, &mut dirty_map)?;
        } else {
            dirty_map.insert(
                wh_path.clone(),
                DirtyWithPayload {
                    raw: RawDirtyFile {
                        path: wh_path.clone(),
                        kind: ChangeKind::Deleted,
                        disk_kind: None,
                        head_entry: Some(HeadTreeEntry {
                            oid: entry.object_id(),
                            kind: normalize_entry_kind(entry.mode().kind()),
                        }),
                    },
                    old_bytes: Vec::new(),
                    new_bytes: Vec::new(),
                    known_binary: false,
                },
            );
        }
    }

    // Check opaque directories (`meta/opaque/<rel_path>.oz_opaque`).
    for op_dir in &opaque_dirs {
        if op_dir.is_empty() {
            collect_deleted_from_subtree(&head_tree, "", &upper_files, &mut dirty_map)?;
            continue;
        }
        if let Some(entry) = lookup_tree_entry(&head_tree, op_dir)?
            && entry.mode().is_tree()
        {
            let subtree = entry
                .object()
                .map_err(|err| OzError::Git(err.to_string()))?
                .try_into_tree()
                .map_err(|err| OzError::Git(err.to_string()))?;
            collect_deleted_from_subtree(&subtree, op_dir, &upper_files, &mut dirty_map)?;
        }
    }

    Ok(dirty_map.into_values().collect())
}

fn collect_deleted_from_subtree(
    subtree: &gix::Tree<'_>,
    prefix: &str,
    upper_files: &BTreeMap<String, (PathBuf, EntryKind)>,
    dirty_map: &mut BTreeMap<String, DirtyWithPayload>,
) -> Result<()> {
    let entries = subtree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|err| OzError::Git(err.to_string()))?;
    for entry in entries {
        if entry.mode.is_tree() || entry.mode.kind() == EntryKind::Commit {
            continue;
        }
        let sub_rel = entry.filepath.to_str_lossy();
        let full_rel = if prefix.is_empty() {
            sub_rel.into_owned()
        } else {
            format!("{prefix}/{sub_rel}")
        };
        if !upper_files.contains_key(&full_rel) {
            dirty_map
                .entry(full_rel.clone())
                .or_insert_with(|| DirtyWithPayload {
                    raw: RawDirtyFile {
                        path: full_rel,
                        kind: ChangeKind::Deleted,
                        disk_kind: None,
                        head_entry: Some(HeadTreeEntry {
                            oid: entry.oid,
                            kind: normalize_entry_kind(entry.mode.kind()),
                        }),
                    },
                    old_bytes: Vec::new(),
                    new_bytes: Vec::new(),
                    known_binary: false,
                });
        }
    }
    Ok(())
}

fn walk_upper_dir(
    dir_abs: &Path,
    rel_prefix: &str,
    head_tree: &gix::Tree<'_>,
    ignore_eval: &mut FuseIgnoreEvaluator<'_, '_>,
    files: &mut BTreeMap<String, (PathBuf, EntryKind)>,
    dirs: &mut Vec<String>,
) -> Result<()> {
    for item in fs::read_dir(dir_abs)? {
        let entry = item?;
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        if name == ".git" {
            continue;
        }

        let abs_path = entry.path();
        let meta = fs::symlink_metadata(&abs_path)?;
        let rel_path = if rel_prefix.is_empty() {
            name.to_string()
        } else {
            format!("{rel_prefix}/{name}")
        };

        if meta.file_type().is_dir() {
            let is_tracked_dir = lookup_tree_entry(head_tree, &rel_path)?
                .is_some_and(|e| e.mode().is_tree());
            if !is_tracked_dir && ignore_eval.is_ignored(&rel_path, true) {
                continue;
            }
            dirs.push(rel_path.clone());
            walk_upper_dir(&abs_path, &rel_path, head_tree, ignore_eval, files, dirs)?;
        } else {
            let kind = disk_entry_kind(&meta);
            files.insert(rel_path, (abs_path, kind));
        }
    }
    Ok(())
}

fn collect_whiteout_paths(whiteouts_dir: &Path) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    if !whiteouts_dir.is_dir() {
        return Ok(out);
    }
    collect_marker_files(whiteouts_dir, "", &mut |rel| {
        if let Some(stripped) = rel.strip_suffix(".oz_wh")
            && !stripped.is_empty()
        {
            out.insert(stripped.to_string());
        }
    })?;
    Ok(out)
}

fn collect_opaque_dirs(opaque_dir: &Path) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    if !opaque_dir.is_dir() {
        return Ok(out);
    }
    collect_marker_files(opaque_dir, "", &mut |rel| {
        if rel == ".oz_opaque" {
            out.insert(String::new());
        } else if let Some(dir) = rel.strip_suffix("/.oz_opaque") {
            out.insert(dir.to_string());
        } else if let Some(dir) = rel.strip_suffix(".oz_opaque") {
            out.insert(dir.to_string());
        } else if !rel.is_empty() {
            out.insert(rel.to_string());
        }
    })?;
    Ok(out)
}

fn collect_marker_files<F: FnMut(&str)>(dir: &Path, prefix: &str, on_file: &mut F) -> Result<()> {
    for item in fs::read_dir(dir)? {
        let entry = item?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if meta.file_type().is_dir() {
            collect_marker_files(&path, &rel, on_file)?;
        } else {
            on_file(&rel);
        }
    }
    Ok(())
}

struct FuseIgnoreEvaluator<'a, 'repo> {
    upper_dir: &'a Path,
    seed_dir: Option<PathBuf>,
    head_tree: &'a gix::Tree<'repo>,
    whiteouts: &'a BTreeSet<String>,
    opaque_dirs: &'a BTreeSet<String>,
    exclude_gi: Gitignore,
    global_gi: Gitignore,
    dir_cache: HashMap<String, Option<Gitignore>>,
}

impl<'a, 'repo> FuseIgnoreEvaluator<'a, 'repo> {
    fn new(
        repo: &OzoneRepo,
        upper_dir: &'a Path,
        head_tree: &'a gix::Tree<'repo>,
        whiteouts: &'a BTreeSet<String>,
        opaque_dirs: &'a BTreeSet<String>,
    ) -> Self {
        let exclude_path = repo.git_common_dir.join("info/exclude");
        let exclude_gi = if exclude_path.is_file() {
            Gitignore::new(&exclude_path).0
        } else {
            Gitignore::empty()
        };
        let global_gi = Gitignore::global().0;
        let seed_dir = upper_dir
            .parent()
            .map(|p| p.join("seed"))
            .filter(|p| p.is_dir());

        Self {
            upper_dir,
            seed_dir,
            head_tree,
            whiteouts,
            opaque_dirs,
            exclude_gi,
            global_gi,
            dir_cache: HashMap::new(),
        }
    }

    fn is_ignored(&mut self, rel_path: &str, is_dir: bool) -> bool {
        let mut ignored = false;

        if !self.global_gi.is_empty() {
            match self.global_gi.matched_path_or_any_parents(rel_path, is_dir) {
                ignore::Match::Ignore(_) => ignored = true,
                ignore::Match::Whitelist(_) => ignored = false,
                ignore::Match::None => {}
            }
        }
        if !self.exclude_gi.is_empty() {
            match self.exclude_gi.matched_path_or_any_parents(rel_path, is_dir) {
                ignore::Match::Ignore(_) => ignored = true,
                ignore::Match::Whitelist(_) => ignored = false,
                ignore::Match::None => {}
            }
        }

        // Evaluate `.gitignore` from root ("") down to the parent directory of `rel_path`.
        let mut dirs = vec![String::new()];
        let parts: Vec<&str> = rel_path.split('/').collect();
        if parts.len() > 1 {
            let mut acc = String::new();
            for comp in &parts[..parts.len() - 1] {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(comp);
                dirs.push(acc.clone());
            }
        }

        for dir in dirs {
            if let Some(gi) = self.load_dir_gitignore(&dir) {
                match gi.matched_path_or_any_parents(rel_path, is_dir) {
                    ignore::Match::Ignore(_) => ignored = true,
                    ignore::Match::Whitelist(_) => ignored = false,
                    ignore::Match::None => {}
                }
            }
        }

        ignored
    }

    fn load_dir_gitignore(&mut self, dir: &str) -> Option<&Gitignore> {
        if !self.dir_cache.contains_key(dir) {
            let loaded = self.build_dir_gitignore(dir);
            self.dir_cache.insert(dir.to_string(), loaded);
        }
        self.dir_cache.get(dir).and_then(Option::as_ref)
    }

    fn build_dir_gitignore(&self, dir: &str) -> Option<Gitignore> {
        let gi_rel = if dir.is_empty() {
            ".gitignore".to_string()
        } else {
            format!("{dir}/.gitignore")
        };

        let upper_gi = self.upper_dir.join(&gi_rel);
        if upper_gi.is_file() {
            if let Ok(content) = fs::read_to_string(&upper_gi) {
                return parse_gitignore_str(dir, &content);
            }
            return None;
        }

        if self.is_hidden_in_lower(dir, &gi_rel) {
            return None;
        }

        if let Ok(Some(entry)) = lookup_tree_entry(self.head_tree, &gi_rel)
            && !entry.mode().is_tree()
            && let Ok(obj) = entry.object()
        {
            let content = String::from_utf8_lossy(&obj.data);
            return parse_gitignore_str(dir, &content);
        }

        if let Some(ref seed) = self.seed_dir {
            let seed_gi = seed.join(&gi_rel);
            if seed_gi.is_file()
                && let Ok(content) = fs::read_to_string(&seed_gi)
            {
                return parse_gitignore_str(dir, &content);
            }
        }

        None
    }

    fn is_hidden_in_lower(&self, dir: &str, gi_rel: &str) -> bool {
        if self.whiteouts.contains(gi_rel) || self.opaque_dirs.contains("") {
            return true;
        }
        if !dir.is_empty() {
            let mut cur = String::new();
            for comp in dir.split('/') {
                if !cur.is_empty() {
                    cur.push('/');
                }
                cur.push_str(comp);
                if self.whiteouts.contains(&cur) || self.opaque_dirs.contains(&cur) {
                    return true;
                }
            }
        }
        false
    }
}

fn parse_gitignore_str(dir: &str, content: &str) -> Option<Gitignore> {
    let root = if dir.is_empty() {
        Path::new(".")
    } else {
        Path::new(dir)
    };
    let mut builder = GitignoreBuilder::new(root);
    for (idx, line) in content.lines().enumerate() {
        let clean = if idx == 0 {
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        let _ = builder.add_line(None, clean);
    }
    builder.build().ok()
}

// ============================================================================
// General Workdir Path (Main repo, reflinks, CasHardlink)
// ============================================================================

fn scan_general_workdir(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    head: &WorkspaceHead,
) -> Result<Vec<DirtyWithPayload>> {
    let head_tree = gix_repo
        .find_tree(head.tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let hash_kind = gix_repo.object_hash();

    let mut head_entries: BTreeMap<String, HeadTreeEntry> = BTreeMap::new();
    let tree_files = head_tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|err| OzError::Git(err.to_string()))?;
    for entry in tree_files {
        if entry.mode.is_tree() || entry.mode.kind() == EntryKind::Commit {
            continue;
        }
        head_entries.insert(
            entry.filepath.to_str_lossy().into_owned(),
            HeadTreeEntry {
                oid: entry.oid,
                kind: normalize_entry_kind(entry.mode.kind()),
            },
        );
    }

    let mut walked_files: BTreeMap<String, (PathBuf, EntryKind)> = BTreeMap::new();
    if repo.workdir_path.is_dir() {
        let mut builder = WalkBuilder::new(&repo.workdir_path);
        builder
            .hidden(false)
            .parents(false)
            .ignore(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .follow_links(false)
            .filter_entry(|entry| entry.file_name() != ".git");

        let exclude_gi = if !repo.workdir_path.join(".git").exists()
            && repo.git_common_dir.join("info/exclude").is_file()
        {
            Some(Gitignore::new(repo.git_common_dir.join("info/exclude")).0)
        } else {
            None
        };

        for result in builder.build() {
            let entry = result.map_err(|err| OzError::Other(err.to_string()))?;
            let path = entry.path();
            if path == repo.workdir_path {
                continue;
            }
            let Ok(rel) = path.strip_prefix(&repo.workdir_path) else {
                continue;
            };
            let rel_str = path_to_slash(rel);
            if rel_str.is_empty() {
                continue;
            }
            let meta = fs::symlink_metadata(path)?;
            if meta.file_type().is_dir() {
                continue;
            }
            if let Some(ref gi) = exclude_gi
                && !head_entries.contains_key(&rel_str)
                && gi.matched_path_or_any_parents(&rel_str, false).is_ignore()
            {
                continue;
            }
            walked_files.insert(rel_str, (path.to_path_buf(), disk_entry_kind(&meta)));
        }
    }

    // Check walked files in parallel by computing Git blob SHA-1 on CPU (zero ODB lock/read for clean files!).
    let walked_vec: Vec<(String, PathBuf, EntryKind)> = walked_files
        .iter()
        .map(|(k, (p, kind))| (k.clone(), p.clone(), *kind))
        .collect();

    let mut dirty: Vec<DirtyWithPayload> = walked_vec
        .into_par_iter()
        .filter_map(|(rel_path, abs_path, disk_kind)| {
            let head_entry = head_entries.get(&rel_path).cloned();
            match compare_disk_with_head_hash(&rel_path, &abs_path, disk_kind, head_entry, hash_kind)
            {
                Ok(opt) => opt.map(Ok),
                Err(err) => Some(Err(err)),
            }
        })
        .collect::<Result<Vec<_>>>()?;

    // Check any tracked HEAD entries that were not yielded by WalkBuilder
    // (either genuinely deleted on disk, or still present on disk while matching `.gitignore`).
    let unvisited: Vec<(String, HeadTreeEntry)> = head_entries
        .iter()
        .filter(|(rel, _)| !walked_files.contains_key(rel.as_str()))
        .map(|(rel, entry)| (rel.clone(), entry.clone()))
        .collect();

    let unvisited_dirty: Vec<DirtyWithPayload> = unvisited
        .into_par_iter()
        .filter_map(|(rel_path, head_entry)| {
            let abs_path = repo.workdir_path.join(&rel_path);
            match fs::symlink_metadata(&abs_path) {
                Ok(meta) if !meta.file_type().is_dir() => {
                    let disk_kind = disk_entry_kind(&meta);
                    match compare_disk_with_head_hash(
                        &rel_path,
                        &abs_path,
                        disk_kind,
                        Some(head_entry),
                        hash_kind,
                    ) {
                        Ok(opt) => opt.map(Ok),
                        Err(err) => Some(Err(err)),
                    }
                }
                _ => Some(Ok(DirtyWithPayload {
                    raw: RawDirtyFile {
                        path: rel_path,
                        kind: ChangeKind::Deleted,
                        disk_kind: None,
                        head_entry: Some(head_entry),
                    },
                    old_bytes: Vec::new(),
                    new_bytes: Vec::new(),
                    known_binary: false,
                })),
            }
        })
        .collect::<Result<Vec<_>>>()?;

    dirty.extend(unvisited_dirty);
    Ok(dirty)
}

fn read_added_payload(abs_path: &Path, disk_kind: EntryKind) -> Result<(Vec<u8>, bool)> {
    if disk_kind != EntryKind::Link && is_file_binary_prefix(abs_path)? {
        return Ok((Vec::new(), true));
    }
    Ok((read_disk_bytes(abs_path, Some(disk_kind))?, false))
}

fn compare_disk_with_head_hash(
    rel_path: &str,
    abs_path: &Path,
    disk_kind: EntryKind,
    head_entry: Option<HeadTreeEntry>,
    hash_kind: gix::hash::Kind,
) -> Result<Option<DirtyWithPayload>> {
    match head_entry {
        Some(head) => {
            let eff_disk_kind = effective_disk_kind(disk_kind, Some(head.kind));
            let disk_bytes = read_disk_bytes(abs_path, Some(disk_kind))?;
            let disk_oid =
                gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, &disk_bytes)
                    .map_err(|err| OzError::Git(err.to_string()))?;
            if disk_oid == head.oid && eff_disk_kind == head.kind {
                return Ok(None);
            }
            let kind = if (eff_disk_kind == EntryKind::Link) != (head.kind == EntryKind::Link) {
                ChangeKind::TypeChanged
            } else {
                ChangeKind::Modified
            };
            Ok(Some(DirtyWithPayload {
                raw: RawDirtyFile {
                    path: rel_path.to_string(),
                    kind,
                    disk_kind: Some(eff_disk_kind),
                    head_entry: Some(head),
                },
                old_bytes: Vec::new(),
                new_bytes: disk_bytes,
                known_binary: false,
            }))
        }
        None => {
            let (new_bytes, known_binary) = read_added_payload(abs_path, disk_kind)?;
            Ok(Some(DirtyWithPayload {
                raw: RawDirtyFile {
                    path: rel_path.to_string(),
                    kind: ChangeKind::Added,
                    disk_kind: Some(disk_kind),
                    head_entry: None,
                },
                old_bytes: Vec::new(),
                new_bytes,
                known_binary,
            }))
        }
    }
}

// ============================================================================
// Shared Tree & Disk Helpers
// ============================================================================

pub(crate) fn lookup_tree_entry<'repo>(
    tree: &gix::Tree<'repo>,
    rel_path: &str,
) -> Result<Option<gix::object::tree::Entry<'repo>>> {
    if rel_path.is_empty() {
        return Ok(None);
    }
    tree.lookup_entry(rel_path.split('/').map(str::as_bytes))
        .map_err(|err| OzError::Git(err.to_string()))
}

pub(crate) fn read_blob_bytes(gix_repo: &gix::Repository, oid: gix::ObjectId) -> Result<Vec<u8>> {
    let blob = gix_repo
        .find_blob(oid)
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(blob.data.clone())
}

pub(crate) fn read_disk_bytes(abs_path: &Path, kind_hint: Option<EntryKind>) -> Result<Vec<u8>> {
    let is_link = match kind_hint {
        Some(EntryKind::Link) => true,
        Some(_) => false,
        None => fs::symlink_metadata(abs_path)?.file_type().is_symlink(),
    };
    if is_link {
        let target = fs::read_link(abs_path)?;
        return Ok(target.as_os_str().as_encoded_bytes().to_vec());
    }
    Ok(fs::read(abs_path)?)
}

fn is_file_binary_prefix(abs_path: &Path) -> Result<bool> {
    let mut file = fs::File::open(abs_path)?;
    let mut buf = [0u8; crate::diff::BINARY_SNIFF_BYTES];
    let n = file.read(&mut buf)?;
    Ok(buf[..n].contains(&0))
}

pub(crate) fn disk_entry_kind(meta: &fs::Metadata) -> EntryKind {
    if meta.file_type().is_symlink() {
        return EntryKind::Link;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if (meta.permissions().mode() & 0o111) != 0 {
            EntryKind::BlobExecutable
        } else {
            EntryKind::Blob
        }
    }
    #[cfg(not(unix))]
    {
        EntryKind::Blob
    }
}

fn normalize_entry_kind(kind: EntryKind) -> EntryKind {
    match kind {
        EntryKind::BlobExecutable => EntryKind::BlobExecutable,
        EntryKind::Link => EntryKind::Link,
        _ => EntryKind::Blob,
    }
}

fn effective_disk_kind(disk_kind: EntryKind, head_kind: Option<EntryKind>) -> EntryKind {
    #[cfg(unix)]
    {
        let _ = head_kind;
        disk_kind
    }
    #[cfg(not(unix))]
    {
        if disk_kind == EntryKind::Blob && head_kind == Some(EntryKind::BlobExecutable) {
            EntryKind::BlobExecutable
        } else {
            disk_kind
        }
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
