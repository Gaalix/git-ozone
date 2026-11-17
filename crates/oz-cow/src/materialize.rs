// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared Git tree delta calculation, `rayon` parallel materialization, reflink cloning,
//! and content-addressed (`0444`/`0555`) hardlink store helpers.

use crate::SpawnRequest;
use filetime::FileTime;
use gix::object::tree::EntryMode;
use gix::object::tree::diff::Change;
use oz_core::{OzError, OzoneConfig, Result, normalize_lexical};
use oz_warm::detect_warm_targets;
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs::Metadata;
use std::ops::ControlFlow;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

/// File-level difference between a baseline Git tree and a target branch tree.
#[derive(Debug, Clone, Default)]
pub struct TreeDelta {
    pub upserts: Vec<(String, gix::ObjectId, EntryMode)>,
    pub deletions: Vec<String>,
}

/// Resolves the shared `.git` common directory from either `.git` or `.git/worktrees/<ws>`.
pub fn resolve_git_common_dir(git_dir: &Path) -> Result<PathBuf> {
    let commondir = git_dir.join("commondir");
    if commondir.is_file() {
        let rel = std::fs::read_to_string(&commondir)?;
        Ok(normalize_lexical(&git_dir.join(rel.trim())))
    } else {
        Ok(git_dir.to_path_buf())
    }
}

/// Attempts to infer the baseline commit's tree OID from `baseline_dir` (`.../baselines/<oid>/root`).
pub fn resolve_baseline_tree_id(
    repo: &gix::Repository,
    baseline_dir: &Path,
) -> Option<gix::ObjectId> {
    let parent = baseline_dir.parent()?;
    let oid_str = parent.file_name()?.to_str()?;
    let commit_id = gix::ObjectId::from_hex(oid_str.as_bytes()).ok()?;
    let commit = repo.find_commit(commit_id).ok()?;
    commit.tree_id().ok().map(|id| id.detach())
}

/// Computes added/modified/deleted blob and symlink paths from `base_tree_id` to `target_tree_id`.
pub fn compute_tree_delta(
    repo: &gix::Repository,
    base_tree_id: gix::ObjectId,
    target_tree_id: gix::ObjectId,
) -> Result<TreeDelta> {
    if base_tree_id == target_tree_id {
        return Ok(TreeDelta::default());
    }

    let base_tree = repo
        .find_tree(base_tree_id)
        .map_err(|e| OzError::Git(e.to_string()))?;
    let target_tree = repo
        .find_tree(target_tree_id)
        .map_err(|e| OzError::Git(e.to_string()))?;

    let mut delta = TreeDelta::default();
    let mut changes = base_tree
        .changes()
        .map_err(|e| OzError::Git(e.to_string()))?;
    changes.options(|opts| {
        opts.track_rewrites(None);
    });
    changes
        .for_each_to_obtain_tree(&target_tree, |change| {
            match change {
                Change::Addition {
                    location,
                    entry_mode,
                    id,
                    ..
                }
                | Change::Modification {
                    location,
                    entry_mode,
                    id,
                    ..
                } => {
                    if !entry_mode.is_tree()
                        && !entry_mode.is_commit()
                        && let Ok(rel) = std::str::from_utf8(location.as_ref())
                    {
                        delta.upserts.push((rel.to_string(), id.detach(), entry_mode));
                    }
                }
                Change::Deletion {
                    location,
                    entry_mode,
                    ..
                } => {
                    if !entry_mode.is_tree()
                        && !entry_mode.is_commit()
                        && let Ok(rel) = std::str::from_utf8(location.as_ref())
                    {
                        delta.deletions.push(rel.to_string());
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
                    if !copy
                        && !source_entry_mode.is_tree()
                        && let Ok(src_rel) = std::str::from_utf8(source_location.as_ref())
                    {
                        delta.deletions.push(src_rel.to_string());
                    }
                    if !entry_mode.is_tree()
                        && !entry_mode.is_commit()
                        && let Ok(dst_rel) = std::str::from_utf8(location.as_ref())
                    {
                        delta
                            .upserts
                            .push((dst_rel.to_string(), id.detach(), entry_mode));
                    }
                }
            }
            Ok::<_, gix::Error>(ControlFlow::Continue(()))
        })
        .map_err(|e| OzError::Git(e.to_string()))?;

    Ok(delta)
}

/// Materializes all files/symlinks from `tree_id` into `dst_root` in parallel using `rayon`.
pub fn materialize_tree_from_odb(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
    dst_root: &Path,
) -> Result<()> {
    let tree = repo
        .find_tree(tree_id)
        .map_err(|e| OzError::Git(e.to_string()))?;
    let entries = tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|e| OzError::Git(e.to_string()))?;

    let mut files = Vec::new();
    let mut dirs = HashSet::new();
    for entry in entries {
        if entry.mode.is_tree() || entry.mode.is_commit() {
            continue;
        }
        let Ok(rel) = std::str::from_utf8(entry.filepath.as_ref()) else {
            continue;
        };
        let dst = dst_root.join(rel);
        if let Some(parent) = dst.parent() {
            dirs.insert(parent.to_path_buf());
        }
        files.push((dst, entry.oid, entry.mode));
    }

    for dir in dirs {
        std::fs::create_dir_all(dir)?;
    }

    let git_dir = repo.git_dir().to_path_buf();
    files
        .par_iter()
        .map_init(
            || gix::open(&git_dir).map_err(|e| OzError::Git(e.to_string())),
            |thread_repo_res, (dst, oid, mode)| {
                let thread_repo = thread_repo_res
                    .as_ref()
                    .map_err(|e| OzError::Git(e.to_string()))?;
                write_git_entry(thread_repo, dst, *oid, *mode)
            },
        )
        .collect::<Result<Vec<()>>>()?;

    Ok(())
}

