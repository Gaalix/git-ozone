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
    ChangeKind, DirtyEntry, FileChange, MountHealth, OzError, OzoneRepo, Result, WorkspaceState,
};
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

const MODE_BLOB: u32 = 0o100_644;
const MODE_EXEC: u32 = 0o100_755;
const MODE_LINK: u32 = 0o120_000;

/// Returns the workspace identifier key used in `ws_snapshots` and checkpoint refs.
#[must_use]
pub(crate) fn workspace_key(repo: &OzoneRepo) -> &str {
    repo.workspace.as_deref().unwrap_or(DEFAULT_WORKSPACE_KEY)
}

/// Ensures a workspace mount is not disconnected (`ENOTCONN`) before touching its working tree.
pub(crate) fn ensure_mount_healthy(repo: &OzoneRepo) -> Result<()> {
    if repo.mount_health == MountHealth::DisconnectedEnotconn {
        return Err(OzError::MountUnavailable {
            workspace: workspace_key(repo).to_string(),
            path: repo.workdir_path.clone(),
            errno: Some(107),
            message: "workspace FUSE mount returned ENOTCONN; remount before operating".to_string(),
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
    .map_err(OzError::git_source)?;

    gix_repo
        .committer_or_set_fallback("Ozone", "bot@ozone.dev")
        .map_err(OzError::git_source)?;
    Ok(gix_repo)
}

/// Resolves the live branch, `HEAD` commit OID (if born), and base tree OID for `repo`.
pub(crate) fn resolve_workspace_head(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
) -> Result<(Option<String>, Option<ObjectId>, ObjectId)> {
    let empty_tree = ObjectId::empty_tree(gix_repo.object_hash());

    if repo.workspace.is_some() && !repo.is_linked_worktree {
        let mut virtual_branch = repo.branch.clone();
        if let Some(ref ws_name) = repo.workspace {
            let state_path = repo.layout.workspace_state_path(ws_name);
            if state_path.is_file()
                && let Ok(raw) = fs::read_to_string(&state_path)
                && let Ok(state) = serde_json::from_str::<WorkspaceState>(&raw)
            {
                virtual_branch = Some(state.branch);
            }
        }
        if let Some(ref branch) = virtual_branch {
            let ref_name = format!("refs/heads/{branch}");
            if let Some(mut reference) = gix_repo
                .try_find_reference(&ref_name)
                .map_err(OzError::git_source)?
            {
                let commit = reference
                    .peel_to_commit()
                    .map_err(OzError::git_source)?;
                let tree_id = commit
                    .tree_id()
                    .map_err(OzError::git_source)?
                    .detach();
                return Ok((Some(branch.clone()), Some(commit.id), tree_id));
            }
            return Ok((Some(branch.clone()), None, empty_tree));
        }
    }

    let mut head = gix_repo.head().map_err(OzError::git_source)?;
    let branch = head
        .referent_name()
        .map(|name| String::from_utf8_lossy(name.shorten().as_ref()).into_owned());
    if head.is_unborn() {
        return Ok((branch, None, empty_tree));
    }
    let commit = head.peel_to_commit().map_err(OzError::git_source)?;
    let tree_id = commit.tree_id().map_err(OzError::git_source)?.detach();
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
    normalized
        .strip_prefix("./")
        .unwrap_or(&normalized)
        .to_string()
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
        .map_err(|err| OzError::git_context(format!("invalid object id '{hex}'"), err))
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
    gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, bytes).map_err(OzError::git_source)
}

fn write_blob_if_needed(gix_repo: &gix::Repository, bytes: &[u8]) -> Result<String> {
    let oid = gix_repo.write_blob(bytes).map_err(OzError::git_source)?;
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
    if let Some(ref record) = repo.mount_record {
        if let (Some(upper), Some(meta)) = (&record.upper_dir, &record.meta_dir) {
            return Some((upper.clone(), meta.clone()));
        }
        if record.backend.requires_daemon() {
            let upper = repo.layout.ws_upper_dir(&record.workspace);
            let meta = repo.layout.ws_tombstone_dir(&record.workspace);
            if upper.is_dir() || meta.is_dir() {
                return Some((upper, meta));
            }
        }
    }
    if let Some(ref state) = repo.workspace_state
        && state.backend.requires_daemon()
    {
        let upper = repo.layout.ws_upper_dir(&state.name);
        let meta = repo.layout.ws_tombstone_dir(&state.name);
        if upper.is_dir() || meta.is_dir() {
            return Some((upper, meta));
        }
    }
    None
}

fn build_gitignore_matcher(
    repo: &OzoneRepo,
    upper_dir: Option<&Path>,
    base_tree: Option<&gix::Tree<'_>>,
) -> Option<Gitignore> {
    let root = &repo.workdir_path;
    let mut builder = GitignoreBuilder::new(root);
    let mut any = false;

    let upper_gi = upper_dir.map(|u| u.join(".gitignore"));
    let workdir_gi = root.join(".gitignore");
    let repo_gi = repo.repo_root.join(".gitignore");

    if let Some(ref u_gi) = upper_gi
        && u_gi.is_file()
    {
        let _ = builder.add(u_gi);
        any = true;
    } else if workdir_gi.is_file() {
        let _ = builder.add(&workdir_gi);
        any = true;
    } else if repo_gi.is_file() {
        let _ = builder.add(&repo_gi);
        any = true;
    } else if let Some(tree) = base_tree
        && let Ok(Some(entry)) = tree.lookup_entry_by_path(".gitignore")
        && !entry.mode().is_tree()
        && let Ok(obj) = entry.object()
    {
        let content = String::from_utf8_lossy(&obj.data);
        for line in content.lines() {
            let _ = builder.add_line(None, line);
        }
        any = true;
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
        .map_err(OzError::git_source)?
    else {
        return Ok(());
    };
    if !entry.mode().is_tree() {
        out.push(rel_dir.to_string());
        return Ok(());
    }
    let subtree = gix_repo
        .find_tree(entry.object_id())
        .map_err(OzError::git_source)?;
    let files = subtree
        .traverse()
        .breadthfirst
        .files()
        .map_err(OzError::git_source)?;
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
        .map_err(OzError::git_source)?;

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
                    .map_err(OzError::git_source)?
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
                        .map_err(OzError::git_source)?
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
            map.insert(rel.to_string(), DirtyFileState::Present { blob_oid, mode });
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if base_tree
                .lookup_entry_by_path(rel)
                .map_err(OzError::git_source)?
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
    let gi = build_gitignore_matcher(repo, Some(upper_dir), Some(base_tree));
    let mut map: BTreeMap<String, DirtyFileState> = BTreeMap::new();
    let mut present_in_upper = HashSet::new();

    if upper_dir.is_dir() {
        let walker = walkdir::WalkDir::new(upper_dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                if e.file_name() == ".git" {
                    return false;
                }
                if e.file_type().is_dir()
                    && let Ok(rel_path) = e.path().strip_prefix(upper_dir)
                    && let Some(rel) = path_to_rel_string(rel_path)
                    && is_ignored_rel(gi.as_ref(), &rel, true)
                    && matches!(base_tree.lookup_entry_by_path(&rel), Ok(None))
                {
                    // Prune untracked gitignored directories (e.g. `upper/target/`) immediately
                    // without descending into any files inside them.
                    return false;
                }
                true
            });

        for entry in walker {
            let entry = entry.map_err(|err| OzError::Other(err.to_string()))?;
            if entry.depth() == 0 {
                continue;
            }
            let ft = entry.file_type();
            if ft.is_dir() || (!ft.is_file() && !ft.is_symlink()) {
                continue;
            }
            let Ok(rel_path) = entry.path().strip_prefix(upper_dir) else {
                continue;
            };
            let Some(rel) = path_to_rel_string(rel_path) else {
                continue;
            };

            let tree_entry = base_tree
                .lookup_entry_by_path(&rel)
                .map_err(OzError::git_source)?;

            // Skip untracked gitignored files BEFORE reading or SHA-1 hashing their bytes.
            if tree_entry.is_none() && is_ignored_rel(gi.as_ref(), &rel, false) {
                continue;
            }

            present_in_upper.insert(rel.clone());

            let full_path = entry.path();
            let meta = fs::symlink_metadata(full_path)?;
            let (bytes, mode) = read_disk_blob_and_mode(full_path, &meta)?;
            let computed_oid = compute_blob_oid(hash_kind, &bytes)?;

            if let Some(base_entry) = tree_entry
                && base_entry.object_id() == computed_oid
                && entry_mode_to_u32(base_entry.mode()) == mode
            {
                // Committed files remain in `upper_dir`; skip when bytes & mode match HEAD.
                continue;
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
            .map_err(OzError::git_source)?;
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

            #[cfg(unix)]
            let disk_mode_matches = {
                use std::os::unix::fs::PermissionsExt;
                let disk_mode = if meta.permissions().mode() & 0o111 != 0 {
                    MODE_EXEC
                } else {
                    MODE_BLOB
                };
                disk_mode == *tree_mode
            };
            #[cfg(not(unix))]
            let disk_mode_matches = true;

            if !meta.file_type().is_symlink()
                && *tree_mode != MODE_LINK
                && disk_mode_matches
                && let (Some(idx_ts), Some((idx_oid, idx_mode, idx_stat))) =
                    (index_timestamp, index_lookup.get(rel))
                && idx_oid == tree_oid
                && idx_mode == tree_mode
                && idx_stat.mtime.secs != 0
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
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        .map_err(OzError::git_source)?;

    for snap in dirty_files {
        match &snap.state {
            DirtyFileState::Present { blob_oid, mode } => {
                let oid = parse_oid(blob_oid)?;
                editor
                    .upsert(snap.path.as_str(), entry_kind_from_mode(*mode), oid)
                    .map_err(OzError::git_source)?;
            }
            DirtyFileState::Deleted => {
                editor
                    .remove(snap.path.as_str())
                    .map_err(OzError::git_source)?;
            }
        }
    }

    let new_tree_id = editor.write().map_err(OzError::git_source)?;
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
        .map_err(OzError::git_source)?;
    let rhs_tree = gix_repo
        .find_tree(rhs_tree_id)
        .map_err(OzError::git_source)?;

    let mut state = gix::diff::tree::State::default();
    let mut recorder = gix::diff::tree::Recorder::default()
        .track_location(Some(gix::diff::tree::recorder::Location::Path));

    let lhs_iter = gix::objs::TreeRefIter::from_bytes(&lhs_tree.data, lhs_tree.id.kind());
    let rhs_iter = gix::objs::TreeRefIter::from_bytes(&rhs_tree.data, rhs_tree.id.kind());

    gix::diff::tree(
        lhs_iter,
        rhs_iter,
        &mut state,
        &gix_repo.objects,
        &mut recorder,
    )
    .map_err(OzError::git_source)?;

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
        .map_err(OzError::git_source)?
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
        ResolvedLeaf::Absent => match fs::symlink_metadata(&disk_path) {
            Ok(meta) => {
                if meta.is_dir() {
                    fs::remove_dir_all(&disk_path)?;
                } else {
                    fs::remove_file(&disk_path)?;
                }
                prune_empty_parents(&repo.workdir_path, &disk_path);
                Ok(true)
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::NotFound
                    || err.kind() == std::io::ErrorKind::NotADirectory =>
            {
                Ok(false)
            }
            Err(err) => Err(OzError::Io(err)),
        },
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

            let blob = gix_repo.find_blob(*oid).map_err(OzError::git_source)?;

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
/// `(dst_base_tree, dst_dirty_files)` in `repo.workdir_path` (`O(dirty_files)`),
/// returning `(touched_files_count, touched_candidate_paths)`.
pub(crate) fn restore_working_tree(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    src_base_tree: ObjectId,
    src_dirty_files: &[DirtyFileSnapshot],
    dst_base_tree: ObjectId,
    dst_dirty_files: &[DirtyFileSnapshot],
) -> Result<(usize, BTreeSet<String>)> {
    let tree_changes = tree_diff_paths(gix_repo, src_base_tree, dst_base_tree)?;
    let mut candidate_paths = tree_changes;
    for snap in src_dirty_files {
        candidate_paths.insert(snap.path.clone());
    }
    let mut dst_dirty_map: BTreeMap<String, DirtyFileState> = BTreeMap::new();
    for snap in dst_dirty_files {
        candidate_paths.insert(snap.path.clone());
        dst_dirty_map.insert(snap.path.clone(), snap.state.clone());
    }

    if candidate_paths.is_empty() {
        return Ok((0, candidate_paths));
    }

    let dst_tree = gix_repo
        .find_tree(dst_base_tree)
        .map_err(OzError::git_source)?;

    let mut touched = 0usize;
    for rel_path in &candidate_paths {
        let target_leaf = resolve_target_leaf(&dst_tree, &dst_dirty_map, rel_path)?;
        if write_resolved_leaf(repo, gix_repo, rel_path, &target_leaf)? {
            touched += 1;
        }
    }

    Ok((touched, candidate_paths))
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
    let map_a: BTreeMap<&str, &DirtyFileState> = dirty_a
        .iter()
        .map(|d| (d.path.as_str(), &d.state))
        .collect();
    let map_b: BTreeMap<&str, &DirtyFileState> = dirty_b
        .iter()
        .map(|d| (d.path.as_str(), &d.state))
        .collect();
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
    let post_detached = post.branch.is_none();
    let (pre_base_tree, pre_dirty_tree, pre_dirty_files, pre_head_commit, pre_branch, pre_detached) =
        match pre {
            Some(p) => {
                let detached = p.branch.is_none();
                (
                    Some(p.base_tree.to_hex().to_string()),
                    p.dirty_tree.map(|id| id.to_hex().to_string()),
                    p.dirty_files,
                    p.head_commit.map(|id| id.to_hex().to_string()),
                    p.branch,
                    detached,
                )
            }
            None => (None, None, Vec::new(), None, None, false),
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
        branch: post.branch,
        pre_branch,
        pre_detached,
        post_detached,
    }
}

/// Patches ONLY mismatched paths from `changed_paths` inside `.git/index` (or
/// `.git/worktrees/<ws>/index`) to match `target_tree_id`, keeping system `git status`
/// and `oz status` accurate after `oz undo` / `oz redo` without rewriting `.git/index`
/// on dirty-only checkpoints.
///
/// Any path present in `dst_dirty_files` differs from `target_tree_id` on disk and MUST
/// receive a zeroed [`gix::index::entry::Stat::default()`] (never `Stat::from_fs`),
/// otherwise `.git/index` would pair the target tree's blob OID with the edited file's
/// stat and hide uncommitted edits from both `git status` and `oz status`.
pub(crate) fn sync_git_index_diff(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    target_tree_id: ObjectId,
    dst_dirty_files: &[DirtyFileSnapshot],
    changed_paths: &BTreeSet<String>,
) -> Result<()> {
    struct IndexPatch<'a> {
        rel: &'a str,
        target: Option<(ObjectId, gix::index::entry::Mode)>,
        is_dirty: bool,
    }

    if changed_paths.is_empty() || (repo.workspace.is_some() && !repo.is_linked_worktree) {
        return Ok(());
    }
    let index_path = repo.git_dir.join("index");
    if !index_path.is_file() {
        return Ok(());
    }

    let (mut index, loaded_from_disk) = match gix::index::File::at(
        &index_path,
        gix_repo.object_hash(),
        true,
        gix::index::decode::Options::default(),
    ) {
        Ok(idx) => (idx, true),
        Err(_) => (
            gix_repo
                .index_from_tree(&target_tree_id)
                .map_err(OzError::git_source)?,
            false,
        ),
    };

    let dst_tree = gix_repo
        .find_tree(target_tree_id)
        .map_err(OzError::git_source)?;

    let dst_dirty_paths: HashSet<&str> = dst_dirty_files.iter().map(|d| d.path.as_str()).collect();
    let stat_opts = gix::index::entry::stat::Options::default();

    let mut patch_paths: Vec<IndexPatch<'_>> = Vec::new();
    for rel in changed_paths {
        let is_dirty = dst_dirty_paths.contains(rel.as_str());
        let target = match dst_tree
            .lookup_entry_by_path(rel.as_str())
            .map_err(OzError::git_source)?
        {
            Some(entry) if !entry.mode().is_tree() => {
                Some((entry.object_id(), entry.mode().kind().into()))
            }
            _ => None,
        };
        let bstr = rel.as_bytes().as_bstr();
        let unconflicted =
            index.entry_by_path_and_stage(bstr, gix::index::entry::Stage::Unconflicted);
        let matches = match (target, unconflicted) {
            (Some((oid, mode)), Some(entry)) => {
                let meta_matches = entry.id == oid
                    && entry.mode == mode
                    && index
                        .entry_by_path_and_stage(bstr, gix::index::entry::Stage::Base)
                        .is_none()
                    && index
                        .entry_by_path_and_stage(bstr, gix::index::entry::Stage::Ours)
                        .is_none()
                    && index
                        .entry_by_path_and_stage(bstr, gix::index::entry::Stage::Theirs)
                        .is_none();
                if !meta_matches {
                    false
                } else if is_dirty && entry.stat.mtime.secs != 0 {
                    // If the existing cached stat for this dirty file could falsely match
                    // disk (e.g. same-second same-size overwrite), force zeroing its stat.
                    let disk_path = repo.workdir_path.join(rel);
                    let risks_false_clean =
                        gix::index::fs::Metadata::from_path_no_follow(&disk_path)
                            .ok()
                            .and_then(|meta| gix::index::entry::Stat::from_fs(&meta).ok())
                            .is_some_and(|disk_stat| {
                                (entry.stat.mtime.secs == disk_stat.mtime.secs
                                    && entry.stat.size == disk_stat.size)
                                    || entry.stat.matches(&disk_stat, stat_opts)
                            });
                    !risks_false_clean
                } else {
                    true
                }
            }
            (None, None) => index.entry_by_path(bstr).is_none(),
            _ => false,
        };
        if !matches {
            patch_paths.push(IndexPatch {
                rel: rel.as_str(),
                target,
                is_dirty,
            });
        }
    }

    if patch_paths.is_empty() && loaded_from_disk {
        return Ok(());
    }

    if !patch_paths.is_empty() {
        let patch_set: BTreeSet<&str> = patch_paths.iter().map(|p| p.rel).collect();
        index.remove_tree();
        index.remove_entries(|_, path, _| patch_set.contains(path.to_str_lossy().as_ref()));

        for patch in patch_paths {
            if let Some((object_id, mode)) = patch.target {
                let stat = if patch.is_dirty
                    || mode == gix::index::entry::Mode::COMMIT
                    || mode == gix::index::entry::Mode::SYMLINK
                {
                    // File on disk differs from `target_tree_id` (or is a gitlink/symlink):
                    // keep stat zeroed so `git status` and `oz status` never mark it clean.
                    gix::index::entry::Stat::default()
                } else {
                    let disk_path = repo.workdir_path.join(patch.rel);
                    gix::index::fs::Metadata::from_path_no_follow(&disk_path)
                        .ok()
                        .and_then(|meta| gix::index::entry::Stat::from_fs(&meta).ok())
                        .unwrap_or_default()
                };
                index.dangerously_push_entry(
                    stat,
                    object_id,
                    gix::index::entry::Flags::empty(),
                    mode,
                    patch.rel.as_bytes().as_bstr(),
                );
            }
        }
        index.sort_entries();
    }
    index
        .write(gix::index::write::Options {
            extensions: gix::index::write::Extensions::None,
            skip_hash: true,
        })
        .map_err(OzError::git_source)?;
    Ok(())
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
    let tree = gix_repo.find_tree(base_tree).map_err(OzError::git_source)?;

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
                    .map_err(OzError::git_source)?
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
pub(crate) fn read_ref_oid_hex(
    gix_repo: &gix::Repository,
    ref_name: &str,
) -> Result<Option<String>> {
    match gix_repo
        .try_find_reference(ref_name)
        .map_err(OzError::git_source)?
    {
        Some(r) => match r.target() {
            gix::refs::TargetRef::Object(oid) => Ok(Some(oid.to_hex().to_string())),
            gix::refs::TargetRef::Symbolic(_) => {
                let mut owned = r;
                let id = owned.peel_to_id().map_err(OzError::git_source)?;
                Ok(Some(id.to_hex().to_string()))
            }
        },
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: stdout={}, stderr={}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn backdate_file(path: &Path, secs_ago: u64) {
        let target = SystemTime::now() - Duration::from_secs(secs_ago);
        let file = fs::File::options()
            .write(true)
            .open(path)
            .expect("open file for set_modified");
        file.set_modified(target).expect("set_modified");
    }

    #[test]
    fn sync_git_index_diff_zeroes_stat_for_dirty_target_and_stamps_clean_target() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("repo");
        fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-b", "main"]);
        git(&dir, &["config", "user.name", "Test"]);
        git(&dir, &["config", "user.email", "test@ozone.dev"]);
        git(&dir, &["config", "core.autocrlf", "false"]);

        fs::write(dir.join("a.txt"), "hello\n").unwrap();
        fs::write(dir.join("clean.txt"), "clean-v1\n").unwrap();
        backdate_file(&dir.join("a.txt"), 20);
        backdate_file(&dir.join("clean.txt"), 20);
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "c1"]);
        let t1_hex = git(&dir, &["rev-parse", "HEAD^{tree}"]);
        let a_b1_hex = git(&dir, &["rev-parse", "HEAD:a.txt"]);

        fs::write(dir.join("a.txt"), "edited-much-longer\n").unwrap();
        fs::write(dir.join("clean.txt"), "clean-v2\n").unwrap();
        backdate_file(&dir.join("a.txt"), 10);
        backdate_file(&dir.join("clean.txt"), 10);
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "c2"]);

        // Simulate `oz undo` where `clean.txt` was restored back to `clean-v1\n`,
        // while `a.txt` intentionally keeps its pre-commit dirty edit on disk.
        fs::write(dir.join("clean.txt"), "clean-v1\n").unwrap();
        backdate_file(&dir.join("clean.txt"), 15);

        let repo = OzoneRepo::discover(Some(&dir)).expect("discover");
        let gix_repo = open_gix_for_workspace(&repo).expect("open gix");
        let t1_oid = parse_oid(&t1_hex).expect("t1");
        let a_b2_oid = compute_blob_oid(gix_repo.object_hash(), b"edited-much-longer\n")
            .unwrap()
            .to_hex()
            .to_string();

        let dst_dirty = vec![DirtyFileSnapshot {
            path: "a.txt".to_string(),
            state: DirtyFileState::Present {
                blob_oid: a_b2_oid,
                mode: MODE_BLOB,
            },
        }];
        let changed: BTreeSet<String> = ["a.txt", "clean.txt"]
            .into_iter()
            .map(str::to_string)
            .collect();

        sync_git_index_diff(&repo, &gix_repo, t1_oid, &dst_dirty, &changed).expect("sync index");

        let idx = gix_repo.open_index().expect("open index");
        let a_entry = idx
            .entry_by_path("a.txt".as_bytes().as_bstr())
            .expect("a.txt in index");
        assert_eq!(a_entry.id.to_hex().to_string(), a_b1_hex);
        assert_eq!(
            a_entry.stat,
            gix::index::entry::Stat::default(),
            "dirty target path must have zeroed stat in .git/index"
        );

        let clean_entry = idx
            .entry_by_path("clean.txt".as_bytes().as_bstr())
            .expect("clean.txt in index");
        assert_ne!(
            clean_entry.stat.mtime.secs, 0,
            "clean restored path should cache disk stat"
        );
        assert_eq!(
            clean_entry.stat.size,
            u32::try_from("clean-v1\n".len()).unwrap()
        );
    }

    #[test]
    fn sync_git_index_diff_invalidates_matching_stat_on_same_oid_dirty_file() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("repo");
        fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-b", "main"]);
        git(&dir, &["config", "user.name", "Test"]);
        git(&dir, &["config", "user.email", "test@ozone.dev"]);

        fs::write(dir.join("a.txt"), "hello\n").unwrap();
        backdate_file(&dir.join("a.txt"), 10);
        let original_mtime = fs::metadata(dir.join("a.txt")).unwrap().modified().unwrap();
        git(&dir, &["add", "a.txt"]);
        git(&dir, &["commit", "-m", "c1"]);

        // Overwrite `a.txt` with same byte length and preserve the exact coarse mtime
        // to simulate a same-second same-size rewrite while `.git/index` still holds c1's OID.
        fs::write(dir.join("a.txt"), "world\n").unwrap();
        fs::File::options()
            .write(true)
            .open(dir.join("a.txt"))
            .unwrap()
            .set_modified(original_mtime)
            .unwrap();

        let repo = OzoneRepo::discover(Some(&dir)).expect("discover");
        let gix_repo = open_gix_for_workspace(&repo).expect("open gix");
        let (_, _, base_tree) = resolve_workspace_head(&repo, &gix_repo).expect("head");
        let world_oid = compute_blob_oid(gix_repo.object_hash(), b"world\n")
            .unwrap()
            .to_hex()
            .to_string();

        let dst_dirty = vec![DirtyFileSnapshot {
            path: "a.txt".to_string(),
            state: DirtyFileState::Present {
                blob_oid: world_oid,
                mode: MODE_BLOB,
            },
        }];
        let changed: BTreeSet<String> = ["a.txt".to_string()].into_iter().collect();

        sync_git_index_diff(&repo, &gix_repo, base_tree, &dst_dirty, &changed).expect("sync index");

        let idx = gix_repo.open_index().expect("open index");
        let entry = idx
            .entry_by_path("a.txt".as_bytes().as_bstr())
            .expect("a.txt entry");
        assert_eq!(
            entry.stat,
            gix::index::entry::Stat::default(),
            "same-OID dirty entry whose cached stat risked false-clean must be zeroed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_from_standard_workdir_detects_mode_change_even_with_cached_stat() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("repo");
        fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-b", "main"]);
        git(&dir, &["config", "user.name", "Test"]);
        git(&dir, &["config", "user.email", "test@ozone.dev"]);

        let script = dir.join("run.sh");
        fs::write(&script, "#!/bin/sh\necho ok\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
        backdate_file(&script, 10);
        git(&dir, &["add", "run.sh"]);
        git(&dir, &["commit", "-m", "c1"]);

        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        backdate_file(&script, 10);

        let repo = OzoneRepo::discover(Some(&dir)).expect("discover");
        let gix_repo = open_gix_for_workspace(&repo).expect("open gix");
        let (_, _, base_tree_id) = resolve_workspace_head(&repo, &gix_repo).expect("head");
        let dirty = scan_dirty_files(&repo, &gix_repo, base_tree_id, None).expect("scan");
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].path, "run.sh");
        assert!(matches!(
            dirty[0].state,
            DirtyFileState::Present {
                mode: MODE_EXEC,
                ..
            }
        ));
    }
}
