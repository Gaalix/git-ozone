// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Quiescence locking checks (`fs4`) and prior-baseline fallback resolution.

use crate::baseline::{capture_baseline, touch_baseline_lru};
use crate::{BaselineMetadata, WarmCacheTarget, detect_warm_targets};
use fs4::{FileExt, TryLockError};
use oz_core::{OzError, OzoneConfig, Result};
use std::collections::{BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

/// A validated baseline snapshot loaded from `.git/ozone/baselines/<commit_oid>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedBaseline {
    pub root_dir: PathBuf,
    pub metadata_path: PathBuf,
    pub metadata: BaselineMetadata,
}

/// Origin of a baseline resolved via [`resolve_or_capture_baseline`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineSource {
    Existing,
    Captured,
    FallbackPrior,
}

/// Result of [`resolve_or_capture_baseline`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineResolution {
    pub baseline: LoadedBaseline,
    pub source: BaselineSource,
}

/// Checks whether all build/package lock indicators in `repo_root` are idle via non-blocking `fs4` locks.
///
/// Returns `Ok(false)` if any lock indicator (e.g., `target/.cargo-lock`) is currently locked.
pub fn is_repo_quiescent(repo_root: &Path, targets: &[WarmCacheTarget]) -> Result<bool> {
    let mut lock_paths = BTreeSet::new();
    for target in targets {
        for indicator in &target.lock_indicators {
            lock_paths.insert(repo_root.join(indicator));
        }
    }

    if repo_root.join("Cargo.toml").is_file() || repo_root.join("target").is_dir() {
        for rel in [
            "target/.cargo-lock",
            "target/debug/.cargo-lock",
            "target/release/.cargo-lock",
        ] {
            lock_paths.insert(repo_root.join(rel));
        }
    }

    for lock_path in lock_paths {
        if !is_lock_file_idle(&lock_path)? {
            return Ok(false);
        }
    }

    Ok(true)
}

fn is_lock_file_idle(lock_path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(lock_path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(OzError::Io(err)),
    }

    let open_res = open_lock_file_for_probe(lock_path);
    let file = match open_res {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(OzError::Io(err)),
    };

    match FileExt::try_lock(&file) {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            Ok(true)
        }
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(err)) => Err(OzError::Io(err)),
    }
}

#[cfg(unix)]
fn open_lock_file_for_probe(lock_path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new().read(true).open(lock_path)
}

#[cfg(not(unix))]
fn open_lock_file_for_probe(lock_path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .or_else(|_| OpenOptions::new().read(true).open(lock_path))
}

pub(crate) fn validate_baseline_id(commit_oid: &str) -> Result<()> {
    if commit_oid.is_empty()
        || commit_oid == "."
        || commit_oid == ".."
        || commit_oid.starts_with('.')
        || commit_oid.contains('/')
        || commit_oid.contains('\\')
        || commit_oid.contains('\0')
    {
        return Err(OzError::InvalidUsage {
            message: format!("invalid baseline commit identifier '{commit_oid}'"),
            recovery: Vec::new(),
        });
    }
    Ok(())
}

/// Loads an existing baseline at `.git/ozone/baselines/<commit_oid>/`, if complete and valid.
pub fn find_baseline(ozone_dir: &Path, commit_oid: &str) -> Result<Option<LoadedBaseline>> {
    validate_baseline_id(commit_oid)?;
    let dir = ozone_dir.join("baselines").join(commit_oid);
    load_baseline_from_dir(&dir)
}

pub(crate) fn load_baseline_from_dir(dir: &Path) -> Result<Option<LoadedBaseline>> {
    let metadata_path = dir.join("metadata.json");
    let root_dir = dir.join("root");
    if !metadata_path.is_file() || !root_dir.is_dir() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&metadata_path)?;
    let metadata: BaselineMetadata = serde_json::from_str(&raw)?;
    Ok(Some(LoadedBaseline {
        root_dir,
        metadata_path,
        metadata,
    }))
}

/// Finds the exact baseline for `commit_oid`, or walks commit ancestry to locate the nearest
/// prior baseline in `.git/ozone/baselines/`, falling back to the newest available baseline.
pub fn find_nearest_prior_baseline(
    repo_root: &Path,
    ozone_dir: &Path,
    commit_oid: Option<&str>,
) -> Result<Option<LoadedBaseline>> {
    if let Some(oid) = commit_oid
        && let Some(exact) = find_baseline(ozone_dir, oid)?
    {
        return Ok(Some(exact));
    }

    let baselines_dir = ozone_dir.join("baselines");
    if !baselines_dir.is_dir() {
        return Ok(None);
    }

    let mut available: HashMap<String, LoadedBaseline> = HashMap::new();
    for entry in std::fs::read_dir(&baselines_dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir()
            && let Some(loaded) = load_baseline_from_dir(&path)?
        {
            available.insert(name.to_string(), loaded);
        }
    }

    if available.is_empty() {
        return Ok(None);
    }

    if let Ok(repo) = gix::open(repo_root) {
        let start_id = commit_oid
            .and_then(|hex| gix::ObjectId::from_hex(hex.as_bytes()).ok())
            .or_else(|| repo.head_id().ok().map(|id| id.detach()));

        if let Some(start) = start_id
            && let Ok(walk) = repo.rev_walk([start]).all()
        {
            for item in walk.flatten() {
                let hex = item.id.to_hex().to_string();
                if let Some(found) = available.remove(&hex) {
                    return Ok(Some(found));
                }
            }
        }
    }

    // Fallback when commit graph is unavailable or disconnected: newest created baseline.
    Ok(available
        .into_values()
        .max_by_key(|b| (b.metadata.created_at, b.metadata.commit_oid.clone())))
}

/// Resolves an existing baseline for `commit_oid`, captures one if the repository is quiescent,
/// or falls back to the nearest prior baseline when a build lock is held.
pub fn resolve_or_capture_baseline(
    repo_root: &Path,
    ozone_dir: &Path,
    commit_oid: &str,
    config: &OzoneConfig,
) -> Result<Option<BaselineResolution>> {
    if let Some(existing) = find_baseline(ozone_dir, commit_oid)? {
        touch_baseline_lru(ozone_dir, commit_oid)?;
        return Ok(Some(BaselineResolution {
            baseline: existing,
            source: BaselineSource::Existing,
        }));
    }

    let targets = detect_warm_targets(repo_root, config);
    if is_repo_quiescent(repo_root, &targets)? {
        let _meta = capture_baseline(repo_root, ozone_dir, commit_oid, config)?;
        if let Some(loaded) = find_baseline(ozone_dir, commit_oid)? {
            touch_baseline_lru(ozone_dir, commit_oid)?;
            return Ok(Some(BaselineResolution {
                baseline: loaded,
                source: BaselineSource::Captured,
            }));
        }
    }

    if let Some(prior) = find_nearest_prior_baseline(repo_root, ozone_dir, Some(commit_oid))? {
        touch_baseline_lru(ozone_dir, &prior.metadata.commit_oid)?;
        return Ok(Some(BaselineResolution {
            baseline: prior,
            source: BaselineSource::FallbackPrior,
        }));
    }

    Ok(None)
}