/// Applies `delta` (deletions and upserts) to `dst_root`.
pub fn apply_tree_delta_to_dir(
    repo: &gix::Repository,
    delta: &TreeDelta,
    dst_root: &Path,
) -> Result<()> {
    for rel in &delta.deletions {
        let p = dst_root.join(rel);
        if p.exists() || std::fs::symlink_metadata(&p).is_ok() {
            let _ = std::fs::remove_file(&p);
        }
    }

    let mut dirs = HashSet::new();
    for (rel, _, _) in &delta.upserts {
        if let Some(parent) = dst_root.join(rel).parent() {
            dirs.insert(parent.to_path_buf());
        }
    }
    for dir in dirs {
        std::fs::create_dir_all(dir)?;
    }

    let git_dir = repo.git_dir().to_path_buf();
    delta
        .upserts
        .par_iter()
        .map_init(
            || gix::open(&git_dir).map_err(|e| OzError::Git(e.to_string())),
            |thread_repo_res, (rel, oid, mode)| {
                let thread_repo = thread_repo_res
                    .as_ref()
                    .map_err(|e| OzError::Git(e.to_string()))?;
                let dst = dst_root.join(rel);
                if std::fs::symlink_metadata(&dst).is_ok() {
                    let _ = std::fs::remove_file(&dst);
                }
                write_git_entry(thread_repo, &dst, *oid, *mode)
            },
        )
        .collect::<Result<Vec<()>>>()?;

    Ok(())
}

fn write_git_entry(
    repo: &gix::Repository,
    dst: &Path,
    oid: gix::ObjectId,
    mode: EntryMode,
) -> Result<()> {
    let blob = repo
        .find_blob(oid)
        .map_err(|e| OzError::Git(e.to_string()))?;
    if mode.is_link() {
        write_symlink_bytes(dst, &blob.data)?;
        return Ok(());
    }
    std::fs::write(dst, &blob.data)?;
    apply_git_mode(dst, mode.is_executable())?;
    Ok(())
}

