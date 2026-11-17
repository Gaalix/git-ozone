// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Quiescent baseline capture into `.git/ozone/baselines/<commit_oid>/` and LRU disk-cap GC.

use crate::mtime::{
    metadata_modified_filetime, metadata_modified_unix_secs, set_file_mtime_compat,
    set_symlink_times_compat,
};
use crate::quiescent::{is_repo_quiescent, validate_baseline_id};
use crate::trampoline::{is_venv_bin_script, rewrite_python_trampoline_bytes};
use crate::{BaselineMetadata, EcosystemKind, WarmCacheTarget, detect_warm_targets};
use chrono::{DateTime, Utc};
use filetime::FileTime;
use oz_core::{OzError, OzoneConfig, Result};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::fs::Metadata;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use walkdir::WalkDir;

/// Summary returned by [`gc_baselines`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GcReport {
    pub removed_baselines: Vec<String>,
    pub reclaimed_bytes: u64,
    pub remaining_bytes: u64,
    pub remaining_count: usize,
}

/// Captures a quiescent warm baseline for `commit_oid` into `.git/ozone/baselines/<commit_oid>/`.
///
/// Captures tracked baseline files (never hardlinked!), detected warm caches (excluding
/// `.cargo-lock` and transient locks), rewrites Python `.venv/bin/*` hardcoded shebangs
/// to portable POSIX polyglot trampolines, and records baseline tracked-file `mtime`s.
pub fn capture_baseline(
    repo_root: &Path,
    ozone_dir: &Path,
    commit_oid: &str,
    config: &OzoneConfig,
) -> Result<BaselineMetadata> {
    validate_baseline_id(commit_oid)?;

    let targets = detect_warm_targets(repo_root, config);
    if !is_repo_quiescent(repo_root, &targets)? {
        return Err(OzError::CheckFailed {
            message: format!(
                "repository at {} is not quiescent (build lock active)",
                repo_root.display()
            ),
            recovery: vec![
                "Wait for active build to finish or fall back to nearest prior baseline."
                    .to_string(),
            ],
        });
    }

    let baselines_dir = ozone_dir.join("baselines");
    std::fs::create_dir_all(&baselines_dir)?;
    let final_dir = baselines_dir.join(commit_oid);
    let final_meta_path = final_dir.join("metadata.json");
    if final_meta_path.is_file() && final_dir.join("root").is_dir() {
        let raw = std::fs::read_to_string(&final_meta_path)?;
        return Ok(serde_json::from_str(&raw)?);
    }

    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(&baselines_dir)?;
    let staging_root = staging.path().join("root");
    std::fs::create_dir_all(&staging_root)?;

    let mut file_mtimes_secs = BTreeMap::new();
    let mut total_bytes = 0u64;

    total_bytes += capture_tracked_files(
        repo_root,
        commit_oid,
        &targets,
        &staging_root,
        &mut file_mtimes_secs,
    )?;

    for target in &targets {
        total_bytes += capture_warm_target(repo_root, &staging_root, target)?;
    }

    let created_at = Utc::now();
    let metadata = BaselineMetadata {
        commit_oid: commit_oid.to_string(),
        created_at,
        targets,
        total_bytes,
        file_mtimes_secs,
    };

    let staging_meta_path = staging.path().join("metadata.json");
    std::fs::write(&staging_meta_path, serde_json::to_string_pretty(&metadata)?)?;
    let meta_ft = FileTime::from_unix_time(
        created_at.timestamp(),
        created_at.timestamp_subsec_nanos(),
    );
    let _ = set_file_mtime_compat(&staging_meta_path, meta_ft);

    let staging_path = staging.keep();
    match std::fs::rename(&staging_path, &final_dir) {
        Ok(()) => Ok(metadata),
        Err(err) => {
            let _ = std::fs::remove_dir_all(&staging_path);
            if final_meta_path.is_file() {
                let raw = std::fs::read_to_string(&final_meta_path)?;
                return Ok(serde_json::from_str(&raw)?);
            }
            Err(OzError::Io(err))
        }
    }
}

