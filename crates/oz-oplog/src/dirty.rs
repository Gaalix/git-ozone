// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Self-contained `O(dirty_files)` dirty scanning, incremental Git tree building via
//! `gix::objs::tree::Editor`, tree-to-tree diffing, and `O(dirty_files)` working-tree restore.

use crate::store::atomic_write_bytes;
use crate::types::{DEFAULT_WORKSPACE_KEY, DirtyFileSnapshot, DirtyFileState, WsSnapshot};
use chrono::Utc;
use gix::bstr::ByteSlice;
use gix::hash::ObjectId;
use gix::objs::tree::EntryKind;
use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use oz_core::{
    BackendKind, ChangeKind, DirtyEntry, FileChange, MountHealth, OzError, OzoneRepo, Result,
    WorkspaceState,
};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

const MODE_BLOB: u32 = 0o100644;
const MODE_EXEC: u32 = 0o100755;
const MODE_LINK: u32 = 0o120000;

/// Returns the workspace identifier key used in `ws_snapshots` and checkpoint refs.
#[must_use]
pub fn workspace_key(repo: &OzoneRepo) -> &str {
    repo.workspace.as_deref().unwrap_or(DEFAULT_WORKSPACE_KEY)
}

/// Ensures a workspace mount is not disconnected (`ENOTCONN`) before touching its working tree.
pub(crate) fn ensure_mount_healthy(repo: &OzoneRepo) -> Result<()> {
    if repo.mount_health == MountHealth::DisconnectedEnotconn {
        return Err(OzError::MountUnavailable {
            workspace: workspace_key(repo).to_string(),
            path: repo.workdir_path.clone(),
            errno: Some(107),
            message: "workspace FUSE mount returned ENOTCONN; remount before operating"
                .to_string(),
        });
    }
    Ok(())
}

/// Opens the underlying `gix::Repository` on host disk with a safe fallback committer identity
/// so reflog-enabled reference transactions never fail when `user.name`/`user.email` are unset.
pub(crate) fn open_gix_for_workspace(repo: &OzoneRepo) -> Result<gix::Repository> {
    let mut gix_repo = if repo.is_linked_worktree && repo.git_dir.join("HEAD").is_file() {
        gix::open(&repo.git_dir)
    } else {
        gix::open(&repo.repo_root)
    }
    .map_err(|err| OzError::Git(err.to_string()))?;

    gix_repo
        .committer_or_set_fallback("Ozone", "bot@ozone.dev")
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(gix_repo)
}

/// Resolves the live branch, `HEAD` commit OID (if born), and base tree OID for `repo`.
pub(crate) fn resolve_workspace_head(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
) -> Result<(Option<String>, Option<ObjectId>, ObjectId)> {
    let empty_tree = ObjectId::empty_tree(gix_repo.object_hash());

    if let Some(ref branch) = repo.branch {
        let ref_name = format!("refs/heads/{branch}");
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
            return Ok((Some(branch.clone()), Some(commit.id), tree_id));
        }
        return Ok((Some(branch.clone()), None, empty_tree));
    }

    let mut head = gix_repo
        .head()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let branch = head
        .referent_name()
        .map(|name| String::from_utf8_lossy(name.shorten().as_ref()).into_owned());
    if head.is_unborn() {
        return Ok((branch, None, empty_tree));
    }
    let commit = head
        .peel_to_commit()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let tree_id = commit
        .tree_id()
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();
    Ok((branch, Some(commit.id), tree_id))
}

/// Converts a Git `EntryMode` (`u16`) into canonical `u32` permissions.
#[must_use]
pub(crate) fn entry_mode_to_u32(mode: gix::objs::tree::EntryMode) -> u32 {
    u32::from(mode.value())
}

/// Maps a stored Git file mode (`0o100644`, `0o100755`, `0o120000`) to `gix::objs::tree::EntryKind`.
#[must_use]
pub(crate) fn entry_kind_from_mode(mode: u32) -> EntryKind {
    match mode {
        MODE_LINK => EntryKind::Link,
        MODE_EXEC => EntryKind::BlobExecutable,
        _ => EntryKind::Blob,
    }
}

/// Normalizes a repo-relative path string to forward-slash separators without leading `./`.
pub(crate) fn normalize_rel_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    normalized.strip_prefix("./").unwrap_or(&normalized).to_string()
}