/// Clones `src_root` into `dst_root` using `reflink_copy::reflink` in parallel.
pub fn clone_dir_with_reflink(src_root: &Path, dst_root: &Path) -> Result<()> {
    std::fs::create_dir_all(dst_root)?;
    let mut jobs = Vec::new();

    for entry in WalkDir::new(src_root).follow_links(false) {
        let entry = entry.map_err(|e| OzError::Other(e.to_string()))?;
        let rel = entry
            .path()
            .strip_prefix(src_root)
            .map_err(|e| OzError::Other(e.to_string()))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dst = dst_root.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dst)?;
            continue;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let meta = std::fs::symlink_metadata(entry.path())?;
        jobs.push((entry.path().to_path_buf(), dst, entry.path_is_symlink(), meta));
    }

    jobs.par_iter().try_for_each(|(src, dst, is_symlink, meta)| -> Result<()> {
        if *is_symlink {
            copy_symlink(src, dst)?;
            return Ok(());
        }
        if meta.is_file() {
            if std::fs::symlink_metadata(dst).is_ok() {
                let _ = std::fs::remove_file(dst);
            }
            reflink_copy::reflink(src, dst)?;
            std::fs::set_permissions(dst, meta.permissions())?;
            let ft = metadata_modified_filetime(meta);
            let _ = set_file_mtime_compat(dst, ft);
        }
        Ok(())
    })
}

/// Materializes a workspace using the CAS hardlink store (`0444`/`0555`) for immutable cache
/// dependencies and `rayon` parallel file copy for editable source files (NEVER hardlinking
/// editable source files!).
pub fn materialize_cas_workspace(
    req: &SpawnRequest,
    cas_dir: &Path,
    git_common_dir: &Path,
) -> Result<()> {
    std::fs::create_dir_all(&req.target_path)?;
    std::fs::create_dir_all(cas_dir)?;

    let warm_roots = collect_warm_root_names(req, git_common_dir);

    let mut tracked_from_odb = false;
    if let Some(ref tree_hex) = req.base_tree
        && let Ok(tree_id) = gix::ObjectId::from_hex(tree_hex.as_bytes())
        && let Ok(repo) = gix::open(&req.repo_root)
    {
        materialize_tree_from_odb(&repo, tree_id, &req.target_path)?;
        tracked_from_odb = true;
    }

    let Some(ref baseline_dir) = req.baseline_dir else {
        return Ok(());
    };
    if !baseline_dir.is_dir() {
        return Ok(());
    }

    let mut dep_jobs = Vec::new();
    let mut source_jobs = Vec::new();

    for entry in WalkDir::new(baseline_dir).follow_links(false) {
        let entry = entry.map_err(|e| OzError::Other(e.to_string()))?;
        let rel = entry
            .path()
            .strip_prefix(baseline_dir)
            .map_err(|e| OzError::Other(e.to_string()))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let is_warm_dep = is_under_warm_root(rel, &warm_roots);
        if tracked_from_odb && !is_warm_dep {
            // Editable tracked files were already freshly written from ODB with distinct inodes.
            continue;
        }

        let dst = req.target_path.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dst)?;
            continue;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let meta = std::fs::symlink_metadata(entry.path())?;
        let job = (entry.path().to_path_buf(), dst, entry.path_is_symlink(), meta);
        if is_warm_dep {
            dep_jobs.push(job);
        } else {
            source_jobs.push(job);
        }
    }

    // Parallel copy of editable source files (NEVER hardlinked!).
    source_jobs
        .par_iter()
        .try_for_each(|(src, dst, is_symlink, meta)| -> Result<()> {
            if *is_symlink {
                copy_symlink(src, dst)?;
                return Ok(());
            }
            if meta.is_file() {
                std::fs::copy(src, dst)?;
                std::fs::set_permissions(dst, meta.permissions())?;
                let ft = metadata_modified_filetime(meta);
                let _ = set_file_mtime_compat(dst, ft);
            }
            Ok(())
        })?;

    // Parallel CAS hardlinking (`0444` / `0555`) for immutable dependencies.
    dep_jobs
        .par_iter()
        .try_for_each(|(src, dst, is_symlink, meta)| -> Result<()> {
            if *is_symlink {
                copy_symlink(src, dst)?;
                return Ok(());
            }
            if meta.is_file() {
                link_file_via_cas(src, dst, meta, cas_dir)?;
            }
            Ok(())
        })?;

    Ok(())
}