fn capture_tracked_files(
    repo_root: &Path,
    commit_oid: &str,
    targets: &[WarmCacheTarget],
    staging_root: &Path,
    file_mtimes_secs: &mut BTreeMap<String, i64>,
) -> Result<u64> {
    if let Ok(repo) = gix::open(repo_root)
        && let Ok(oid) = gix::ObjectId::from_hex(commit_oid.as_bytes())
        && let Ok(commit) = repo.find_commit(oid)
    {
        return capture_git_commit_tree(repo_root, &repo, &commit, staging_root, file_mtimes_secs);
    }

    capture_non_git_source_files(repo_root, targets, staging_root, file_mtimes_secs)
}

fn capture_git_commit_tree(
    repo_root: &Path,
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
    staging_root: &Path,
    file_mtimes_secs: &mut BTreeMap<String, i64>,
) -> Result<u64> {
    let commit_secs = commit
        .time()
        .map(|t| t.seconds)
        .unwrap_or_else(|_| Utc::now().timestamp());
    let tree = commit.tree().map_err(|e| OzError::Git(e.to_string()))?;
    let entries = tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|e| OzError::Git(e.to_string()))?;

    let mut total_bytes = 0u64;
    for entry in entries {
        if entry.mode.is_tree() || entry.mode.is_commit() {
            continue;
        }
        let Ok(rel_str) = std::str::from_utf8(entry.filepath.as_ref()) else {
            continue;
        };
        let rel_path = Path::new(rel_str);
        let work_file = repo_root.join(rel_path);
        let mtime_secs = match std::fs::symlink_metadata(&work_file) {
            Ok(meta) => metadata_modified_unix_secs(&meta),
            Err(_) => commit_secs,
        };
        file_mtimes_secs.insert(rel_str.to_string(), mtime_secs);

        let dst_path = staging_root.join(rel_path);
        if let Some(parent) = dst_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let blob = repo
            .find_blob(entry.oid)
            .map_err(|e| OzError::Git(e.to_string()))?;
        let data = &blob.data;

        if entry.mode.is_link() {
            write_symlink_bytes(&dst_path, data)?;
            let _ = set_symlink_mtime_secs(&dst_path, mtime_secs);
            total_bytes += data.len() as u64;
            continue;
        }

        // Editable source files are NEVER hardlinked. Try reflink only when clean on disk,
        // otherwise write directly from the Git blob.
        let mut materialized = false;
        if let Ok(meta) = std::fs::symlink_metadata(&work_file)
            && meta.is_file()
            && meta.len() == data.len() as u64
            && reflink_copy::reflink(&work_file, &dst_path).is_ok()
        {
            materialized = true;
        }
        if !materialized {
            std::fs::write(&dst_path, data)?;
        }

        apply_git_mode(&dst_path, entry.mode.is_executable())?;
        set_file_mtime_compat(&dst_path, FileTime::from_unix_time(mtime_secs, 0))?;
        total_bytes += data.len() as u64;
    }

    Ok(total_bytes)
}

