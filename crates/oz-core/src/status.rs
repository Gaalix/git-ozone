// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Indexless `O(dirty_files)` working-tree status, `ScopePolicy` (`globset`), and
//! FUSE3 `upper/` + `meta/{whiteouts,opaque}` fast path.

use crate::attributes::{
    AttributeResolver, clean_blob_bytes, clean_eol_bytes, is_lfs_pointer,
};
use crate::diff::{compute_file_diff, file_id};
use crate::error::{OzError, Result};
use crate::git_config::GitConfig;
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
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

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
    let mut gix_repo = gix::open(target).map_err(|err| OzError::Git(err.to_string()))?;
    let _ = gix_repo.committer_or_set_fallback("Ozone", "oz@ozone.dev");
    Ok(gix_repo)
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
    compute_status_with_overrides(repo, &[])
}

/// Computes working-tree status with ephemeral `-c key=value` Git config overrides.
pub fn compute_status_with_overrides(
    repo: &OzoneRepo,
    overrides: &[(String, String)],
) -> Result<StatusReport> {
    let ctx = compute_status_context_with_overrides(repo, overrides)?;
    Ok(StatusReport {
        workspace: repo.workspace.clone(),
        branch: ctx.head.branch,
        head_oid: ctx.head.commit_oid.map(|id| id.to_hex().to_string()),
        base_tree: Some(ctx.head.tree_id.to_hex().to_string()),
        used_fuse_fast_path: matches!(ctx.source, WorktreeSource::FuseOverlay { .. }),
        changes: ctx.changes,
    })
}

/// Looks up `(octal_mode, oid_hex)` in `tree_hex` for a batch of repository-relative paths.
#[must_use]
pub fn lookup_tree_entries_meta(
    repo: &OzoneRepo,
    tree_hex: &str,
    paths: &[&str],
) -> BTreeMap<String, (&'static str, String)> {
    let mut out = BTreeMap::new();
    let Ok(tree_id) = gix::ObjectId::from_hex(tree_hex.as_bytes()) else {
        return out;
    };
    let Ok(gix_repo) = open_gix_for_workspace(repo) else {
        return out;
    };
    let Ok(tree) = gix_repo.find_tree(tree_id) else {
        return out;
    };
    for &p in paths {
        if let Ok(Some(entry)) = lookup_tree_entry(&tree, p) {
            let mode_str = match entry.mode().kind() {
                EntryKind::BlobExecutable => "100755",
                EntryKind::Link => "120000",
                EntryKind::Commit => "160000",
                EntryKind::Tree => "040000",
                EntryKind::Blob => "100644",
            };
            out.insert(p.to_string(), (mode_str, entry.id().to_hex().to_string()));
        }
    }
    out
}

pub(crate) fn compute_status_context(repo: &OzoneRepo) -> Result<StatusContext> {
    compute_status_context_with_overrides(repo, &[])
}