fn link_file_via_cas(src: &Path, dst: &Path, src_meta: &Metadata, cas_dir: &Path) -> Result<()> {
    let digest = blake3_file_hex(src)?;
    let (shard, rest) = digest.split_at(2);
    let shard_dir = cas_dir.join(shard);
    std::fs::create_dir_all(&shard_dir)?;
    let cas_obj = shard_dir.join(rest);

    if !cas_obj.exists() {
        let pid = std::process::id();
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp_path = shard_dir.join(format!(".tmp_{rest}_{pid}_{seq}"));

        let linked_from_readonly = is_readonly_mode(src_meta) && std::fs::hard_link(src, &tmp_path).is_ok();
        if !linked_from_readonly {
            std::fs::copy(src, &tmp_path)?;
            set_readonly_permissions(&tmp_path, is_executable(src_meta))?;
            let ft = metadata_modified_filetime(src_meta);
            let _ = set_file_mtime_compat(&tmp_path, ft);
        }

        if let Err(err) = std::fs::hard_link(&tmp_path, &cas_obj)
            && err.kind() != std::io::ErrorKind::AlreadyExists
        {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(OzError::Io(err));
        }
        let _ = std::fs::remove_file(&tmp_path);
    }

    if std::fs::symlink_metadata(dst).is_ok() {
        let _ = std::fs::remove_file(dst);
    }
    if std::fs::hard_link(&cas_obj, dst).is_err() {
        std::fs::copy(&cas_obj, dst)?;
        set_readonly_permissions(dst, is_executable(src_meta))?;
        let ft = metadata_modified_filetime(src_meta);
        let _ = set_file_mtime_compat(dst, ft);
    }
    Ok(())
}

fn blake3_file_hex(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

fn collect_warm_root_names(req: &SpawnRequest, git_common_dir: &Path) -> HashSet<String> {
    let mut roots = HashSet::new();
    let cfg = OzoneConfig::load_for_repo(&req.repo_root, &git_common_dir.join("ozone"))
        .unwrap_or_default();
    for target in detect_warm_targets(&req.repo_root, &cfg) {
        roots.insert(target.relative_path.trim_end_matches('/').to_string());
    }
    for known in [
        "target",
        "node_modules",
        ".next",
        ".turbo",
        ".vite",
        ".venv",
        ".mypy_cache",
        ".ruff_cache",
        ".pytest_cache",
    ] {
        roots.insert(known.to_string());
    }
    roots
}

fn is_under_warm_root(rel: &Path, warm_roots: &HashSet<String>) -> bool {
    let mut parts = Vec::new();
    for comp in rel.components() {
        if let Component::Normal(s) = comp {
            parts.push(s.to_string_lossy().into_owned());
        }
    }
    if parts.is_empty() {
        return false;
    }
    for i in 1..=parts.len() {
        let prefix = parts[..i].join("/");
        if warm_roots.contains(&prefix) {
            return true;
        }
    }
    false
}

fn is_readonly_mode(meta: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o222 == 0
    }
    #[cfg(not(unix))]
    {
        meta.permissions().readonly()
    }
}

fn is_executable(meta: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

fn set_readonly_permissions(path: &Path, executable: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o555 } else { 0o444 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = executable;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn apply_git_mode(path: &Path, executable: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, executable);
    }
    Ok(())
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
        std::fs::write(dst, target_bytes)?;
    }
    Ok(())
}

fn copy_symlink(src: &Path, dst: &Path) -> Result<()> {
    let target = std::fs::read_link(src)?;
    if std::fs::symlink_metadata(dst).is_ok() {
        let _ = std::fs::remove_file(dst);
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, dst)?;
    }
    #[cfg(windows)]
    {
        if src.is_dir() {
            std::os::windows::fs::symlink_dir(&target, dst)?;
        } else {
            std::os::windows::fs::symlink_file(&target, dst)?;
        }
    }
    Ok(())
}

fn metadata_modified_filetime(meta: &Metadata) -> FileTime {
    meta.modified()
        .map(|st| match st.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => FileTime::from_unix_time(d.as_secs() as i64, d.subsec_nanos()),
            Err(e) => FileTime::from_unix_time(-(e.duration().as_secs() as i64), 0),
        })
        .unwrap_or_else(|_| FileTime::zero())
}

#[allow(deprecated)]
fn set_file_mtime_compat(path: &Path, ft: FileTime) -> std::io::Result<()> {
    filetime::set_file_mtime(path, ft)
}