fn capture_non_git_source_files(
    repo_root: &Path,
    targets: &[WarmCacheTarget],
    staging_root: &Path,
    file_mtimes_secs: &mut BTreeMap<String, i64>,
) -> Result<u64> {
    let warm_roots: HashSet<String> = targets
        .iter()
        .map(|t| t.relative_path.trim_end_matches('/').to_string())
        .collect();

    let mut total_bytes = 0u64;
    let walker = WalkDir::new(repo_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let Ok(rel) = e.path().strip_prefix(repo_root) else {
                return false;
            };
            let rel_posix = path_to_posix(rel);
            if rel_posix == ".git" || rel_posix == ".ozone" {
                return false;
            }
            !warm_roots.iter().any(|w| rel_posix == *w)
        });

    for entry in walker {
        let entry = entry.map_err(|e| OzError::Other(e.to_string()))?;
        if entry.depth() == 0 {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(repo_root)
            .map_err(|e| OzError::Other(e.to_string()))?;
        let dst = staging_root.join(rel);

        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dst)?;
            continue;
        }

        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let meta = std::fs::symlink_metadata(entry.path())?;
        let mtime_secs = metadata_modified_unix_secs(&meta);
        file_mtimes_secs.insert(path_to_posix(rel), mtime_secs);

        if entry.path_is_symlink() {
            copy_symlink(entry.path(), &dst)?;
            let _ = set_symlink_mtime_secs(&dst, mtime_secs);
            total_bytes += meta.len();
            continue;
        }

        if meta.is_file() {
            // NEVER hardlink editable source files!
            if reflink_copy::reflink(entry.path(), &dst).is_err() {
                std::fs::copy(entry.path(), &dst)?;
            }
            std::fs::set_permissions(&dst, meta.permissions())?;
            set_file_mtime_compat(&dst, FileTime::from_unix_time(mtime_secs, 0))?;
            total_bytes += meta.len();
        }
    }

    Ok(total_bytes)
}

#[derive(Debug)]
struct WarmFileJob {
    src: PathBuf,
    dst: PathBuf,
    rel_in_target: PathBuf,
    is_symlink: bool,
    meta: Metadata,
}

