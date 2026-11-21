// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Deterministic baseline `mtime` preservation for warm build cache hits.

use crate::BaselineMetadata;
use filetime::FileTime;
use gix::object::tree::diff::Change;
use oz_core::{OzError, Result};
use rayon::prelude::*;
use std::collections::HashSet;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use std::time::{Duration, UNIX_EPOCH};

/// Summary of [`apply_baseline_mtimes`] execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MtimeApplyStats {
    pub restored_files: usize,
    pub kept_modified_files: usize,
    pub missing_files: usize,
}

/// Restores deterministic baseline `mtime` (`<= cached artifact mtime`) on all untouched files
/// in `workspace_root`, while leaving `modified_or_added_paths` untouched at `NOW`.
pub fn apply_baseline_mtimes(
    workspace_root: &Path,
    baseline: &BaselineMetadata,
    modified_or_added_paths: &HashSet<String>,
) -> Result<MtimeApplyStats> {
    let entries: Vec<(&String, &i64)> = baseline.file_mtimes_secs.iter().collect();
    let restored = AtomicUsize::new(0);
    let kept = AtomicUsize::new(0);
    let missing = AtomicUsize::new(0);

    entries
        .par_iter()
        .try_for_each(|(rel_path, mtime_secs)| -> Result<()> {
            if modified_or_added_paths.contains(*rel_path) {
                kept.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }

            let target_path = workspace_root.join(rel_path.as_str());
            let meta = match std::fs::symlink_metadata(&target_path) {
                Ok(m) => m,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    missing.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Err(err) => return Err(OzError::Io(err)),
            };

            let nanos = baseline
                .file_mtimes_nanos
                .get(*rel_path)
                .copied()
                .unwrap_or(0);
            let ft = FileTime::from_unix_time(**mtime_secs, nanos);
            if meta.file_type().is_symlink() {
                let _ = set_symlink_times_compat(&target_path, ft);
            } else if meta.is_file() {
                set_file_mtime_compat(&target_path, ft)?;
            }
            restored.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })?;

    Ok(MtimeApplyStats {
        restored_files: restored.load(Ordering::Relaxed),
        kept_modified_files: kept.load(Ordering::Relaxed),
        missing_files: missing.load(Ordering::Relaxed),
    })
}

pub(crate) fn metadata_modified_secs_nanos(meta: &std::fs::Metadata) -> (i64, u32) {
    let ft = metadata_modified_filetime(meta);
    (ft.unix_seconds(), ft.nanoseconds())
}

pub(crate) fn system_time_to_filetime(st: std::time::SystemTime) -> FileTime {
    match st.duration_since(UNIX_EPOCH) {
        Ok(d) => FileTime::from_unix_time(d.as_secs() as i64, d.subsec_nanos()),
        Err(err) => {
            let d = err.duration();
            FileTime::from_unix_time(-(d.as_secs() as i64), 0)
        }
    }
}

pub(crate) fn metadata_modified_filetime(meta: &std::fs::Metadata) -> FileTime {
    meta.modified()
        .map(system_time_to_filetime)
        .unwrap_or_else(|_| FileTime::from_system_time(UNIX_EPOCH + Duration::from_secs(0)))
}

#[allow(deprecated)]
pub(crate) fn set_file_mtime_compat(path: &Path, ft: FileTime) -> std::io::Result<()> {
    filetime::set_file_mtime(path, ft)
}

#[allow(deprecated)]
pub(crate) fn set_symlink_times_compat(path: &Path, ft: FileTime) -> std::io::Result<()> {
    filetime::set_symlink_file_times(path, ft, ft)
}

/// Computes the set of relative file paths added or modified between `base_commit_oid` and
/// `target_commit_oid` using `gix` in-memory tree diffing.
pub fn compute_changed_paths_between_commits(
    repo_root: &Path,
    base_commit_oid: &str,
    target_commit_oid: &str,
) -> Result<HashSet<String>> {
    if base_commit_oid == target_commit_oid {
        return Ok(HashSet::new());
    }

    let repo = gix::open(repo_root).map_err(|e| OzError::Git(e.to_string()))?;
    let base_id = gix::ObjectId::from_hex(base_commit_oid.as_bytes())
        .map_err(|e| OzError::Git(e.to_string()))?;
    let target_id = gix::ObjectId::from_hex(target_commit_oid.as_bytes())
        .map_err(|e| OzError::Git(e.to_string()))?;

    let base_tree = repo
        .find_commit(base_id)
        .map_err(|e| OzError::Git(e.to_string()))?
        .tree()
        .map_err(|e| OzError::Git(e.to_string()))?;
    let target_tree = repo
        .find_commit(target_id)
        .map_err(|e| OzError::Git(e.to_string()))?
        .tree()
        .map_err(|e| OzError::Git(e.to_string()))?;

    let mut changed = HashSet::new();
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
                    ..
                }
                | Change::Modification {
                    location,
                    entry_mode,
                    ..
                } => {
                    if !entry_mode.is_tree()
                        && let Ok(s) = std::str::from_utf8(location.as_ref())
                    {
                        changed.insert(s.to_string());
                    }
                }
                Change::Rewrite {
                    location,
                    entry_mode,
                    ..
                } => {
                    if !entry_mode.is_tree()
                        && let Ok(s) = std::str::from_utf8(location.as_ref())
                    {
                        changed.insert(s.to_string());
                    }
                }
                Change::Deletion { .. } => {}
            }
            Ok::<_, gix::Error>(ControlFlow::Continue(()))
        })
        .map_err(|e| OzError::Git(e.to_string()))?;

    Ok(changed)
}