fn path_to_rel_string(rel: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(s) => {
                let s = s.to_str()?;
                parts.push(s);
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

pub(crate) fn parse_oid(hex: &str) -> Result<ObjectId> {
    ObjectId::from_hex(hex.as_bytes())
        .map_err(|err| OzError::Git(format!("invalid object id '{hex}': {err}")))
}

fn read_disk_blob_and_mode(path: &Path, meta: &fs::Metadata) -> Result<(Vec<u8>, u32)> {
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        #[cfg(unix)]
        let bytes = {
            use std::os::unix::ffi::OsStrExt;
            target.as_os_str().as_bytes().to_vec()
        };
        #[cfg(not(unix))]
        let bytes = target.to_string_lossy().as_bytes().to_vec();
        return Ok((bytes, MODE_LINK));
    }

    let bytes = fs::read(path)?;
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            MODE_EXEC
        } else {
            MODE_BLOB
        }
    };
    #[cfg(not(unix))]
    let mode = MODE_BLOB;

    Ok((bytes, mode))
}

fn compute_blob_oid(hash_kind: gix::hash::Kind, bytes: &[u8]) -> Result<ObjectId> {
    gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, bytes)
        .map_err(|err| OzError::Git(err.to_string()))
}

fn write_blob_if_needed(gix_repo: &gix::Repository, bytes: &[u8]) -> Result<String> {
    let oid = gix_repo
        .write_blob(bytes)
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(oid.detach().to_hex().to_string())
}

struct PendingDirtyDiskFile {
    rel_path: String,
    bytes: Vec<u8>,
    mode: u32,
}

enum ScanItem {
    Present(PendingDirtyDiskFile),
    Deleted(String),
}

fn resolve_fuse_dirs(repo: &OzoneRepo) -> Option<(PathBuf, PathBuf)> {
    if let Some(ref record) = repo.mount_record
        && let (Some(upper), Some(meta)) = (&record.upper_dir, &record.meta_dir)
    {
        return Some((upper.clone(), meta.clone()));
    }
    if let Some(ref state) = repo.workspace_state
        && state.backend == BackendKind::LinuxFuse3
    {
        let upper = repo.layout.ws_upper_dir(&state.name);
        let meta = repo.layout.ws_tombstone_dir(&state.name);
        if upper.is_dir() || meta.is_dir() {
            return Some((upper, meta));
        }
    }
    None
}

fn build_gitignore_matcher(repo: &OzoneRepo, upper_dir: Option<&Path>) -> Option<Gitignore> {
    let root = &repo.workdir_path;
    let mut builder = GitignoreBuilder::new(root);
    let mut any = false;

    let workdir_gi = root.join(".gitignore");
    if workdir_gi.is_file() {
        let _ = builder.add(&workdir_gi);
        any = true;
    } else if let Some(upper) = upper_dir {
        let upper_gi = upper.join(".gitignore");
        if upper_gi.is_file() {
            let _ = builder.add(&upper_gi);
            any = true;
        } else if repo.repo_root.join(".gitignore").is_file() {
            let _ = builder.add(repo.repo_root.join(".gitignore"));
            any = true;
        }
    }

    let exclude = repo.git_common_dir.join("info").join("exclude");
    if exclude.is_file() {
        let _ = builder.add(&exclude);
        any = true;
    }

    if !any {
        return None;
    }
    builder.build().ok()
}

fn is_ignored_rel(gi: Option<&Gitignore>, rel_path: &str, is_dir: bool) -> bool {
    let Some(gi) = gi else {
        return false;
    };
    gi.matched_path_or_any_parents(rel_path, is_dir).is_ignore()
}

fn collect_subtree_leaf_paths(
    gix_repo: &gix::Repository,
    base_tree: &gix::Tree<'_>,
    rel_dir: &str,
    out: &mut Vec<String>,
) -> Result<()> {
    let Some(entry) = base_tree
        .lookup_entry_by_path(rel_dir)
        .map_err(|err| OzError::Git(err.to_string()))?
    else {
        return Ok(());
    };
    if !entry.mode().is_tree() {
        out.push(rel_dir.to_string());
        return Ok(());
    }
    let subtree = gix_repo
        .find_tree(entry.object_id())
        .map_err(|err| OzError::Git(err.to_string()))?;
    let files = subtree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|err| OzError::Git(err.to_string()))?;
    for child in files {
        if !child.mode.is_tree() {
            let child_rel = child.filepath.to_str_lossy();
            out.push(format!("{rel_dir}/{child_rel}"));
        }
    }
    Ok(())
}