fn capture_warm_target(
    repo_root: &Path,
    staging_root: &Path,
    target: &WarmCacheTarget,
) -> Result<u64> {
    let src_root = repo_root.join(&target.relative_path);
    if !src_root.exists() {
        return Ok(0);
    }

    let dst_root = staging_root.join(&target.relative_path);
    if src_root.is_file() {
        if let Some(parent) = dst_root.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let meta = std::fs::symlink_metadata(&src_root)?;
        let job = WarmFileJob {
            src: src_root,
            dst: dst_root,
            rel_in_target: PathBuf::from(&target.relative_path),
            is_symlink: meta.file_type().is_symlink(),
            meta,
        };
        return process_warm_file_job(&job, &target.ecosystem);
    }

    std::fs::create_dir_all(&dst_root)?;
    let mut jobs = Vec::new();

    for entry in WalkDir::new(&src_root).follow_links(false) {
        let entry = entry.map_err(|e| OzError::Other(e.to_string()))?;
        let rel = entry
            .path()
            .strip_prefix(&src_root)
            .map_err(|e| OzError::Other(e.to_string()))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        if is_transient_lock_file(rel, entry.file_type().is_dir()) {
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
        jobs.push(WarmFileJob {
            src: entry.path().to_path_buf(),
            dst,
            rel_in_target: rel.to_path_buf(),
            is_symlink: entry.path_is_symlink(),
            meta,
        });
    }

    let total = AtomicU64::new(0);
    jobs.par_iter()
        .try_for_each(|job| -> Result<()> {
            let bytes = process_warm_file_job(job, &target.ecosystem)?;
            total.fetch_add(bytes, Ordering::Relaxed);
            Ok(())
        })?;

    Ok(total.load(Ordering::Relaxed))
}

fn process_warm_file_job(job: &WarmFileJob, ecosystem: &EcosystemKind) -> Result<u64> {
    if job.is_symlink {
        copy_symlink(&job.src, &job.dst)?;
        return Ok(job.meta.len());
    }
    if !job.meta.is_file() {
        return Ok(0);
    }

    let mtime = metadata_modified_filetime(&job.meta);

    // Python `.venv/bin/*` trampoline rewriting writes an independent file (never hardlinked).
    if matches!(ecosystem, EcosystemKind::Python) && is_venv_bin_script(&job.rel_in_target) {
        let bytes = std::fs::read(&job.src)?;
        if let Some(rewritten) = rewrite_python_trampoline_bytes(&bytes) {
            std::fs::write(&job.dst, &rewritten)?;
            std::fs::set_permissions(&job.dst, job.meta.permissions())?;
            set_file_mtime_compat(&job.dst, mtime)?;
            return Ok(rewritten.len() as u64);
        }
        if reflink_copy::reflink(&job.src, &job.dst).is_err() {
            std::fs::write(&job.dst, &bytes)?;
        }
        std::fs::set_permissions(&job.dst, job.meta.permissions())?;
        set_file_mtime_compat(&job.dst, mtime)?;
        return Ok(bytes.len() as u64);
    }

    if reflink_copy::reflink(&job.src, &job.dst).is_ok() {
        std::fs::set_permissions(&job.dst, job.meta.permissions())?;
        set_file_mtime_compat(&job.dst, mtime)?;
        return Ok(job.meta.len());
    }

    if is_immutable_cache_artifact(&job.rel_in_target, &job.meta, ecosystem)
        && std::fs::hard_link(&job.src, &job.dst).is_ok()
    {
        return Ok(job.meta.len());
    }

    std::fs::copy(&job.src, &job.dst)?;
    std::fs::set_permissions(&job.dst, job.meta.permissions())?;
    set_file_mtime_compat(&job.dst, mtime)?;
    Ok(job.meta.len())
}

fn is_transient_lock_file(rel: &Path, is_dir: bool) -> bool {
    if is_dir {
        return false;
    }
    let Some(name) = rel.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if name == ".cargo-lock" || name == ".oz-lock" || name.ends_with(".flock") {
        return true;
    }
    let in_incremental = rel
        .components()
        .any(|c| matches!(c, Component::Normal(s) if s == "incremental"));
    in_incremental && (name == "lock" || name.ends_with(".lock"))
}

fn is_immutable_cache_artifact(
    rel_in_target: &Path,
    meta: &Metadata,
    ecosystem: &EcosystemKind,
) -> bool {
    if is_readonly_mode(meta) {
        return true;
    }

    let comps: Vec<&str> = rel_in_target
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();

    match ecosystem {
        EcosystemKind::RustCargo => comps
            .iter()
            .any(|c| matches!(*c, "deps" | ".fingerprint" | "build" | "doc")),
        EcosystemKind::NodeJs => true,
        EcosystemKind::Python => !comps.contains(&"bin")
            && rel_in_target.file_name().and_then(|s| s.to_str()) != Some("pyvenv.cfg"),
        EcosystemKind::Custom(_) => false,
    }
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

/// Updates the LRU access timestamp for baseline `commit_oid`.
pub fn touch_baseline_lru(ozone_dir: &Path, commit_oid: &str) -> Result<()> {
    validate_baseline_id(commit_oid)?;
    let dir = ozone_dir.join("baselines").join(commit_oid);
    if !dir.is_dir() {
        return Ok(());
    }
    let now = Utc::now();
    let nanos = i128::from(now.timestamp()) * 1_000_000_000 + i128::from(now.timestamp_subsec_nanos());
    std::fs::write(dir.join(".last_used"), format!("{nanos}\n"))?;
    Ok(())
}

/// Prunes least-recently-used baselines in `.git/ozone/baselines/` until total retained bytes
/// fit within `max_baseline_gb`.
pub fn gc_baselines(ozone_dir: &Path, max_baseline_gb: f64) -> Result<GcReport> {
    let baselines_dir = ozone_dir.join("baselines");
    if !baselines_dir.is_dir() {
        return Ok(GcReport::default());
    }

    let budget_bytes = if !max_baseline_gb.is_finite() || max_baseline_gb <= 0.0 {
        0u64
    } else {
        (max_baseline_gb * 1024.0 * 1024.0 * 1024.0).floor().max(0.0) as u64
    };

    struct BaselineEntry {
        id: String,
        path: PathBuf,
        size_bytes: u64,
        lru_nanos: i128,
    }

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&baselines_dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(id) = file_name.to_str() else {
            continue;
        };
        if id.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let meta_path = path.join("metadata.json");
        let parsed_meta = if meta_path.is_file() {
            std::fs::read_to_string(&meta_path)
                .ok()
                .and_then(|s| serde_json::from_str::<BaselineMetadata>(&s).ok())
        } else {
            None
        };

        let walked_size = directory_logical_bytes(&path.join("root"));
        let size_bytes = if walked_size > 0 {
            walked_size
        } else {
            parsed_meta.as_ref().map_or(0, |m| m.total_bytes)
        };

        let lru_nanos = read_baseline_lru_nanos(&path, &meta_path, parsed_meta.as_ref());
        entries.push(BaselineEntry {
            id: id.to_string(),
            path,
            size_bytes,
            lru_nanos,
        });
    }

    entries.sort_by(|a, b| a.lru_nanos.cmp(&b.lru_nanos).then_with(|| a.id.cmp(&b.id)));

    let mut total_bytes: u64 = entries.iter().map(|e| e.size_bytes).sum();
    let mut removed_baselines = Vec::new();
    let mut reclaimed_bytes = 0u64;
    let mut remaining_count = entries.len();

    for entry in entries {
        if total_bytes <= budget_bytes {
            break;
        }
        let removing_path = baselines_dir.join(format!(".removing-{}", entry.id));
        if removing_path.exists() {
            let _ = std::fs::remove_dir_all(&removing_path);
        }
        if std::fs::rename(&entry.path, &removing_path).is_ok() {
            std::fs::remove_dir_all(&removing_path)?;
        } else {
            std::fs::remove_dir_all(&entry.path)?;
        }
        total_bytes = total_bytes.saturating_sub(entry.size_bytes);
        reclaimed_bytes += entry.size_bytes;
        remaining_count = remaining_count.saturating_sub(1);
        removed_baselines.push(entry.id);
    }

    Ok(GcReport {
        removed_baselines,
        reclaimed_bytes,
        remaining_bytes: total_bytes,
        remaining_count,
    })
}

fn read_baseline_lru_nanos(
    dir: &Path,
    meta_path: &Path,
    parsed_meta: Option<&BaselineMetadata>,
) -> i128 {
    let lru_file = dir.join(".last_used");
    if let Ok(raw) = std::fs::read_to_string(&lru_file)
        && let Ok(val) = raw.trim().parse::<i128>()
    {
        return val;
    }

    let file_mtime_nanos = std::fs::metadata(meta_path)
        .or_else(|_| std::fs::metadata(dir))
        .ok()
        .map(|m| {
            let ft = metadata_modified_filetime(&m);
            i128::from(ft.unix_seconds()) * 1_000_000_000 + i128::from(ft.nanoseconds())
        });

    let json_nanos = parsed_meta.map(|m| datetime_to_nanos(&m.created_at));

    match (json_nanos, file_mtime_nanos) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => 0,
    }
}

fn datetime_to_nanos(dt: &DateTime<Utc>) -> i128 {
    i128::from(dt.timestamp()) * 1_000_000_000 + i128::from(dt.timestamp_subsec_nanos())
}

fn directory_logical_bytes(dir: &Path) -> u64 {
    if !dir.is_dir() {
        return 0;
    }
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn path_to_posix(path: &Path) -> String {
    let mut parts = Vec::new();
    for comp in path.components() {
        if let Component::Normal(s) = comp {
            parts.push(s.to_string_lossy().into_owned());
        }
    }
    parts.join("/")
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

fn set_symlink_mtime_secs(path: &Path, secs: i64) -> Result<()> {
    let ft = FileTime::from_unix_time(secs, 0);
    set_symlink_times_compat(path, ft)?;
    Ok(())
}
