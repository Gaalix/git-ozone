// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared Git tree delta calculation, `rayon` parallel materialization, reflink cloning,
//! and content-addressed (`0444`/`0555`) hardlink store helpers.

use crate::SpawnRequest;
use filetime::FileTime;
use gix::bstr::ByteSlice;
use gix::object::tree::EntryMode;
use gix::object::tree::diff::Change;
use oz_core::{
    AttributeResolver, AutoCrlf, GitConfig, GitConfigContext, OzError, OzoneConfig, Result,
    is_lfs_pointer, normalize_lexical, smudge_blob_bytes,
};
use oz_warm::{
    BaselineMetadata, CargoArtifactDisposition, CargoTargetClassifier, EcosystemKind,
    apply_baseline_mtimes, detect_warm_targets,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::Metadata;
use std::ops::ControlFlow;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
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
                        && let Ok(rel) = std::str::from_utf8(location.as_ref())
                    {
                        delta
                            .upserts
                            .push((rel.to_string(), id.detach(), entry_mode));
                    }
                }
                Change::Deletion {
                    location,
                    entry_mode,
                    ..
                } => {
                    if !entry_mode.is_tree()
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

/// Materializes all files/symlinks/gitlink dirs from `tree_id` into `dst_root` in parallel using `rayon`,
/// applying `.gitattributes` EOL/clean-smudge rules (`filter=lfs`, `eol=*`, etc.).
pub fn materialize_tree_from_odb(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
    dst_root: &Path,
) -> Result<()> {
    let git_common_dir = resolve_git_common_dir(repo.git_dir())?;
    let git_cfg = load_git_config_for_cow(repo, &git_common_dir)?;
    let attr_resolver =
        AttributeResolver::for_tree(repo, Some(tree_id), &git_common_dir, &git_cfg)?;

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
    let mut gitlink_dirs = Vec::new();
    for entry in entries {
        if entry.mode.is_tree() {
            continue;
        }
        let Ok(rel) = std::str::from_utf8(entry.filepath.as_ref()) else {
            continue;
        };
        let dst = dst_root.join(rel);
        if entry.mode.is_commit() {
            gitlink_dirs.push(dst);
            continue;
        }
        if let Some(parent) = dst.parent() {
            dirs.insert(parent.to_path_buf());
        }
        files.push((rel.to_string(), dst, entry.oid, entry.mode));
    }

    for dir in dirs {
        std::fs::create_dir_all(dir)?;
    }
    for gdir in gitlink_dirs {
        std::fs::create_dir_all(gdir)?;
    }

    let git_dir = repo.git_dir().to_path_buf();
    files
        .par_iter()
        .map_init(
            || gix::open(&git_dir).map_err(|e| OzError::Git(e.to_string())),
            |thread_repo_res, (rel, dst, oid, mode)| {
                let thread_repo = thread_repo_res
                    .as_ref()
                    .map_err(|e| OzError::Git(e.to_string()))?;
                write_git_entry(
                    thread_repo,
                    rel,
                    dst,
                    *oid,
                    *mode,
                    &attr_resolver,
                    &git_cfg,
                    dst_root,
                    &git_common_dir,
                )
            },
        )
        .collect::<Result<Vec<()>>>()?;

    Ok(())
}

/// Applies `delta` (deletions and upserts) to `dst_root`, handling gitlinks (`160000`) and
/// `.gitattributes` smudge filters / EOL conversion.
pub fn apply_tree_delta_to_dir(
    repo: &gix::Repository,
    delta: &TreeDelta,
    dst_root: &Path,
) -> Result<()> {
    for rel in &delta.deletions {
        let p = dst_root.join(rel);
        if let Ok(meta) = std::fs::symlink_metadata(&p) {
            if meta.file_type().is_dir() {
                let _ = std::fs::remove_dir_all(&p);
            } else {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    let git_common_dir = resolve_git_common_dir(repo.git_dir())?;
    let git_cfg = load_git_config_for_cow(repo, &git_common_dir)?;
    let attr_resolver =
        build_delta_attribute_resolver(repo, &delta.upserts, dst_root, &git_common_dir, &git_cfg)?;

    let mut dirs = HashSet::new();
    let mut file_upserts = Vec::new();
    for (rel, oid, mode) in &delta.upserts {
        let dst = dst_root.join(rel);
        if mode.is_commit() {
            dirs.insert(dst);
            continue;
        }
        if let Some(parent) = dst.parent() {
            dirs.insert(parent.to_path_buf());
        }
        file_upserts.push((rel.clone(), dst, *oid, *mode));
    }
    for dir in dirs {
        std::fs::create_dir_all(dir)?;
    }

    let git_dir = repo.git_dir().to_path_buf();
    file_upserts
        .par_iter()
        .map_init(
            || gix::open(&git_dir).map_err(|e| OzError::Git(e.to_string())),
            |thread_repo_res, (rel, dst, oid, mode)| {
                let thread_repo = thread_repo_res
                    .as_ref()
                    .map_err(|e| OzError::Git(e.to_string()))?;
                if let Ok(meta) = std::fs::symlink_metadata(dst) {
                    if meta.file_type().is_dir() {
                        let _ = std::fs::remove_dir_all(dst);
                    } else {
                        let _ = std::fs::remove_file(dst);
                    }
                }
                write_git_entry(
                    thread_repo,
                    rel,
                    dst,
                    *oid,
                    *mode,
                    &attr_resolver,
                    &git_cfg,
                    dst_root,
                    &git_common_dir,
                )
            },
        )
        .collect::<Result<Vec<()>>>()?;

    Ok(())
}

/// Ensures any gitlink directories (`160000`) and attribute-filtered (`filter=*`, `eol=crlf`,
/// `core.autocrlf=true`) entries in `tree_id` are cleanly created/smudged inside `dst_root`,
/// even when files were inherited directly from a raw `oz-warm` baseline copy.
pub(crate) fn reconcile_attributes_and_gitlinks_in_dir(
    repo: &gix::Repository,
    git_common_dir: &Path,
    tree_id: gix::ObjectId,
    dst_root: &Path,
) -> Result<()> {
    let git_cfg = load_git_config_for_cow(repo, git_common_dir)?;
    let attr_resolver = AttributeResolver::for_tree(repo, Some(tree_id), git_common_dir, &git_cfg)?;

    let tree = repo
        .find_tree(tree_id)
        .map_err(|e| OzError::Git(e.to_string()))?;
    let entries = tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|e| OzError::Git(e.to_string()))?;

    let needs_file_checks = attr_resolver.may_have_rules() || git_cfg.autocrlf() == AutoCrlf::True;
    let mut jobs = Vec::new();

    for entry in entries {
        if entry.mode.is_tree() {
            continue;
        }
        let Ok(rel) = std::str::from_utf8(entry.filepath.as_ref()) else {
            continue;
        };
        let dst = dst_root.join(rel);
        if entry.mode.is_commit() {
            std::fs::create_dir_all(&dst)?;
            continue;
        }
        if !needs_file_checks || entry.mode.is_link() {
            continue;
        }
        let resolved = attr_resolver.resolve(rel);
        if resolved.filter_driver().is_some()
            || resolved.eol().is_some()
            || resolved.is_text_set()
            || resolved.is_text_auto()
            || git_cfg.autocrlf() == AutoCrlf::True
        {
            jobs.push((rel.to_string(), dst, entry.oid, entry.mode, resolved));
        }
    }

    for (rel, dst, oid, mode, resolved) in jobs {
        let blob = repo
            .find_blob(oid)
            .map_err(|e| OzError::Git(e.to_string()))?;
        let needs_rewrite = resolved.filter_driver().is_some()
            || is_lfs_pointer(&blob.data)
            || match oz_core::smudge_eol_bytes(&blob.data, &resolved, &git_cfg) {
                std::borrow::Cow::Owned(_) => true,
                std::borrow::Cow::Borrowed(_) => false,
            };
        if !needs_rewrite {
            continue;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::symlink_metadata(&dst).is_ok() {
            let _ = std::fs::remove_file(&dst);
        }
        let smudged = smudge_blob_bytes(
            &rel,
            &blob.data,
            &resolved,
            &git_cfg,
            dst_root,
            git_common_dir,
        )?;
        std::fs::write(&dst, &smudged)?;
        apply_git_mode(&dst, mode.is_executable())?;
    }

    Ok(())
}

fn load_git_config_for_cow(repo: &gix::Repository, git_common_dir: &Path) -> Result<GitConfig> {
    let branch = repo
        .head_name()
        .ok()
        .flatten()
        .map(|n| n.shorten().to_string());
    let ctx = GitConfigContext {
        git_dir: Some(repo.git_dir().to_path_buf()),
        git_common_dir: Some(git_common_dir.to_path_buf()),
        workdir_path: repo.workdir().map(Path::to_path_buf),
        branch,
        ..Default::default()
    };
    GitConfig::load_with_context(&ctx)
}

fn build_delta_attribute_resolver(
    repo: &gix::Repository,
    upserts: &[(String, gix::ObjectId, EntryMode)],
    dst_root: &Path,
    git_common_dir: &Path,
    git_cfg: &GitConfig,
) -> Result<AttributeResolver> {
    let mut per_dir: HashMap<String, String> = HashMap::new();
    if let Ok(head_tree) = repo.head_tree_id()
        && let Ok(tree) = repo.find_tree(head_tree)
        && let Ok(entries) = tree.traverse().breadthfirst.files()
    {
        for entry in entries {
            if !entry.mode.is_blob() {
                continue;
            }
            let rel = entry.filepath.to_str_lossy();
            if rel == ".gitattributes" || rel.ends_with("/.gitattributes") {
                let dir = rel
                    .strip_suffix("/.gitattributes")
                    .unwrap_or("")
                    .to_string();
                if let Ok(blob) = repo.find_blob(entry.oid) {
                    per_dir.insert(dir, String::from_utf8_lossy(&blob.data).into_owned());
                }
            }
        }
    }

    // Overlay `.gitattributes` files already present in `dst_root`.
    if dst_root.is_dir() {
        for entry in WalkDir::new(dst_root)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            if entry.file_name() == ".git" {
                continue;
            }
            if entry.file_name() == ".gitattributes"
                && entry.file_type().is_file()
                && let Ok(rel) = entry.path().strip_prefix(dst_root)
                && let Ok(raw) = std::fs::read_to_string(entry.path())
            {
                let rel_s = rel.to_string_lossy().replace('\\', "/");
                let dir = rel_s
                    .strip_suffix("/.gitattributes")
                    .unwrap_or("")
                    .to_string();
                per_dir.insert(dir, raw);
            }
        }
    }

    // Highest-priority target-tree `.gitattributes` from this delta's upserts.
    for (rel, oid, mode) in upserts {
        if !mode.is_commit()
            && !mode.is_tree()
            && (rel == ".gitattributes" || rel.ends_with("/.gitattributes"))
        {
            let dir = rel
                .strip_suffix("/.gitattributes")
                .unwrap_or("")
                .to_string();
            let blob = repo
                .find_blob(*oid)
                .map_err(|e| OzError::Git(e.to_string()))?;
            per_dir.insert(dir, String::from_utf8_lossy(&blob.data).into_owned());
        }
    }

    AttributeResolver::from_preloaded_files(per_dir, git_common_dir, git_cfg)
}

#[allow(clippy::too_many_arguments)]
fn write_git_entry(
    repo: &gix::Repository,
    rel_path: &str,
    dst: &Path,
    oid: gix::ObjectId,
    mode: EntryMode,
    attrs: &AttributeResolver,
    cfg: &GitConfig,
    workdir: &Path,
    git_common_dir: &Path,
) -> Result<()> {
    if mode.is_commit() {
        std::fs::create_dir_all(dst)?;
        return Ok(());
    }
    let blob = repo
        .find_blob(oid)
        .map_err(|e| OzError::Git(e.to_string()))?;
    if mode.is_link() {
        write_symlink_bytes(dst, &blob.data)?;
        return Ok(());
    }
    let resolved = attrs.resolve(rel_path);
    let smudged = smudge_blob_bytes(
        rel_path,
        &blob.data,
        &resolved,
        cfg,
        workdir,
        git_common_dir,
    )?;
    std::fs::write(dst, &smudged)?;
    apply_git_mode(dst, mode.is_executable())?;
    Ok(())
}

/// Clones `src_root` into `dst_root` using `reflink_copy::reflink` in parallel.
pub fn clone_dir_with_reflink(src_root: &Path, dst_root: &Path) -> Result<()> {
    std::fs::create_dir_all(dst_root)?;
    let mut created_dirs = HashSet::new();
    created_dirs.insert(dst_root.to_path_buf());
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
            if created_dirs.insert(dst.clone()) {
                std::fs::create_dir_all(&dst)?;
            }
            continue;
        }
        if let Some(parent) = dst.parent()
            && !created_dirs.contains(parent)
        {
            std::fs::create_dir_all(parent)?;
            created_dirs.insert(parent.to_path_buf());
        }
        let meta = std::fs::symlink_metadata(entry.path())?;
        jobs.push((
            entry.path().to_path_buf(),
            dst,
            entry.path_is_symlink(),
            meta,
        ));
    }

    jobs.par_iter()
        .try_for_each(|(src, dst, is_symlink, meta)| -> Result<()> {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedCasDigest {
    size: u64,
    mtime_secs: i64,
    mtime_nsecs: u32,
    digest: String,
}

fn cas_digest_cache_path(cas_dir: &Path, baseline_dir: &Path) -> Option<PathBuf> {
    let baseline_id = baseline_dir.parent()?.file_name()?.to_str()?;
    if baseline_id.is_empty() || baseline_id.contains('/') || baseline_id.contains('\\') {
        return None;
    }
    Some(cas_dir.join(format!(".digests_{baseline_id}.json")))
}

fn load_cas_digest_cache(path: &Path) -> HashMap<String, CachedCasDigest> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_cas_digest_cache(path: &Path, map: &HashMap<String, CachedCasDigest>) {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let Some(parent) = path.parent() else {
        return;
    };
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(".tmp_digests_{}_{}", std::process::id(), seq));
    if let Ok(json) = serde_json::to_vec(map)
        && std::fs::write(&tmp, json).is_ok()
        && std::fs::rename(&tmp, path).is_err()
    {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Restores baseline tracked-file `mtime`s on paths inside `target_dir` that are unchanged
/// between `baseline_root`'s commit tree and `candidate_tree_id`, while leaving modified/added
/// paths at their current (`NOW`) timestamps so Cargo/build tools only recompile changed units.
pub fn restore_baseline_mtimes_for_tree(
    gix_repo: &gix::Repository,
    baseline_root: &Path,
    candidate_tree_id: gix::ObjectId,
    target_dir: &Path,
) -> Result<()> {
    let Some(meta_path) = baseline_root.parent().map(|p| p.join("metadata.json")) else {
        return Ok(());
    };
    if !meta_path.is_file() {
        return Ok(());
    }
    let Ok(raw) = std::fs::read_to_string(&meta_path) else {
        return Ok(());
    };
    let Ok(baseline_meta) = serde_json::from_str::<BaselineMetadata>(&raw) else {
        return Ok(());
    };
    if baseline_meta.file_mtimes_secs.is_empty() {
        return Ok(());
    }
    let Some(base_tree_id) = resolve_baseline_tree_id(gix_repo, baseline_root) else {
        return Ok(());
    };
    let delta = compute_tree_delta(gix_repo, base_tree_id, candidate_tree_id)?;
    let changed: HashSet<String> = delta.upserts.into_iter().map(|(rel, _, _)| rel).collect();
    let _ = apply_baseline_mtimes(target_dir, &baseline_meta, &changed)?;
    Ok(())
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

    let (warm_roots, cargo_warm_roots) = collect_warm_root_names(req, git_common_dir);

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

    let cargo_classifiers =
        build_cargo_target_classifiers(&req.repo_root, baseline_dir, &cargo_warm_roots);

    let mut created_dirs = HashSet::new();
    created_dirs.insert(req.target_path.clone());
    let mut dep_jobs = Vec::new();
    let mut source_jobs = Vec::new();

    if tracked_from_odb {
        // Editable tracked files were already freshly written from ODB with distinct inodes;
        // walk ONLY the warm dependency root directories inside `baseline_dir` instead of
        // re-traversing every tracked source file!
        let mut sorted_roots: Vec<&String> = warm_roots.iter().collect();
        sorted_roots.sort();
        for root_name in sorted_roots {
            let warm_src = baseline_dir.join(root_name);
            if !warm_src.exists() {
                continue;
            }
            for entry in WalkDir::new(&warm_src).follow_links(false) {
                let entry = entry.map_err(|e| OzError::Other(e.to_string()))?;
                let rel = entry
                    .path()
                    .strip_prefix(baseline_dir)
                    .map_err(|e| OzError::Other(e.to_string()))?;
                if rel.as_os_str().is_empty() {
                    continue;
                }
                let dst = req.target_path.join(rel);
                if entry.file_type().is_dir() {
                    if created_dirs.insert(dst.clone()) {
                        std::fs::create_dir_all(&dst)?;
                    }
                    continue;
                }
                if !entry.path_is_symlink()
                    && let Some(disp) = classify_cargo_warm_path(rel, &cargo_classifiers)
                {
                    match disp {
                        CargoArtifactDisposition::SkipTransientLock => continue,
                        CargoArtifactDisposition::MutableWritable => {
                            ensure_parent_dir(&dst, &mut created_dirs)?;
                            let meta = std::fs::symlink_metadata(entry.path())?;
                            source_jobs.push((entry.path().to_path_buf(), dst, false, meta));
                            continue;
                        }
                        CargoArtifactDisposition::ImmutableCas => {
                            ensure_parent_dir(&dst, &mut created_dirs)?;
                            let meta = std::fs::symlink_metadata(entry.path())?;
                            dep_jobs.push((
                                rel.to_string_lossy().into_owned(),
                                entry.path().to_path_buf(),
                                dst,
                                false,
                                meta,
                            ));
                            continue;
                        }
                    }
                }
                ensure_parent_dir(&dst, &mut created_dirs)?;
                let meta = std::fs::symlink_metadata(entry.path())?;
                if !entry.path_is_symlink() && is_mutable_warm_metadata_file(rel) {
                    source_jobs.push((entry.path().to_path_buf(), dst, false, meta));
                } else {
                    dep_jobs.push((
                        rel.to_string_lossy().into_owned(),
                        entry.path().to_path_buf(),
                        dst,
                        entry.path_is_symlink(),
                        meta,
                    ));
                }
            }
        }
    } else {
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
            let dst = req.target_path.join(rel);
            if entry.file_type().is_dir() {
                if created_dirs.insert(dst.clone()) {
                    std::fs::create_dir_all(&dst)?;
                }
                continue;
            }
            if is_warm_dep
                && !entry.path_is_symlink()
                && let Some(disp) = classify_cargo_warm_path(rel, &cargo_classifiers)
            {
                match disp {
                    CargoArtifactDisposition::SkipTransientLock => continue,
                    CargoArtifactDisposition::MutableWritable => {
                        ensure_parent_dir(&dst, &mut created_dirs)?;
                        let meta = std::fs::symlink_metadata(entry.path())?;
                        source_jobs.push((entry.path().to_path_buf(), dst, false, meta));
                        continue;
                    }
                    CargoArtifactDisposition::ImmutableCas => {
                        ensure_parent_dir(&dst, &mut created_dirs)?;
                        let meta = std::fs::symlink_metadata(entry.path())?;
                        dep_jobs.push((
                            rel.to_string_lossy().into_owned(),
                            entry.path().to_path_buf(),
                            dst,
                            false,
                            meta,
                        ));
                        continue;
                    }
                }
            }
            ensure_parent_dir(&dst, &mut created_dirs)?;
            let meta = std::fs::symlink_metadata(entry.path())?;
            if is_warm_dep && (entry.path_is_symlink() || !is_mutable_warm_metadata_file(rel)) {
                dep_jobs.push((
                    rel.to_string_lossy().into_owned(),
                    entry.path().to_path_buf(),
                    dst,
                    entry.path_is_symlink(),
                    meta,
                ));
            } else {
                source_jobs.push((
                    entry.path().to_path_buf(),
                    dst,
                    entry.path_is_symlink(),
                    meta,
                ));
            }
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
                set_writable_permissions(dst, meta)?;
                let ft = metadata_modified_filetime(meta);
                let _ = set_file_mtime_compat(dst, ft);
            }
            Ok(())
        })?;

    // Load cached BLAKE3 digests for this immutable baseline so spawns #2..N hardlink
    // directly from `.git/ozone/cas/` without re-reading or re-hashing gigabytes of artifacts.
    let cache_path = cas_digest_cache_path(cas_dir, baseline_dir);
    let existing_digests = cache_path
        .as_deref()
        .map(load_cas_digest_cache)
        .unwrap_or_default();
    let new_digests: Mutex<Vec<(String, CachedCasDigest)>> = Mutex::new(Vec::new());

    // Parallel CAS hardlinking (`0444` / `0555`) for immutable dependencies.
    dep_jobs
        .par_iter()
        .try_for_each(|(rel_key, src, dst, is_symlink, meta)| -> Result<()> {
            if *is_symlink {
                copy_symlink(src, dst)?;
                return Ok(());
            }
            if meta.is_file() {
                let ft = metadata_modified_filetime(meta);
                let cached_hex = existing_digests
                    .get(rel_key)
                    .filter(|c| {
                        c.size == meta.len()
                            && c.mtime_secs == ft.unix_seconds()
                            && c.mtime_nsecs == ft.nanoseconds()
                    })
                    .map(|c| c.digest.as_str());
                let digest = link_file_via_cas(src, dst, meta, cas_dir, cached_hex)?;
                if cached_hex.is_none() && cache_path.is_some() {
                    new_digests
                        .lock()
                        .expect("digest cache lock poisoned")
                        .push((
                            rel_key.clone(),
                            CachedCasDigest {
                                size: meta.len(),
                                mtime_secs: ft.unix_seconds(),
                                mtime_nsecs: ft.nanoseconds(),
                                digest,
                            },
                        ));
                }
            }
            Ok(())
        })?;

    if let Some(ref cpath) = cache_path {
        let computed = new_digests
            .into_inner()
            .expect("digest cache lock poisoned");
        if !computed.is_empty() {
            let mut merged = existing_digests;
            for (k, v) in computed {
                merged.insert(k, v);
            }
            save_cas_digest_cache(cpath, &merged);
        }
    }

    Ok(())
}

fn link_file_via_cas(
    src: &Path,
    dst: &Path,
    src_meta: &Metadata,
    cas_dir: &Path,
    cached_digest: Option<&str>,
) -> Result<String> {
    let digest = match cached_digest {
        Some(hex) if hex.len() > 2 => hex.to_string(),
        _ => blake3_file_hex(src)?,
    };
    let (shard, rest) = digest.split_at(2);
    let shard_dir = cas_dir.join(shard);
    let cas_obj = shard_dir.join(rest);

    if !cas_obj.exists() {
        std::fs::create_dir_all(&shard_dir)?;
        let pid = std::process::id();
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp_path = shard_dir.join(format!(".tmp_{rest}_{pid}_{seq}"));

        let linked_from_readonly =
            is_readonly_mode(src_meta) && std::fs::hard_link(src, &tmp_path).is_ok();
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
    Ok(digest)
}

fn blake3_file_hex(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

fn ensure_parent_dir(dst: &Path, created_dirs: &mut HashSet<PathBuf>) -> Result<()> {
    if let Some(parent) = dst.parent()
        && !created_dirs.contains(parent)
    {
        std::fs::create_dir_all(parent)?;
        created_dirs.insert(parent.to_path_buf());
    }
    Ok(())
}

fn collect_warm_root_names(
    req: &SpawnRequest,
    git_common_dir: &Path,
) -> (HashSet<String>, HashSet<String>) {
    let mut roots = HashSet::new();
    let mut cargo_roots = HashSet::new();
    let cfg = OzoneConfig::load_for_repo(&req.repo_root, &git_common_dir.join("ozone"))
        .unwrap_or_default();
    for target in detect_warm_targets(&req.repo_root, &cfg) {
        let rel = target.relative_path.trim_end_matches('/').to_string();
        if matches!(target.ecosystem, EcosystemKind::RustCargo) {
            cargo_roots.insert(rel.clone());
        }
        roots.insert(rel);
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
    (roots, cargo_roots)
}

fn build_cargo_target_classifiers(
    repo_root: &Path,
    baseline_dir: &Path,
    cargo_warm_roots: &HashSet<String>,
) -> Vec<(String, CargoTargetClassifier)> {
    let mut candidates = cargo_warm_roots.clone();
    if baseline_dir.join("Cargo.toml").is_file()
        || repo_root.join("Cargo.toml").is_file()
        || baseline_dir.join("target/.rustc_info.json").is_file()
        || baseline_dir.join("target/debug/.fingerprint").is_dir()
        || baseline_dir.join("target/debug/build").is_dir()
    {
        candidates.insert("target".to_string());
    }

    let manifest_roots = [baseline_dir, repo_root];
    let mut classifiers = Vec::new();
    let mut sorted: Vec<String> = candidates.into_iter().collect();
    sorted.sort();
    for rel_root in sorted {
        let target_dir = baseline_dir.join(&rel_root);
        if target_dir.is_dir() {
            let cls =
                CargoTargetClassifier::for_target_dir(&manifest_roots, &target_dir, &rel_root);
            classifiers.push((rel_root, cls));
        }
    }
    // Longest prefix first so nested custom `target-dir` paths match before shorter prefixes.
    classifiers.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    classifiers
}

fn classify_cargo_warm_path(
    rel: &Path,
    classifiers: &[(String, CargoTargetClassifier)],
) -> Option<CargoArtifactDisposition> {
    for (root_rel, cls) in classifiers {
        if let Ok(rel_in_target) = rel.strip_prefix(root_rel)
            && !rel_in_target.as_os_str().is_empty()
        {
            return Some(cls.classify(rel_in_target));
        }
    }
    None
}

/// Returns `true` for package-manager / build-tool metadata and shim files inside warm cache
/// directories (such as `node_modules/.modules.yaml`, `.pnpm-workspace-state-v1.json`,
/// `node_modules/.bin/*`, `node_modules/.cache/*`, `target/.rustc_info.json`,
/// `target/**/.fingerprint/**`, `target/**/incremental/**`, `target/**/*.d`, etc.) that MUST
/// be copied writable (unique inode per workspace) rather than read-only (`0444`) CAS hardlinked.
#[must_use]
pub fn is_mutable_warm_metadata_file(rel: &Path) -> bool {
    let rel_posix = rel.to_string_lossy().replace('\\', "/");
    let file_name = rel.file_name().and_then(|s| s.to_str()).unwrap_or("");

    if matches!(
        file_name,
        ".modules.yaml"
            | ".package-lock.json"
            | ".yarn-state.yml"
            | ".yarn-integrity"
            | ".pnpm-debug.log"
            | "pyvenv.cfg"
            | ".rustc_info.json"
            | ".rustdoc_fingerprint.json"
            | "CACHEDIR.TAG"
    ) {
        return true;
    }
    if file_name.starts_with(".pnpm-workspace-state") && file_name.ends_with(".json") {
        return true;
    }
    if file_name.ends_with(".tsbuildinfo") || file_name.ends_with(".pth") {
        return true;
    }
    if rel_posix.ends_with(".pnpm/lock.yaml")
        || rel_posix.ends_with(".vite/deps/_metadata.json")
        || rel_posix.contains("node_modules/.bin/")
        || rel_posix.contains("node_modules/.cache/")
        || rel_posix.contains(".venv/bin/")
        || rel_posix.contains("/.fingerprint/")
        || rel_posix.starts_with(".fingerprint/")
        || rel_posix.contains("/incremental/")
        || rel_posix.starts_with("incremental/")
        || rel_posix.starts_with("target/doc/")
        || rel_posix.starts_with("target/tmp/")
    {
        return true;
    }
    if file_name.ends_with(".d")
        && (rel_posix.starts_with("target/")
            || rel_posix.contains("/deps/")
            || rel_posix.contains("/build/")
            || rel_posix.contains("/examples/"))
    {
        return true;
    }
    if rel_posix.contains("/build/")
        && (matches!(
            file_name,
            "invoked.timestamp" | "output" | "root-output" | "stderr" | "stdout"
        ) || rel_posix.contains("/fingerprint/")
            || rel_posix.contains("/run/")
            || rel_posix.contains("/out/"))
    {
        return true;
    }
    false
}

fn set_writable_permissions(path: &Path, src_meta: &Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = src_meta.permissions().mode() | 0o200;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let mut perms = src_meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
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