/// Scans dirty working-tree files in `O(dirty_files)` when `dirty_override` is supplied or when
/// running on a FUSE3 workspace (`upper_dir` + `meta_dir/{whiteouts,opaque}`), and falls back
/// to a parallel `gix_index` stat-shortcut + `ignore::WalkBuilder` scan on standard trees.
pub(crate) fn scan_dirty_files(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    base_tree_id: ObjectId,
    dirty_override: Option<&[FileChange]>,
) -> Result<Vec<DirtyFileSnapshot>> {
    let base_tree = gix_repo
        .find_tree(base_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;

    if let Some(changes) = dirty_override {
        return snapshot_from_override(repo, gix_repo, &base_tree, changes);
    }

    if let Some((upper_dir, meta_dir)) = resolve_fuse_dirs(repo)
        && (upper_dir.is_dir() || meta_dir.is_dir())
    {
        return snapshot_from_fuse_layers(repo, gix_repo, &base_tree, &upper_dir, &meta_dir);
    }

    snapshot_from_standard_workdir(repo, gix_repo, &base_tree)
}

fn snapshot_from_override(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    base_tree: &gix::Tree<'_>,
    changes: &[FileChange],
) -> Result<Vec<DirtyFileSnapshot>> {
    let mut map: BTreeMap<String, DirtyFileState> = BTreeMap::new();

    for change in changes {
        let rel = normalize_rel_path(&change.path);
        match change.kind {
            ChangeKind::Deleted => {
                if base_tree
                    .lookup_entry_by_path(&rel)
                    .map_err(|err| OzError::Git(err.to_string()))?
                    .is_some()
                {
                    map.insert(rel, DirtyFileState::Deleted);
                }
            }
            ChangeKind::Renamed => {
                if let Some(ref old_path) = change.old_path {
                    let old_rel = normalize_rel_path(old_path);
                    if base_tree
                        .lookup_entry_by_path(&old_rel)
                        .map_err(|err| OzError::Git(err.to_string()))?
                        .is_some()
                    {
                        map.insert(old_rel, DirtyFileState::Deleted);
                    }
                }
                snapshot_single_disk_path(repo, gix_repo, base_tree, &rel, &mut map)?;
            }
            _ => {
                snapshot_single_disk_path(repo, gix_repo, base_tree, &rel, &mut map)?;
            }
        }
    }

    Ok(map
        .into_iter()
        .map(|(path, state)| DirtyFileSnapshot { path, state })
        .collect())
}

fn snapshot_single_disk_path(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    base_tree: &gix::Tree<'_>,
    rel: &str,
    map: &mut BTreeMap<String, DirtyFileState>,
) -> Result<()> {
    let disk_path = repo.workdir_path.join(rel);
    match fs::symlink_metadata(&disk_path) {
        Ok(meta) if !meta.is_dir() => {
            let (bytes, mode) = read_disk_blob_and_mode(&disk_path, &meta)?;
            let blob_oid = write_blob_if_needed(gix_repo, &bytes)?;
            map.insert(
                rel.to_string(),
                DirtyFileState::Present { blob_oid, mode },
            );
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if base_tree
                .lookup_entry_by_path(rel)
                .map_err(|e| OzError::Git(e.to_string()))?
                .is_some()
            {
                map.insert(rel.to_string(), DirtyFileState::Deleted);
            }
        }
        Err(err) => return Err(OzError::Io(err)),
    }
    Ok(())
}

fn snapshot_from_fuse_layers(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    base_tree: &gix::Tree<'_>,
    upper_dir: &Path,
    meta_dir: &Path,
) -> Result<Vec<DirtyFileSnapshot>> {
    let hash_kind = gix_repo.object_hash();
    let gi = build_gitignore_matcher(repo, Some(upper_dir));
    let mut map: BTreeMap<String, DirtyFileState> = BTreeMap::new();
    let mut present_in_upper = HashSet::new();

    if upper_dir.is_dir() {
        let mut upper_entries = Vec::new();
        let walker = walkdir::WalkDir::new(upper_dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| e.file_name() != ".git");

        for entry in walker {
            let entry = entry.map_err(|err| OzError::Other(err.to_string()))?;
            if entry.depth() == 0 {
                continue;
            }
            let ft = entry.file_type();
            if ft.is_dir() {
                continue;
            }
            if !ft.is_file() && !ft.is_symlink() {
                continue;
            }
            let Ok(rel_path) = entry.path().strip_prefix(upper_dir) else {
                continue;
            };
            let Some(rel) = path_to_rel_string(rel_path) else {
                continue;
            };
            present_in_upper.insert(rel.clone());
            upper_entries.push((rel, entry.path().to_path_buf()));
        }

        for (rel, full_path) in upper_entries {
            let meta = fs::symlink_metadata(&full_path)?;
            let (bytes, mode) = read_disk_blob_and_mode(&full_path, &meta)?;
            let computed_oid = compute_blob_oid(hash_kind, &bytes)?;

            let tree_entry = base_tree
                .lookup_entry_by_path(&rel)
                .map_err(|err| OzError::Git(err.to_string()))?;

            match tree_entry {
                Some(entry) => {
                    // Committed files remain in `upper_dir`; skip when bytes & mode match HEAD.
                    if entry.object_id() == computed_oid && entry_mode_to_u32(entry.mode()) == mode
                    {
                        continue;
                    }
                }
                None => {
                    if is_ignored_rel(gi.as_ref(), &rel, false) {
                        continue;
                    }
                }
            }

            let blob_oid = write_blob_if_needed(gix_repo, &bytes)?;
            map.insert(rel, DirtyFileState::Present { blob_oid, mode });
        }
    }

    let whiteouts_dir = meta_dir.join("whiteouts");
    if whiteouts_dir.is_dir() {
        for entry in walkdir::WalkDir::new(&whiteouts_dir).follow_links(false) {
            let entry = entry.map_err(|err| OzError::Other(err.to_string()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(rel_path) = entry.path().strip_prefix(&whiteouts_dir) else {
                continue;
            };
            let Some(rel_raw) = path_to_rel_string(rel_path) else {
                continue;
            };
            let Some(target_rel) = rel_raw.strip_suffix(".oz_wh") else {
                continue;
            };
            if present_in_upper.contains(target_rel) || map.contains_key(target_rel) {
                continue;
            }
            let mut leaves = Vec::new();
            collect_subtree_leaf_paths(gix_repo, base_tree, target_rel, &mut leaves)?;
            for leaf in leaves {
                if !present_in_upper.contains(&leaf) {
                    map.entry(leaf).or_insert(DirtyFileState::Deleted);
                }
            }
        }
    }

    let opaque_dir = meta_dir.join("opaque");
    if opaque_dir.is_dir() {
        for entry in walkdir::WalkDir::new(&opaque_dir).follow_links(false) {
            let entry = entry.map_err(|err| OzError::Other(err.to_string()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(rel_path) = entry.path().strip_prefix(&opaque_dir) else {
                continue;
            };
            let Some(rel_raw) = path_to_rel_string(rel_path) else {
                continue;
            };
            let Some(dir_rel) = rel_raw.strip_suffix(".oz_opaque") else {
                continue;
            };
            let mut leaves = Vec::new();
            collect_subtree_leaf_paths(gix_repo, base_tree, dir_rel, &mut leaves)?;
            for leaf in leaves {
                if !present_in_upper.contains(&leaf) {
                    map.entry(leaf).or_insert(DirtyFileState::Deleted);
                }
            }
        }
    }

    Ok(map
        .into_iter()
        .map(|(path, state)| DirtyFileSnapshot { path, state })
        .collect())
}

fn snapshot_from_standard_workdir(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    base_tree: &gix::Tree<'_>,
) -> Result<Vec<DirtyFileSnapshot>> {
    let hash_kind = gix_repo.object_hash();
    let workdir = &repo.workdir_path;

    // Collect tracked leaf entries from base_tree.
    let mut tracked_entries: Vec<(String, u32, ObjectId)> = Vec::new();
    let mut tracked_set: HashSet<String> = HashSet::new();
    if !base_tree.id.is_empty_tree() {
        let records = base_tree
            .traverse()
            .breadthfirst
            .files()
            .map_err(|err| OzError::Git(err.to_string()))?;
        tracked_entries.reserve(records.len());
        tracked_set.reserve(records.len());
        for rec in records {
            if rec.mode.is_tree() {
                continue;
            }
            let rel = rec.filepath.to_str_lossy().into_owned();
            tracked_set.insert(rel.clone());
            tracked_entries.push((rel, entry_mode_to_u32(rec.mode), rec.oid));
        }
    }

    // Load `.git/index` as a fast stat-cache only when it governs `repo.workdir_path`.
    let can_use_index = repo.workspace.is_none() || repo.is_linked_worktree;
    let index_opt = if can_use_index && gix_repo.index_path().is_file() {
        gix_repo.open_index().ok()
    } else {
        None
    };

    let mut index_lookup: HashMap<String, (ObjectId, u32, gix::index::entry::Stat)> =
        HashMap::new();
    let mut index_timestamp = None;
    if let Some(ref idx) = index_opt {
        index_timestamp = Some(idx.timestamp());
        index_lookup.reserve(idx.entries().len());
        for entry in idx.entries() {
            if let Some(mode) = entry.mode.to_tree_entry_mode() {
                let rel = entry.path(idx).to_str_lossy().into_owned();
                index_lookup.insert(rel, (entry.id, entry_mode_to_u32(mode), entry.stat));
            }
        }
    }

    let stat_opts = gix::index::entry::stat::Options::default();

    // Pass 1 (parallel): verify tracked files against `.git/index` stat shortcut or SHA-1 hash.
    let tracked_results: Result<Vec<Option<ScanItem>>> = tracked_entries
        .par_iter()
        .map(|(rel, tree_mode, tree_oid)| {
            let disk_path = workdir.join(rel);
            let meta = match fs::symlink_metadata(&disk_path) {
                Ok(m) => m,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Some(ScanItem::Deleted(rel.clone())));
                }
                Err(err) => return Err(OzError::Io(err)),
            };

            if meta.is_dir() {
                // Replaced tracked file by a directory -> tracked file itself is gone.
                return Ok(Some(ScanItem::Deleted(rel.clone())));
            }

            if let (Some(idx_ts), Some((idx_oid, idx_mode, idx_stat))) =
                (index_timestamp, index_lookup.get(rel))
                && idx_oid == tree_oid
                && idx_mode == tree_mode
                && !idx_stat.is_racy(idx_ts, stat_opts)
                && let Ok(fs_meta) = gix::index::fs::Metadata::from_path_no_follow(&disk_path)
                && let Ok(disk_stat) = gix::index::entry::Stat::from_fs(&fs_meta)
                && idx_stat.matches(&disk_stat, stat_opts)
            {
                return Ok(None);
            }

            let (bytes, disk_mode) = read_disk_blob_and_mode(&disk_path, &meta)?;
            let computed_oid = compute_blob_oid(hash_kind, &bytes)?;
            if computed_oid == *tree_oid && disk_mode == *tree_mode {
                Ok(None)
            } else {
                Ok(Some(ScanItem::Present(PendingDirtyDiskFile {
                    rel_path: rel.clone(),
                    bytes,
                    mode: disk_mode,
                })))
            }
        })
        .collect();

    // Pass 2 (parallel): walk non-ignored untracked files not in `base_tree`.
    let untracked_paths = Mutex::new(Vec::<(String, PathBuf)>::new());
    if workdir.is_dir() {
        let mut builder = WalkBuilder::new(workdir);
        builder
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            .parents(false)
            .require_git(false)
            .threads(rayon::current_num_threads().clamp(1, 8))
            .filter_entry(|entry| entry.file_name() != ".git");

        let exclude_path = repo.git_common_dir.join("info").join("exclude");
        if exclude_path.is_file() {
            builder.add_ignore(exclude_path);
        }

        let tracked_ref = &tracked_set;
        let untracked_ref = &untracked_paths;
        builder.build_parallel().run(|| {
            Box::new(move |entry_res| {
                let Ok(entry) = entry_res else {
                    return ignore::WalkState::Continue;
                };
                if entry.depth() == 0 {
                    return ignore::WalkState::Continue;
                }
                let Some(ft) = entry.file_type() else {
                    return ignore::WalkState::Continue;
                };
                if ft.is_dir() || (!ft.is_file() && !ft.is_symlink()) {
                    return ignore::WalkState::Continue;
                }
                let Ok(rel_path) = entry.path().strip_prefix(workdir) else {
                    return ignore::WalkState::Continue;
                };
                let Some(rel) = path_to_rel_string(rel_path) else {
                    return ignore::WalkState::Continue;
                };
                if tracked_ref.contains(&rel) {
                    return ignore::WalkState::Continue;
                }
                if let Ok(mut guard) = untracked_ref.lock() {
                    guard.push((rel, entry.path().to_path_buf()));
                }
                ignore::WalkState::Continue
            })
        });
    }

    let untracked_list = untracked_paths
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let untracked_items: Result<Vec<PendingDirtyDiskFile>> = untracked_list
        .into_par_iter()
        .map(|(rel, disk_path)| {
            let meta = fs::symlink_metadata(&disk_path)?;
            let (bytes, mode) = read_disk_blob_and_mode(&disk_path, &meta)?;
            Ok(PendingDirtyDiskFile {
                rel_path: rel,
                bytes,
                mode,
            })
        })
        .collect();

    let mut dirty_map: BTreeMap<String, DirtyFileState> = BTreeMap::new();
    for item in tracked_results?.into_iter().flatten() {
        match item {
            ScanItem::Deleted(rel) => {
                dirty_map.insert(rel, DirtyFileState::Deleted);
            }
            ScanItem::Present(file) => {
                let blob_oid = write_blob_if_needed(gix_repo, &file.bytes)?;
                dirty_map.insert(
                    file.rel_path,
                    DirtyFileState::Present {
                        blob_oid,
                        mode: file.mode,
                    },
                );
            }
        }
    }

    for file in untracked_items? {
        let blob_oid = write_blob_if_needed(gix_repo, &file.bytes)?;
        dirty_map.insert(
            file.rel_path,
            DirtyFileState::Present {
                blob_oid,
                mode: file.mode,
            },
        );
    }

    Ok(dirty_map
        .into_iter()
        .map(|(path, state)| DirtyFileSnapshot { path, state })
        .collect())
}

/// Builds a Git tree OID over `base_tree_id` applying ONLY `dirty_files` (`O(dirty_files)`)
/// via `gix::objs::tree::Editor` (never rebuilding untouched subtrees).
pub(crate) fn build_dirty_tree(
    gix_repo: &gix::Repository,
    base_tree_id: ObjectId,
    dirty_files: &[DirtyFileSnapshot],
) -> Result<ObjectId> {
    if dirty_files.is_empty() {
        return Ok(base_tree_id);
    }

    let mut editor = gix_repo
        .edit_tree(base_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;

    for snap in dirty_files {
        match &snap.state {
            DirtyFileState::Present { blob_oid, mode } => {
                let oid = parse_oid(blob_oid)?;
                editor
                    .upsert(snap.path.as_str(), entry_kind_from_mode(*mode), oid)
                    .map_err(|err| OzError::Git(err.to_string()))?;
            }
            DirtyFileState::Deleted => {
                editor
                    .remove(snap.path.as_str())
                    .map_err(|err| OzError::Git(err.to_string()))?;
            }
        }
    }

    let new_tree_id = editor
        .write()
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(new_tree_id.detach())
}

/// Computes the set of leaf file paths that differ between two Git trees (`O(changed_subtrees)`).
fn tree_diff_paths(
    gix_repo: &gix::Repository,
    lhs_tree_id: ObjectId,
    rhs_tree_id: ObjectId,
) -> Result<BTreeSet<String>> {
    let mut changed = BTreeSet::new();
    if lhs_tree_id == rhs_tree_id {
        return Ok(changed);
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

    for change in recorder.records {
        match change {
            gix::diff::tree::recorder::Change::Addition {
                entry_mode, path, ..
            }
            | gix::diff::tree::recorder::Change::Deletion {
                entry_mode, path, ..
            } => {
                if !entry_mode.is_tree() {
                    changed.insert(path.to_str_lossy().into_owned());
                }
            }
            gix::diff::tree::recorder::Change::Modification {
                previous_entry_mode,
                entry_mode,
                path,
                ..
            } => {
                if !previous_entry_mode.is_tree() || !entry_mode.is_tree() {
                    changed.insert(path.to_str_lossy().into_owned());
                }
            }
        }
    }

    Ok(changed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedLeaf {
    File { oid: ObjectId, mode: u32 },
    Absent,
}

fn resolve_target_leaf(
    target_tree: &gix::Tree<'_>,
    target_dirty_map: &BTreeMap<String, DirtyFileState>,
    rel_path: &str,
) -> Result<ResolvedLeaf> {
    if let Some(state) = target_dirty_map.get(rel_path) {
        return Ok(match state {
            DirtyFileState::Present { blob_oid, mode } => ResolvedLeaf::File {
                oid: parse_oid(blob_oid)?,
                mode: *mode,
            },
            DirtyFileState::Deleted => ResolvedLeaf::Absent,
        });
    }

    match target_tree
        .lookup_entry_by_path(rel_path)
        .map_err(|err| OzError::Git(err.to_string()))?
    {
        Some(entry) if !entry.mode().is_tree() => Ok(ResolvedLeaf::File {
            oid: entry.object_id(),
            mode: entry_mode_to_u32(entry.mode()),
        }),
        _ => Ok(ResolvedLeaf::Absent),
    }
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
        match fs::symlink_metadata(&acc) {
            Ok(meta) if !meta.is_dir() => {
                fs::remove_file(&acc)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn write_resolved_leaf(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    rel_path: &str,
    leaf: &ResolvedLeaf,
) -> Result<bool> {
    let disk_path = repo.workdir_path.join(rel_path);

    match leaf {
        ResolvedLeaf::Absent => {
            match fs::symlink_metadata(&disk_path) {
                Ok(meta) => {
                    if meta.is_dir() {
                        fs::remove_dir_all(&disk_path)?;
                    } else {
                        fs::remove_file(&disk_path)?;
                    }
                    prune_empty_parents(&repo.workdir_path, &disk_path);
                    Ok(true)
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(err) => Err(OzError::Io(err)),
            }
        }
        ResolvedLeaf::File { oid, mode } => {
            // Fast check if file on disk already matches exact OID and mode.
            if let Ok(meta) = fs::symlink_metadata(&disk_path)
                && !meta.is_dir()
            {
                let is_link = meta.file_type().is_symlink();
                if (*mode == MODE_LINK) == is_link
                    && let Ok((existing_bytes, existing_mode)) =
                        read_disk_blob_and_mode(&disk_path, &meta)
                    && let Ok(existing_oid) =
                        compute_blob_oid(gix_repo.object_hash(), &existing_bytes)
                    && existing_oid == *oid
                    && existing_mode == *mode
                {
                    return Ok(false);
                }
            }

            remove_conflicting_non_dir_ancestors(&repo.workdir_path, &disk_path)?;
            if let Some(parent) = disk_path.parent() {
                fs::create_dir_all(parent)?;
            }

            let blob = gix_repo
                .find_blob(*oid)
                .map_err(|err| OzError::Git(err.to_string()))?;

            if let Ok(meta) = fs::symlink_metadata(&disk_path) {
                if meta.is_dir() {
                    fs::remove_dir_all(&disk_path)?;
                } else if meta.file_type().is_symlink() || *mode == MODE_LINK {
                    fs::remove_file(&disk_path)?;
                }
            }

            if *mode == MODE_LINK {
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
                // Direct write through `workdir_path` atomically updates both real files and
                // FUSE3 `upper/` + `whiteouts/` without extra temp-file churn in `upper/`.
                fs::write(&disk_path, &blob.data)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let perm_bits = if *mode == MODE_EXEC { 0o755 } else { 0o644 };
                    fs::set_permissions(&disk_path, fs::Permissions::from_mode(perm_bits))?;
                }
            }

            Ok(true)
        }
    }
}

/// Restores ONLY changed paths between `(src_base_tree, src_dirty_files)` and
/// `(dst_base_tree, dst_dirty_files)` in `repo.workdir_path` (`O(dirty_files)`).
pub(crate) fn restore_working_tree(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    src_base_tree: ObjectId,
    src_dirty_files: &[DirtyFileSnapshot],
    dst_base_tree: ObjectId,
    dst_dirty_files: &[DirtyFileSnapshot],
) -> Result<usize> {
    let mut candidate_paths = tree_diff_paths(gix_repo, src_base_tree, dst_base_tree)?;
    for snap in src_dirty_files {
        candidate_paths.insert(snap.path.clone());
    }
    let mut dst_dirty_map: BTreeMap<String, DirtyFileState> = BTreeMap::new();
    for snap in dst_dirty_files {
        candidate_paths.insert(snap.path.clone());
        dst_dirty_map.insert(snap.path.clone(), snap.state.clone());
    }

    if candidate_paths.is_empty() {
        return Ok(0);
    }

    let dst_tree = gix_repo
        .find_tree(dst_base_tree)
        .map_err(|err| OzError::Git(err.to_string()))?;

    let mut touched = 0usize;
    for rel_path in candidate_paths {
        let target_leaf = resolve_target_leaf(&dst_tree, &dst_dirty_map, &rel_path)?;
        if write_resolved_leaf(repo, gix_repo, &rel_path, &target_leaf)? {
            touched += 1;
        }
    }

    Ok(touched)
}

/// Compares two snapshot states (`base_tree` + sorted `dirty_files`) for exact equality.
#[must_use]
pub(crate) fn snapshots_match(
    base_a: &str,
    dirty_a: &[DirtyFileSnapshot],
    base_b: &str,
    dirty_b: &[DirtyFileSnapshot],
) -> bool {
    if base_a != base_b {
        return false;
    }
    let map_a: BTreeMap<&str, &DirtyFileState> =
        dirty_a.iter().map(|d| (d.path.as_str(), &d.state)).collect();
    let map_b: BTreeMap<&str, &DirtyFileState> =
        dirty_b.iter().map(|d| (d.path.as_str(), &d.state)).collect();
    map_a == map_b
}

/// In-memory workspace snapshot state (used for building pre/post [`WsSnapshot`] pairs).
#[derive(Debug, Clone)]
pub(crate) struct SnapshotSlice {
    pub(crate) base_tree: ObjectId,
    pub(crate) dirty_tree: Option<ObjectId>,
    pub(crate) dirty_files: Vec<DirtyFileSnapshot>,
    pub(crate) head_commit: Option<ObjectId>,
    pub(crate) branch: Option<String>,
}

/// Builds a complete [`WsSnapshot`] from pre/post [`SnapshotSlice`] states.
pub(crate) fn make_ws_snapshot(post: SnapshotSlice, pre: Option<SnapshotSlice>) -> WsSnapshot {
    let (pre_base_tree, pre_dirty_tree, pre_dirty_files, pre_head_commit, pre_branch) = match pre
    {
        Some(p) => (
            Some(p.base_tree.to_hex().to_string()),
            p.dirty_tree.map(|id| id.to_hex().to_string()),
            p.dirty_files,
            p.head_commit.map(|id| id.to_hex().to_string()),
            p.branch,
        ),
        None => (None, None, Vec::new(), None, None),
    };
    WsSnapshot {
        base_tree: post.base_tree.to_hex().to_string(),
        dirty_tree: post.dirty_tree.map(|id| id.to_hex().to_string()),
        dirty_files: post.dirty_files,
        pre_base_tree,
        pre_dirty_tree,
        pre_dirty_files,
        head_commit: post.head_commit.map(|id| id.to_hex().to_string()),
        pre_head_commit,
        branch: post.branch.or(pre_branch),
    }
}

/// Updates `.git/ozone/state/<ws>.json` atomically if the state file already exists.
pub(crate) fn sync_workspace_state_file(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    base_commit: Option<ObjectId>,
    base_tree: ObjectId,
    dirty_files: &[DirtyFileSnapshot],
    last_checkpoint_oid: Option<&str>,
    branch_override: Option<&str>,
) -> Result<()> {
    let Some(ref ws_name) = repo.workspace else {
        return Ok(());
    };
    let state_path = repo.layout.workspace_state_path(ws_name);
    if !state_path.is_file() {
        return Ok(());
    }

    let raw = fs::read_to_string(&state_path)?;
    let mut state: WorkspaceState = serde_json::from_str(&raw)?;
    let tree = gix_repo
        .find_tree(base_tree)
        .map_err(|err| OzError::Git(err.to_string()))?;

    let now = Utc::now();
    state.base_commit = base_commit.map(|id| id.to_hex().to_string());
    state.base_tree = base_tree.to_hex().to_string();
    if let Some(branch) = branch_override {
        state.branch = branch.to_string();
    }

    let mut new_dirty = BTreeMap::new();
    for snap in dirty_files {
        let entry = match &snap.state {
            DirtyFileState::Deleted => DirtyEntry {
                kind: ChangeKind::Deleted,
                recorded_at: now,
                size_bytes: None,
                oid_hint: None,
                mtime_nanos: None,
            },
            DirtyFileState::Present { blob_oid, .. } => {
                let in_base = tree
                    .lookup_entry_by_path(&snap.path)
                    .map_err(|err| OzError::Git(err.to_string()))?
                    .is_some();
                let size_bytes = fs::symlink_metadata(repo.workdir_path.join(&snap.path))
                    .ok()
                    .map(|m| m.len());
                DirtyEntry {
                    kind: if in_base {
                        ChangeKind::Modified
                    } else {
                        ChangeKind::Added
                    },
                    recorded_at: now,
                    size_bytes,
                    oid_hint: Some(blob_oid.clone()),
                    mtime_nanos: None,
                }
            }
        };
        new_dirty.insert(snap.path.clone(), entry);
    }
    state.dirty_paths = new_dirty;
    if let Some(cp_oid) = last_checkpoint_oid {
        state.last_checkpoint_oid = Some(cp_oid.to_string());
    }
    state.updated_at = now;

    let bytes = serde_json::to_vec_pretty(&state)?;
    atomic_write_bytes(&state_path, &bytes)?;
    Ok(())
}

/// Checks if `gix_repo` currently holds a ref target (hex string) for `ref_name`.
pub(crate) fn read_ref_oid_hex(gix_repo: &gix::Repository, ref_name: &str) -> Result<Option<String>> {
    match gix_repo
        .try_find_reference(ref_name)
        .map_err(|err| OzError::Git(err.to_string()))?
    {
        Some(r) => match r.target() {
            gix::refs::TargetRef::Object(oid) => Ok(Some(oid.to_hex().to_string())),
            gix::refs::TargetRef::Symbolic(_) => {
                let mut owned = r;
                let id = owned
                    .peel_to_id()
                    .map_err(|err| OzError::Git(err.to_string()))?;
                Ok(Some(id.to_hex().to_string()))
            }
        },
        None => Ok(None),
    }
}