pub(crate) fn compute_status_context_with_overrides(
    repo: &OzoneRepo,
    overrides: &[(String, String)],
) -> Result<StatusContext> {
    let gix_repo = open_gix_for_workspace(repo)?;
    let git_cfg = repo.load_git_config(overrides)?;
    let head = resolve_workspace_head(repo, &gix_repo)?;
    let scope_policy = ScopePolicy::from_repo(repo)?;

    let (source, mut dirty_items) = if let Some((upper_dir, meta_dir)) = detect_fuse_dirs(repo) {
        let dirty =
            scan_fuse_fast_path(repo, &gix_repo, &git_cfg, &head, &upper_dir, &meta_dir)?;
        (WorktreeSource::FuseOverlay { upper_dir }, dirty)
    } else {
        let dirty = scan_general_workdir(repo, &gix_repo, &git_cfg, &head)?;
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
            && he.kind != EntryKind::Commit
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

type TreeDirEntries = HashMap<String, (gix::ObjectId, gix::objs::tree::EntryMode)>;

/// Lightweight per-directory cache of decoded `HEAD` tree entries so FUSE status
/// avoids repeatedly re-decoding ancestor tree objects when `upper/` contains many files
/// under the same subdirectories.
struct HeadTreeCache<'repo> {
    gix_repo: &'repo gix::Repository,
    root_tree_id: gix::ObjectId,
    /// Maps directory relative path (`""` for root, `"pkg/src"` for subdirs) to its
    /// immediate children `(name -> (oid, mode))`. `None` means the directory does not exist in HEAD.
    dirs: HashMap<String, Option<TreeDirEntries>>,
}

impl<'repo> HeadTreeCache<'repo> {
    fn new(gix_repo: &'repo gix::Repository, root_tree_id: gix::ObjectId) -> Self {
        Self {
            gix_repo,
            root_tree_id,
            dirs: HashMap::new(),
        }
    }

    fn load_dir(&mut self, dir_rel: &str) -> Result<Option<&TreeDirEntries>> {
        if !self.dirs.contains_key(dir_rel) {
            let loaded = self.resolve_dir_entries(dir_rel)?;
            self.dirs.insert(dir_rel.to_string(), loaded);
        }
        Ok(self.dirs.get(dir_rel).and_then(Option::as_ref))
    }

    fn resolve_dir_entries(&mut self, dir_rel: &str) -> Result<Option<TreeDirEntries>> {
        let tree_oid = if dir_rel.is_empty() {
            self.root_tree_id
        } else {
            let (parent, name) = match dir_rel.rsplit_once('/') {
                Some((p, n)) => (p, n),
                None => ("", dir_rel),
            };
            let Some(parent_map) = self.load_dir(parent)? else {
                return Ok(None);
            };
            let Some(&(oid, mode)) = parent_map.get(name) else {
                return Ok(None);
            };
            if !mode.is_tree() {
                return Ok(None);
            }
            oid
        };

        let tree = self
            .gix_repo
            .find_tree(tree_oid)
            .map_err(|err| OzError::Git(err.to_string()))?;
        let mut map = HashMap::new();
        for entry_res in gix::objs::TreeRefIter::from_bytes(&tree.data, tree_oid.kind()) {
            let entry = entry_res.map_err(|err| OzError::Git(err.to_string()))?;
            let name = entry.filename.to_str_lossy().into_owned();
            map.insert(name, (entry.oid.to_owned(), entry.mode));
        }
        Ok(Some(map))
    }

    fn lookup(&mut self, rel_path: &str) -> Result<Option<(gix::ObjectId, gix::objs::tree::EntryMode)>> {
        if rel_path.is_empty() {
            return Ok(None);
        }
        let (parent, name) = match rel_path.rsplit_once('/') {
            Some((p, n)) => (p, n),
            None => ("", rel_path),
        };
        let Some(dir_map) = self.load_dir(parent)? else {
            return Ok(None);
        };
        Ok(dir_map.get(name).copied())
    }
}

fn scan_fuse_fast_path(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    git_cfg: &GitConfig,
    head: &WorkspaceHead,
    upper_dir: &Path,
    meta_dir: &Path,
) -> Result<Vec<DirtyWithPayload>> {
    let head_tree = gix_repo
        .find_tree(head.tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let hash_kind = gix_repo.object_hash();
    let mut tree_cache = HeadTreeCache::new(gix_repo, head.tree_id);

    let whiteouts_dir = meta_dir.join("whiteouts");
    let opaque_dir = meta_dir.join("opaque");

    let whiteout_paths = collect_whiteout_paths(&whiteouts_dir)?;
    let opaque_dirs = collect_opaque_dirs(&opaque_dir)?;

    let attr_resolver = collect_fuse_attributes(
        repo,
        gix_repo,
        &head_tree,
        upper_dir,
        &whiteout_paths,
        &opaque_dirs,
        git_cfg,
    )?;

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
            &mut tree_cache,
            &mut ignore_eval,
            &mut upper_files,
            &mut upper_dirs,
        )?;
    }

    let mut dirty_map: BTreeMap<String, DirtyWithPayload> = BTreeMap::new();

    // Check directories in upper_dir against HEAD (gitlink updates or file->dir replacements).
    for dir_rel in &upper_dirs {
        if let Some((oid, mode)) = tree_cache.lookup(dir_rel)? {
            if mode.kind() == EntryKind::Commit {
                if let Some(item) = inspect_gitlink_dir_state(
                    dir_rel,
                    &upper_dir.join(dir_rel),
                    oid,
                    whiteout_paths.contains(dir_rel),
                )? {
                    dirty_map.insert(dir_rel.clone(), item);
                }
            } else if !mode.is_tree() {
                dirty_map.insert(
                    dir_rel.clone(),
                    DirtyWithPayload {
                        raw: RawDirtyFile {
                            path: dir_rel.clone(),
                            kind: ChangeKind::Deleted,
                            disk_kind: None,
                            head_entry: Some(HeadTreeEntry {
                                oid,
                                kind: normalize_entry_kind(mode.kind()),
                            }),
                        },
                        old_bytes: Vec::new(),
                        new_bytes: Vec::new(),
                        known_binary: false,
                    },
                );
            }
        }
    }

    // Partition upper files against cached HEAD tree lookups (O(dirty_files) — never scanning untouched baseline!).
    // Compare tracked upper files against HEAD blob OIDs in parallel on CPU (zero ODB decompression for clean files!).
    let mut tracked_checks: Vec<(String, PathBuf, EntryKind, HeadTreeEntry)> = Vec::new();
    let mut added_checks: Vec<(String, PathBuf, EntryKind)> = Vec::new();

    for (rel_path, (abs_path, disk_kind)) in &upper_files {
        match tree_cache.lookup(rel_path)? {
            Some((oid, mode)) if mode.is_tree() => {
                // Directory in HEAD was replaced by a file/symlink in upper_dir.
                let subtree = gix_repo
                    .find_tree(oid)
                    .map_err(|err| OzError::Git(err.to_string()))?;
                collect_deleted_from_subtree(&subtree, rel_path, &upper_files, &mut dirty_map)?;
                let (new_bytes, known_binary) =
                    read_added_payload(rel_path, abs_path, *disk_kind, &attr_resolver, git_cfg)?;
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
            Some((oid, mode)) => {
                tracked_checks.push((
                    rel_path.clone(),
                    abs_path.clone(),
                    *disk_kind,
                    HeadTreeEntry {
                        oid,
                        kind: normalize_entry_kind(mode.kind()),
                    },
                ));
            }
            None => {
                if ignore_eval.is_ignored(rel_path, false) {
                    continue;
                }
                added_checks.push((rel_path.clone(), abs_path.clone(), *disk_kind));
            }
        }
    }

    let tracked_dirty: Vec<DirtyWithPayload> = tracked_checks
        .into_par_iter()
        .filter_map(|(rel_path, abs_path, disk_kind, head_entry)| {
            match compare_disk_with_head_hash(
                &rel_path,
                &abs_path,
                disk_kind,
                Some(head_entry),
                hash_kind,
                &attr_resolver,
                git_cfg,
                &repo.workdir_path,
                &repo.git_dir,
            ) {
                Ok(opt) => opt.map(Ok),
                Err(err) => Some(Err(err)),
            }
        })
        .collect::<Result<Vec<_>>>()?;

    for item in tracked_dirty {
        dirty_map.insert(item.raw.path.clone(), item);
    }

    let added_dirty: Vec<DirtyWithPayload> = added_checks
        .into_par_iter()
        .map(|(rel_path, abs_path, disk_kind)| {
            let (new_bytes, known_binary) =
                read_added_payload(&rel_path, &abs_path, disk_kind, &attr_resolver, git_cfg)?;
            Ok(DirtyWithPayload {
                raw: RawDirtyFile {
                    path: rel_path,
                    kind: ChangeKind::Added,
                    disk_kind: Some(disk_kind),
                    head_entry: None,
                },
                old_bytes: Vec::new(),
                new_bytes,
                known_binary,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    for item in added_dirty {
        dirty_map.insert(item.raw.path.clone(), item);
    }

    // Check whiteouts (`meta/whiteouts/<rel_path>.oz_wh`).
    for wh_path in &whiteout_paths {
        if upper_files.contains_key(wh_path) || dirty_map.contains_key(wh_path) {
            continue;
        }
        let Some((oid, mode)) = tree_cache.lookup(wh_path)? else {
            continue;
        };
        if mode.is_tree() {
            let subtree = gix_repo
                .find_tree(oid)
                .map_err(|err| OzError::Git(err.to_string()))?;
            collect_deleted_from_subtree(&subtree, wh_path, &upper_files, &mut dirty_map)?;
        } else if mode.kind() == EntryKind::Commit {
            if !upper_dir.join(wh_path).is_dir() {
                dirty_map.insert(
                    wh_path.clone(),
                    make_gitlink_deleted_dirty(wh_path, oid),
                );
            }
        } else {
            dirty_map.insert(
                wh_path.clone(),
                DirtyWithPayload {
                    raw: RawDirtyFile {
                        path: wh_path.clone(),
                        kind: ChangeKind::Deleted,
                        disk_kind: None,
                        head_entry: Some(HeadTreeEntry {
                            oid,
                            kind: normalize_entry_kind(mode.kind()),
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
        if let Some((oid, mode)) = tree_cache.lookup(op_dir)?
            && mode.is_tree()
        {
            let subtree = gix_repo
                .find_tree(oid)
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
        if entry.mode.is_tree() {
            continue;
        }
        let sub_rel = entry.filepath.to_str_lossy();
        let full_rel = if prefix.is_empty() {
            sub_rel.into_owned()
        } else {
            format!("{prefix}/{sub_rel}")
        };
        if !upper_files.contains_key(&full_rel) {
            if entry.mode.kind() == EntryKind::Commit {
                dirty_map
                    .entry(full_rel.clone())
                    .or_insert_with(|| make_gitlink_deleted_dirty(&full_rel, entry.oid));
                continue;
            }
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
    tree_cache: &mut HeadTreeCache<'_>,
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
        let ft = meta.file_type();
        let rel_path = if rel_prefix.is_empty() {
            name.to_string()
        } else {
            format!("{rel_prefix}/{name}")
        };

        if ft.is_dir() {
            let head_entry = tree_cache.lookup(&rel_path)?;
            let is_gitlink = head_entry.is_some_and(|(_, m)| m.kind() == EntryKind::Commit)
                || abs_path.join(".git").exists();
            if is_gitlink {
                dirs.push(rel_path);
                continue;
            }
            let is_tracked_dir = head_entry.is_some_and(|(_, mode)| mode.is_tree());
            if !is_tracked_dir && ignore_eval.is_ignored(&rel_path, true) {
                continue;
            }
            dirs.push(rel_path.clone());
            walk_upper_dir(&abs_path, &rel_path, tree_cache, ignore_eval, files, dirs)?;
        } else if ft.is_file() || ft.is_symlink() {
            let kind = disk_entry_kind(&meta);
            files.insert(rel_path, (abs_path, kind));
        }
        // Skip special filesystem entries (FIFOs, Unix sockets, device nodes) just like Git.
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
        Path::new("")
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

/// Read-only `.git/index` stat-cache accelerator for [`scan_general_workdir`].
///
/// Preserves 100% indexless semantics (all comparisons remain strictly against `HEAD`):
/// an entry is considered provably clean without reading/hashing file bytes ONLY when:
/// 1. The index entry is unconflicted, has non-zero stat metadata, and matches `HEAD`'s OID & mode.
/// 2. Its `mtime` is strictly older than `.git/index`'s `mtime` (racy-git guard).
/// 3. Its on-disk `len`, `mtime` (secs + nsecs when present), and on Unix `ctime` & `ino`
///    match the index stat record exactly.
struct IndexStatCache {
    index: gix::index::File,
    index_mtime_secs: u32,
}

impl IndexStatCache {
    fn load(repo: &OzoneRepo, hash_kind: gix::hash::Kind) -> Option<Self> {
        // Only trust `.git/index` when it belongs to this workdir (main repo or linked worktree).
        if repo.workspace.is_some() && !repo.is_linked_worktree {
            return None;
        }
        let index_path = repo.git_dir.join("index");
        let index_meta = fs::metadata(&index_path).ok()?;
        let index_mtime_secs = metadata_mtime_secs_u32(&index_meta)?;
        let index = gix::index::File::at(
            &index_path,
            hash_kind,
            true,
            gix::index::decode::Options::default(),
        )
        .ok()?;
        Some(Self {
            index,
            index_mtime_secs,
        })
    }

    fn entry_count(&self) -> usize {
        self.index.entries().len()
    }

    fn is_provably_clean(
        &self,
        rel_path: &str,
        meta: &fs::Metadata,
        disk_kind: EntryKind,
        head: &HeadTreeEntry,
    ) -> bool {
        // Always check symlinks and gitlinks directly.
        if disk_kind == EntryKind::Link
            || head.kind == EntryKind::Link
            || head.kind == EntryKind::Commit
        {
            return false;
        }
        let eff_disk_kind = effective_disk_kind(disk_kind, Some(head.kind));
        if eff_disk_kind != head.kind {
            return false;
        }

        let Some(entry) = self.index.entry_by_path(rel_path.as_bytes().as_bstr()) else {
            return false;
        };
        if entry.stage() != gix::index::entry::Stage::Unconflicted {
            return false;
        }
        if entry.id != head.oid {
            return false;
        }
        let Some(idx_kind) = index_mode_to_entry_kind(entry.mode) else {
            return false;
        };
        if idx_kind != head.kind {
            return false;
        }

        let stat = &entry.stat;
        // Reject zeroed placeholder stats (e.g. from `index_from_tree` or partial-hunk commits).
        if stat.mtime.secs == 0 {
            return false;
        }
        // Racy-git guard: never trust stat entries written in the same or later second as `.git/index`.
        if stat.mtime.secs >= self.index_mtime_secs {
            return false;
        }

        let file_len = meta.len();
        if file_len > u64::from(u32::MAX) || (file_len as u32) != stat.size {
            return false;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let m_sec = meta.mtime();
            if m_sec < 0 || (m_sec as u32) != stat.mtime.secs {
                return false;
            }
            if stat.mtime.nsecs != 0 && (meta.mtime_nsec() as u32) != stat.mtime.nsecs {
                return false;
            }
            if stat.ctime.secs != 0 {
                let c_sec = meta.ctime();
                if c_sec < 0 || (c_sec as u32) != stat.ctime.secs {
                    return false;
                }
                if stat.ctime.nsecs != 0 && (meta.ctime_nsec() as u32) != stat.ctime.nsecs {
                    return false;
                }
            }
            stat.ino == 0 || (meta.ino() as u32) == stat.ino
        }
        #[cfg(not(unix))]
        {
            let Ok(modified) = meta.modified() else {
                return false;
            };
            let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH) else {
                return false;
            };
            if (dur.as_secs() as u32) != stat.mtime.secs {
                return false;
            }
            if stat.mtime.nsecs != 0 && dur.subsec_nanos() != stat.mtime.nsecs {
                return false;
            }
            true
        }
    }
}

fn metadata_mtime_secs_u32(meta: &fs::Metadata) -> Option<u32> {
    let dur = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    u32::try_from(dur.as_secs()).ok()
}

fn index_mode_to_entry_kind(mode: gix::index::entry::Mode) -> Option<EntryKind> {
    match mode {
        gix::index::entry::Mode::FILE => Some(EntryKind::Blob),
        gix::index::entry::Mode::FILE_EXECUTABLE => Some(EntryKind::BlobExecutable),
        gix::index::entry::Mode::SYMLINK => Some(EntryKind::Link),
        _ => None,
    }
}

/// Traverses `tree_id` into a flat list of `(rel_path, HeadTreeEntry)`, fanning out
/// subtrees across `rayon` worker threads for large monorepos.
fn collect_head_entries_parallel(
    gix_repo: &gix::Repository,
    tree_id: gix::ObjectId,
    capacity_hint: usize,
) -> Result<Vec<(String, HeadTreeEntry)>> {
    let mut files = Vec::with_capacity(capacity_hint);
    let mut frontier: Vec<(String, gix::ObjectId)> = vec![(String::new(), tree_id)];

    // Expand up to 2 levels sequentially until we have enough subtrees to saturate worker threads.
    let target_parallelism = rayon::current_num_threads().saturating_mul(2).max(8);
    let mut level = 0usize;
    while !frontier.is_empty() && frontier.len() < target_parallelism && level < 3 {
        let mut next_frontier = Vec::new();
        for (prefix, oid) in frontier {
            let tree = gix_repo
                .find_tree(oid)
                .map_err(|err| OzError::Git(err.to_string()))?;
            for entry_res in gix::objs::TreeRefIter::from_bytes(&tree.data, oid.kind()) {
                let entry = entry_res.map_err(|err| OzError::Git(err.to_string()))?;
                let name = entry.filename.to_str_lossy();
                let full_path = if prefix.is_empty() {
                    name.into_owned()
                } else {
                    let mut s = String::with_capacity(prefix.len() + 1 + name.len());
                    s.push_str(&prefix);
                    s.push('/');
                    s.push_str(&name);
                    s
                };
                if entry.mode.is_tree() {
                    next_frontier.push((full_path, entry.oid.to_owned()));
                } else {
                    files.push((
                        full_path,
                        HeadTreeEntry {
                            oid: entry.oid.to_owned(),
                            kind: normalize_entry_kind(entry.mode.kind()),
                        },
                    ));
                }
            }
        }
        frontier = next_frontier;
        level += 1;
    }

    if frontier.is_empty() {
        return Ok(files);
    }

    let git_dir = gix_repo.git_dir().to_path_buf();
    let chunks: Vec<Vec<(String, HeadTreeEntry)>> = frontier
        .into_par_iter()
        .map_init(
            || gix::open(&git_dir).map_err(|err| OzError::Git(err.to_string())),
            |thread_repo_res, root_subtree| -> Result<Vec<(String, HeadTreeEntry)>> {
                let thread_repo = thread_repo_res
                    .as_ref()
                    .map_err(|err| OzError::Git(err.to_string()))?;
                let mut local = Vec::new();
                let mut stack = vec![root_subtree];
                while let Some((prefix, oid)) = stack.pop() {
                    let tree = thread_repo
                        .find_tree(oid)
                        .map_err(|err| OzError::Git(err.to_string()))?;
                    for entry_res in gix::objs::TreeRefIter::from_bytes(&tree.data, oid.kind()) {
                        let entry = entry_res.map_err(|err| OzError::Git(err.to_string()))?;
                        let name = entry.filename.to_str_lossy();
                        let mut full_path =
                            String::with_capacity(prefix.len() + 1 + name.len());
                        full_path.push_str(&prefix);
                        full_path.push('/');
                        full_path.push_str(&name);
                        if entry.mode.is_tree() {
                            stack.push((full_path, entry.oid.to_owned()));
                        } else {
                            local.push((
                                full_path,
                                HeadTreeEntry {
                                    oid: entry.oid.to_owned(),
                                    kind: normalize_entry_kind(entry.mode.kind()),
                                },
                            ));
                        }
                    }
                }
                Ok(local)
            },
        )
        .collect::<Result<Vec<_>>>()?;

    let extra: usize = chunks.iter().map(Vec::len).sum();
    files.reserve(extra);
    for chunk in chunks {
        files.extend(chunk);
    }
    Ok(files)
}

fn scan_general_workdir(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    git_cfg: &GitConfig,
    head: &WorkspaceHead,
) -> Result<Vec<DirtyWithPayload>> {
    let hash_kind = gix_repo.object_hash();
    let mut index_cache = IndexStatCache::load(repo, hash_kind);
    let cap_hint = index_cache.as_ref().map_or(0, IndexStatCache::entry_count);

    let staged_gitlinks: Vec<(String, gix::ObjectId)> = index_cache
        .as_ref()
        .map(|ic| {
            ic.index
                .entries()
                .iter()
                .filter(|e| e.mode == gix::index::entry::Mode::COMMIT)
                .map(|e| (e.path(&ic.index).to_str_lossy().into_owned(), e.id))
                .collect()
        })
        .unwrap_or_default();

    let attr_resolver =
        AttributeResolver::for_workdir(&repo.workdir_path, &repo.git_common_dir, git_cfg)?;

    if let Some(ref ic) = index_cache
        && let Some(attr_mtime) =
            attr_resolver.max_attributes_mtime(&repo.workdir_path, &repo.git_common_dir)
        && let Ok(dur) = attr_mtime.duration_since(std::time::UNIX_EPOCH)
        && dur.as_secs() >= u64::from(ic.index_mtime_secs)
    {
        index_cache = None;
    }

    let head_entries = collect_head_entries_parallel(gix_repo, head.tree_id, cap_hint)?;
    let mut head_map: HashMap<&str, usize> = HashMap::with_capacity(head_entries.len());
    let mut gitlink_paths: BTreeSet<String> = BTreeSet::new();
    for (idx, (path, entry)) in head_entries.iter().enumerate() {
        head_map.insert(path.as_str(), idx);
        if entry.kind == EntryKind::Commit {
            gitlink_paths.insert(path.clone());
        }
    }
    let visited: Vec<AtomicBool> = (0..head_entries.len())
        .map(|_| AtomicBool::new(false))
        .collect();

    let shared_results: Mutex<(Vec<DirtyWithPayload>, Option<OzError>)> =
        Mutex::new((Vec::new(), None));

    if repo.workdir_path.is_dir() {
        let exclude_gi = if !repo.workdir_path.join(".git").exists()
            && repo.git_common_dir.join("info/exclude").is_file()
        {
            Some(Gitignore::new(repo.git_common_dir.join("info/exclude")).0)
        } else {
            None
        };

        let walk_root = repo.workdir_path.clone();
        let walk_gitlinks = gitlink_paths.clone();
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
            .threads(rayon::current_num_threads().clamp(1, 16))
            .filter_entry(move |entry| {
                if entry.file_name() == ".git" {
                    return false;
                }
                if entry.depth() > 0 && entry.file_type().is_some_and(|ft| ft.is_dir()) {
                    if let Ok(rel) = entry.path().strip_prefix(&walk_root) {
                        let rel_s = path_to_slash(rel);
                        if walk_gitlinks.contains(&rel_s) {
                            return false;
                        }
                    }
                    if entry.path().join(".git").exists() {
                        return false;
                    }
                }
                true
            });

        let workdir_root = &repo.workdir_path;
        let head_entries_ref = &head_entries;
        let head_map_ref = &head_map;
        let visited_ref = &visited;
        let index_cache_ref = &index_cache;
        let exclude_gi_ref = &exclude_gi;
        let shared_ref = &shared_results;
        let attrs_ref = &attr_resolver;
        let cfg_ref = git_cfg;
        let git_dir_ref = &repo.git_dir;

        struct WorkerFlush<'a> {
            local_dirty: Vec<DirtyWithPayload>,
            local_err: Option<OzError>,
            shared: &'a Mutex<(Vec<DirtyWithPayload>, Option<OzError>)>,
        }

        impl Drop for WorkerFlush<'_> {
            fn drop(&mut self) {
                if self.local_dirty.is_empty() && self.local_err.is_none() {
                    return;
                }
                let mut guard = self.shared.lock().expect("status walk lock poisoned");
                guard.0.append(&mut self.local_dirty);
                if guard.1.is_none() && self.local_err.is_some() {
                    guard.1 = self.local_err.take();
                }
            }
        }

        builder.build_parallel().run(|| {
            let mut collector = WorkerFlush {
                local_dirty: Vec::new(),
                local_err: None,
                shared: shared_ref,
            };
            Box::new(move |result| {
                if collector.local_err.is_some() {
                    return ignore::WalkState::Quit;
                }
                let entry = match result {
                    Ok(e) => e,
                    Err(err) => {
                        collector.local_err = Some(OzError::Other(err.to_string()));
                        return ignore::WalkState::Quit;
                    }
                };
                if entry.depth() == 0 {
                    return ignore::WalkState::Continue;
                }
                // Skip directories and special entries (FIFOs, Unix sockets, device nodes) cheaply.
                if let Some(ft) = entry.file_type()
                    && (ft.is_dir() || (!ft.is_file() && !ft.is_symlink()))
                {
                    return ignore::WalkState::Continue;
                }

                let path = entry.path();
                let Ok(rel) = path.strip_prefix(workdir_root) else {
                    return ignore::WalkState::Continue;
                };

                // Zero-allocation relative path lookup on Unix when valid UTF-8.
                let rel_owned: String;
                let rel_str: &str = {
                    #[cfg(unix)]
                    {
                        if let Some(s) = rel.to_str() {
                            s
                        } else {
                            rel_owned = path_to_slash(rel);
                            &rel_owned
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        rel_owned = path_to_slash(rel);
                        &rel_owned
                    }
                };
                if rel_str.is_empty() {
                    return ignore::WalkState::Continue;
                }

                let meta = match fs::symlink_metadata(path) {
                    Ok(m) => m,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        return ignore::WalkState::Continue;
                    }
                    Err(err) => {
                        collector.local_err = Some(OzError::Io(err));
                        return ignore::WalkState::Quit;
                    }
                };
                let ft = meta.file_type();
                if ft.is_dir() || (!ft.is_file() && !ft.is_symlink()) {
                    return ignore::WalkState::Continue;
                }

                let disk_kind = disk_entry_kind(&meta);
                if let Some(&idx) = head_map_ref.get(rel_str) {
                    visited_ref[idx].store(true, Ordering::Relaxed);
                    let head_entry = &head_entries_ref[idx].1;
                    if let Some(cache) = index_cache_ref
                        && cache.is_provably_clean(rel_str, &meta, disk_kind, head_entry)
                    {
                        return ignore::WalkState::Continue;
                    }
                    match compare_disk_with_head_hash(
                        rel_str,
                        path,
                        disk_kind,
                        Some(head_entry.clone()),
                        hash_kind,
                        attrs_ref,
                        cfg_ref,
                        workdir_root,
                        git_dir_ref,
                    ) {
                        Ok(Some(item)) => collector.local_dirty.push(item),
                        Ok(None) => {}
                        Err(err) => {
                            collector.local_err = Some(err);
                            return ignore::WalkState::Quit;
                        }
                    }
                } else {
                    if let Some(gi) = exclude_gi_ref
                        && gi.matched_path_or_any_parents(rel_str, false).is_ignore()
                    {
                        return ignore::WalkState::Continue;
                    }
                    match compare_disk_with_head_hash(
                        rel_str,
                        path,
                        disk_kind,
                        None,
                        hash_kind,
                        attrs_ref,
                        cfg_ref,
                        workdir_root,
                        git_dir_ref,
                    ) {
                        Ok(Some(item)) => collector.local_dirty.push(item),
                        Ok(None) => {}
                        Err(err) => {
                            collector.local_err = Some(err);
                            return ignore::WalkState::Quit;
                        }
                    }
                }
                ignore::WalkState::Continue
            })
        });
    }

    let (mut dirty, walk_err) = shared_results
        .into_inner()
        .expect("status walk lock poisoned");
    if let Some(err) = walk_err {
        return Err(err);
    }

    // Check any tracked HEAD entries not yielded by WalkBuilder
    // (gitlink directories, deleted files, or still present while matching `.gitignore`).
    let unvisited_dirty: Vec<DirtyWithPayload> = visited
        .par_iter()
        .enumerate()
        .filter_map(|(idx, seen)| {
            if seen.load(Ordering::Relaxed) {
                return None;
            }
            let (ref rel_path, ref head_entry) = head_entries[idx];
            let abs_path = repo.workdir_path.join(rel_path);
            if head_entry.kind == EntryKind::Commit {
                match fs::symlink_metadata(&abs_path) {
                    Ok(meta) if meta.file_type().is_dir() => {
                        match inspect_gitlink_dir_state(rel_path, &abs_path, head_entry.oid, false) {
                            Ok(opt) => opt.map(Ok),
                            Err(err) => Some(Err(err)),
                        }
                    }
                    Ok(meta) if meta.file_type().is_file() || meta.file_type().is_symlink() => {
                        let disk_kind = disk_entry_kind(&meta);
                        match compare_disk_with_head_hash(
                            rel_path,
                            &abs_path,
                            disk_kind,
                            Some(head_entry.clone()),
                            hash_kind,
                            &attr_resolver,
                            git_cfg,
                            &repo.workdir_path,
                            &repo.git_dir,
                        ) {
                            Ok(opt) => opt.map(Ok),
                            Err(err) => Some(Err(err)),
                        }
                    }
                    _ => Some(Ok(make_gitlink_deleted_dirty(rel_path, head_entry.oid))),
                }
            } else {
                match fs::symlink_metadata(&abs_path) {
                    Ok(meta) if meta.file_type().is_file() || meta.file_type().is_symlink() => {
                        let disk_kind = disk_entry_kind(&meta);
                        if let Some(ref cache) = index_cache
                            && cache.is_provably_clean(rel_path, &meta, disk_kind, head_entry)
                        {
                            return None;
                        }
                        match compare_disk_with_head_hash(
                            rel_path,
                            &abs_path,
                            disk_kind,
                            Some(head_entry.clone()),
                            hash_kind,
                            &attr_resolver,
                            git_cfg,
                            &repo.workdir_path,
                            &repo.git_dir,
                        ) {
                            Ok(opt) => opt.map(Ok),
                            Err(err) => Some(Err(err)),
                        }
                    }
                    _ => Some(Ok(DirtyWithPayload {
                        raw: RawDirtyFile {
                            path: rel_path.clone(),
                            kind: ChangeKind::Deleted,
                            disk_kind: None,
                            head_entry: Some(head_entry.clone()),
                        },
                        old_bytes: Vec::new(),
                        new_bytes: Vec::new(),
                        known_binary: false,
                    })),
                }
            }
        })
        .collect::<Result<Vec<_>>>()?;

    dirty.extend(unvisited_dirty);

    // Pick up newly staged gitlinks (`160000` in `.git/index`, not yet in HEAD).
    for (sub_rel, idx_oid) in staged_gitlinks {
        if head_map.contains_key(sub_rel.as_str()) {
            continue;
        }
        let abs_path = repo.workdir_path.join(&sub_rel);
        if abs_path.is_dir() {
            let sub_oid = read_submodule_head_oid(&abs_path).unwrap_or(idx_oid);
            dirty.push(DirtyWithPayload {
                raw: RawDirtyFile {
                    path: sub_rel,
                    kind: ChangeKind::Added,
                    disk_kind: Some(EntryKind::Commit),
                    head_entry: None,
                },
                old_bytes: Vec::new(),
                new_bytes: format!("Subproject commit {}\n", sub_oid.to_hex()).into_bytes(),
                known_binary: false,
            });
        }
    }

    Ok(dirty)
}

pub(crate) fn build_repo_attribute_resolver(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    tree_id: gix::ObjectId,
    git_cfg: &GitConfig,
) -> Result<AttributeResolver> {
    if let Some((upper_dir, meta_dir)) = detect_fuse_dirs(repo) {
        let head_tree = gix_repo
            .find_tree(tree_id)
            .map_err(|err| OzError::Git(err.to_string()))?;
        let whiteout_paths = collect_whiteout_paths(&meta_dir.join("whiteouts"))?;
        let opaque_dirs = collect_opaque_dirs(&meta_dir.join("opaque"))?;
        collect_fuse_attributes(
            repo,
            gix_repo,
            &head_tree,
            &upper_dir,
            &whiteout_paths,
            &opaque_dirs,
            git_cfg,
        )
    } else {
        AttributeResolver::for_workdir(&repo.workdir_path, &repo.git_common_dir, git_cfg)
    }
}

fn collect_fuse_attributes(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    head_tree: &gix::Tree<'_>,
    upper_dir: &Path,
    whiteouts: &BTreeSet<String>,
    opaque_dirs: &BTreeSet<String>,
    git_cfg: &GitConfig,
) -> Result<AttributeResolver> {
    let mut per_dir: HashMap<String, String> = HashMap::new();
    let entries = head_tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|err| OzError::Git(err.to_string()))?;
    for entry in entries {
        if !entry.mode.is_blob() {
            continue;
        }
        let rel = entry.filepath.to_str_lossy();
        if rel == ".gitattributes" || rel.ends_with("/.gitattributes") {
            if whiteouts.contains(rel.as_ref()) || opaque_dirs.contains("") {
                continue;
            }
            let dir = rel.strip_suffix("/.gitattributes").unwrap_or("").to_string();
            if !dir.is_empty() && opaque_dirs.contains(&dir) {
                continue;
            }
            let blob = gix_repo
                .find_blob(entry.oid)
                .map_err(|err| OzError::Git(err.to_string()))?;
            per_dir.insert(dir, String::from_utf8_lossy(&blob.data).into_owned());
        }
    }

    if upper_dir.is_dir() {
        collect_upper_gitattributes(upper_dir, "", &mut per_dir)?;
    }

    AttributeResolver::from_preloaded_files(per_dir, &repo.git_common_dir, git_cfg)
}

fn collect_upper_gitattributes(
    dir_abs: &Path,
    rel_prefix: &str,
    per_dir: &mut HashMap<String, String>,
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
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_dir() {
            if path.join(".git").exists() {
                continue;
            }
            let sub = if rel_prefix.is_empty() {
                name.to_string()
            } else {
                format!("{rel_prefix}/{name}")
            };
            collect_upper_gitattributes(&path, &sub, per_dir)?;
        } else if name == ".gitattributes" && meta.file_type().is_file() {
            per_dir.insert(rel_prefix.to_string(), fs::read_to_string(&path)?);
        }
    }
    Ok(())
}

pub(crate) fn read_submodule_head_oid(sub_dir: &Path) -> Option<gix::ObjectId> {
    if !sub_dir.join(".git").exists() {
        return None;
    }
    let sub_repo = gix::open(sub_dir).ok()?;
    let head = sub_repo.head().ok()?;
    if head.is_unborn() {
        return None;
    }
    head.into_peeled_id().ok().map(|id| id.detach())
}

fn format_subproject_commit(oid: gix::ObjectId) -> Vec<u8> {
    format!("Subproject commit {}\n", oid.to_hex()).into_bytes()
}

fn make_gitlink_deleted_dirty(rel_path: &str, head_oid: gix::ObjectId) -> DirtyWithPayload {
    DirtyWithPayload {
        raw: RawDirtyFile {
            path: rel_path.to_string(),
            kind: ChangeKind::Deleted,
            disk_kind: None,
            head_entry: Some(HeadTreeEntry {
                oid: head_oid,
                kind: EntryKind::Commit,
            }),
        },
        old_bytes: format_subproject_commit(head_oid),
        new_bytes: Vec::new(),
        known_binary: false,
    }
}

fn inspect_gitlink_dir_state(
    rel_path: &str,
    abs_dir: &Path,
    head_oid: gix::ObjectId,
    had_whiteout: bool,
) -> Result<Option<DirtyWithPayload>> {
    if !abs_dir.exists() {
        if had_whiteout {
            return Ok(Some(make_gitlink_deleted_dirty(rel_path, head_oid)));
        }
        return Ok(None);
    }
    if !abs_dir.join(".git").exists() {
        // Empty / uninitialized submodule mount dir is clean in Git worktrees.
        return Ok(None);
    }
    let Some(sub_oid) = read_submodule_head_oid(abs_dir) else {
        return Ok(None);
    };
    if sub_oid == head_oid {
        return Ok(None);
    }
    Ok(Some(DirtyWithPayload {
        raw: RawDirtyFile {
            path: rel_path.to_string(),
            kind: ChangeKind::Modified,
            disk_kind: Some(EntryKind::Commit),
            head_entry: Some(HeadTreeEntry {
                oid: head_oid,
                kind: EntryKind::Commit,
            }),
        },
        old_bytes: format_subproject_commit(head_oid),
        new_bytes: format_subproject_commit(sub_oid),
        known_binary: false,
    }))
}

fn read_added_payload(
    rel_path: &str,
    abs_path: &Path,
    disk_kind: EntryKind,
    attrs: &AttributeResolver,
    cfg: &GitConfig,
) -> Result<(Vec<u8>, bool)> {
    let resolved = attrs.resolve(rel_path);
    if resolved.is_diff_disabled() {
        return Ok((Vec::new(), true));
    }
    if disk_kind != EntryKind::Link && is_file_binary_prefix(abs_path)? {
        return Ok((Vec::new(), true));
    }
    let raw = read_disk_bytes(abs_path, Some(disk_kind))?;
    if resolved.filter_driver() == Some("lfs") && !is_lfs_pointer(&raw) {
        // Do not run mutating `git-lfs clean` or dump multi-MB binary diffs on untracked files.
        return Ok((Vec::new(), true));
    }
    let cleaned = clean_eol_bytes(&raw, &resolved, cfg);
    Ok((cleaned.into_owned(), false))
}

#[allow(clippy::too_many_arguments)]
fn compare_disk_with_head_hash(
    rel_path: &str,
    abs_path: &Path,
    disk_kind: EntryKind,
    head_entry: Option<HeadTreeEntry>,
    hash_kind: gix::hash::Kind,
    attrs: &AttributeResolver,
    cfg: &GitConfig,
    workdir: &Path,
    git_dir: &Path,
) -> Result<Option<DirtyWithPayload>> {
    let resolved = attrs.resolve(rel_path);
    match head_entry {
        Some(head) => {
            if head.kind == EntryKind::Commit {
                let head_oid = head.oid;
                let raw = read_disk_bytes(abs_path, Some(disk_kind))?;
                let known_binary = resolved.is_diff_disabled()
                    || (disk_kind != EntryKind::Link && crate::diff::is_binary_bytes(&raw));
                return Ok(Some(DirtyWithPayload {
                    raw: RawDirtyFile {
                        path: rel_path.to_string(),
                        kind: ChangeKind::TypeChanged,
                        disk_kind: Some(disk_kind),
                        head_entry: Some(head),
                    },
                    old_bytes: format_subproject_commit(head_oid),
                    new_bytes: if known_binary { Vec::new() } else { raw },
                    known_binary,
                }));
            }
            let eff_disk_kind = effective_disk_kind(disk_kind, Some(head.kind));
            let raw_disk_bytes = read_disk_bytes(abs_path, Some(disk_kind))?;
            let raw_oid =
                gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, &raw_disk_bytes)
                    .map_err(|err| OzError::Git(err.to_string()))?;
            if raw_oid == head.oid && eff_disk_kind == head.kind {
                return Ok(None);
            }

            let cleaned_cow = clean_blob_bytes(
                rel_path,
                &raw_disk_bytes,
                &resolved,
                cfg,
                workdir,
                git_dir,
                false,
            )?;
            let cleaned_oid =
                gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, &cleaned_cow)
                    .map_err(|err| OzError::Git(err.to_string()))?;
            if cleaned_oid == head.oid && eff_disk_kind == head.kind {
                return Ok(None);
            }

            let kind = if (eff_disk_kind == EntryKind::Link) != (head.kind == EntryKind::Link) {
                ChangeKind::TypeChanged
            } else {
                ChangeKind::Modified
            };
            let known_binary = resolved.is_diff_disabled()
                || (eff_disk_kind != EntryKind::Link && crate::diff::is_binary_bytes(&cleaned_cow));
            let new_bytes = if known_binary {
                Vec::new()
            } else {
                cleaned_cow.into_owned()
            };
            Ok(Some(DirtyWithPayload {
                raw: RawDirtyFile {
                    path: rel_path.to_string(),
                    kind,
                    disk_kind: Some(eff_disk_kind),
                    head_entry: Some(head),
                },
                old_bytes: Vec::new(),
                new_bytes,
                known_binary,
            }))
        }
        None => {
            let (new_bytes, known_binary) =
                read_added_payload(rel_path, abs_path, disk_kind, attrs, cfg)?;
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
        EntryKind::Commit => EntryKind::Commit,
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
