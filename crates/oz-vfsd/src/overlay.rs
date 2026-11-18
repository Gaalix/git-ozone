// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Linux FUSE3 Copy-on-Write overlay engine (pure-Rust `fuser 0.18`).
//!
//! Layers read from `[upper_dir, seed_dir, baseline_dir]` while keeping
//! tombstones (`meta_dir/whiteouts/*.oz_wh`), replaced-directory opacity markers
//! (`meta_dir/opaque/*.oz_opaque`), and seed whiteouts (`meta_dir/seed_whiteouts/*.oz_wh`)
//! outside `upper_dir` so `upper_dir` holds exclusively user-modified/added files.

use crate::protocol::{DirtyPathEntry, MountSpec};
use oz_core::{ChangeKind, OzError, Result};
use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::FileType as StdFileType;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

/// Marker suffix for deleted entries inside `meta_dir/whiteouts/` and `meta_dir/seed_whiteouts/`.
pub const WHITEOUT_SUFFIX: &str = ".oz_wh";
/// Marker suffix for replaced/opaque directories inside `meta_dir/opaque/`.
pub const OPAQUE_SUFFIX: &str = ".oz_opaque";

/// Resolved configuration for a single CoW overlay mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayMountConfig {
    pub workspace: String,
    pub mount_point: PathBuf,
    pub upper_dir: PathBuf,
    pub seed_dir: Option<PathBuf>,
    pub baseline_dir: Option<PathBuf>,
    pub meta_dir: PathBuf,
    pub allow_other: bool,
    pub writeback_cache: bool,
}

impl OverlayMountConfig {
    /// Builds an overlay configuration from an IPC [`MountSpec`], automatically
    /// detecting `<meta_dir>/../seed` when present on disk.
    #[must_use]
    pub fn from_spec(spec: &MountSpec) -> Self {
        Self {
            workspace: spec.workspace.clone(),
            mount_point: spec.mount_point.clone(),
            upper_dir: spec.upper_dir.clone(),
            seed_dir: resolve_seed_dir(spec),
            baseline_dir: spec.baseline_dir.clone().filter(|d| d.is_dir()),
            meta_dir: spec.meta_dir.clone(),
            allow_other: spec.allow_other,
            writeback_cache: spec.writeback_cache,
        }
    }

    /// Directory containing `.oz_wh` user deletion tombstones.
    #[must_use]
    pub fn whiteouts_dir(&self) -> PathBuf {
        self.meta_dir.join("whiteouts")
    }

    /// Directory containing `.oz_opaque` replaced-directory markers.
    #[must_use]
    pub fn opaque_dir(&self) -> PathBuf {
        self.meta_dir.join("opaque")
    }

    /// Directory containing `.oz_wh` entries deleted in `seed_dir` relative to `baseline_dir`.
    #[must_use]
    pub fn seed_whiteouts_dir(&self) -> PathBuf {
        self.meta_dir.join("seed_whiteouts")
    }

    /// Ensures `mount_point`, `upper_dir`, and `meta_dir/{whiteouts,opaque,seed_whiteouts,tmp}` exist.
    pub fn ensure_directories(&self) -> std::io::Result<()> {
        for dir in [
            &self.mount_point,
            &self.upper_dir,
            &self.meta_dir,
            &self.whiteouts_dir(),
            &self.opaque_dir(),
            &self.seed_whiteouts_dir(),
            &self.meta_dir.join("tmp"),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

/// Auto-detects `<meta_dir>/../seed` when present as a directory on disk.
#[must_use]
pub fn resolve_seed_dir(spec: &MountSpec) -> Option<PathBuf> {
    spec.resolved_seed_dir()
}

/// Source layer where a resolved virtual path currently resides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    Upper,
    Seed,
    Baseline,
}

/// Host filesystem target resolved across `[upper, seed, baseline]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEntry {
    pub layer: LayerKind,
    pub host_path: PathBuf,
}

/// Single merged directory entry returned by [`list_merged_dir_entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedDirEntry {
    pub name: OsString,
    pub file_type: StdFileType,
    pub host_path: PathBuf,
    pub layer: LayerKind,
}

pub(crate) fn marker_path(root: &Path, rel: &Path, suffix: &str) -> PathBuf {
    let mut os = root.join(rel).into_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

pub(crate) fn ancestors_inclusive(rel: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = PathBuf::new();
    for comp in rel.components() {
        if let Component::Normal(part) = comp {
            cur.push(part);
            out.push(cur.clone());
        }
    }
    out
}

fn rel_to_slash_string(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(p) => Some(p.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn is_valid_child_name(name: &OsStr) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    let bytes = name.as_encoded_bytes();
    !bytes.contains(&b'/') && !bytes.contains(&b'\0')
}

/// Resolves `rel` across `[upper_dir, seed_dir, baseline_dir]` honoring whiteouts,
/// opaque directory markers, and `seed_whiteouts`.
#[must_use]
pub fn resolve_rel_path(cfg: &OverlayMountConfig, rel: &Path) -> Option<ResolvedEntry> {
    if rel.as_os_str().is_empty() {
        return Some(ResolvedEntry {
            layer: LayerKind::Upper,
            host_path: cfg.upper_dir.clone(),
        });
    }

    let prefixes = ancestors_inclusive(rel);
    if prefixes.is_empty() {
        return None;
    }

    let whiteouts = cfg.whiteouts_dir();
    for pref in &prefixes {
        if marker_path(&whiteouts, pref, WHITEOUT_SUFFIX).exists() {
            return None;
        }
    }

    let upper_cand = cfg.upper_dir.join(rel);
    if std::fs::symlink_metadata(&upper_cand).is_ok() {
        return Some(ResolvedEntry {
            layer: LayerKind::Upper,
            host_path: upper_cand,
        });
    }

    // Check proper ancestors in upper_dir: an opaque marker or a non-dir blocks lower layers.
    let opaque = cfg.opaque_dir();
    for anc in &prefixes[..prefixes.len() - 1] {
        if marker_path(&opaque, anc, OPAQUE_SUFFIX).exists() {
            return None;
        }
        if let Ok(meta) = std::fs::symlink_metadata(cfg.upper_dir.join(anc))
            && !meta.file_type().is_dir()
        {
            return None;
        }
    }

    if let Some(ref seed) = cfg.seed_dir {
        let seed_cand = seed.join(rel);
        if std::fs::symlink_metadata(&seed_cand).is_ok() {
            return Some(ResolvedEntry {
                layer: LayerKind::Seed,
                host_path: seed_cand,
            });
        }
    }

    let seed_whiteouts = cfg.seed_whiteouts_dir();
    for pref in &prefixes {
        if marker_path(&seed_whiteouts, pref, WHITEOUT_SUFFIX).exists() {
            return None;
        }
    }
    if let Some(ref seed) = cfg.seed_dir {
        for anc in &prefixes[..prefixes.len() - 1] {
            if let Ok(meta) = std::fs::symlink_metadata(seed.join(anc))
                && !meta.file_type().is_dir()
            {
                return None;
            }
        }
    }

    if let Some(ref base) = cfg.baseline_dir {
        let base_cand = base.join(rel);
        if std::fs::symlink_metadata(&base_cand).is_ok() {
            return Some(ResolvedEntry {
                layer: LayerKind::Baseline,
                host_path: base_cand,
            });
        }
    }

    None
}

/// Checks whether `rel` exists in `seed_dir` or `baseline_dir` (ignoring `upper_dir` and
/// user whiteouts/opaque markers, while respecting `seed_whiteouts`).
pub(crate) fn resolve_in_lower_layers(
    cfg: &OverlayMountConfig,
    rel: &Path,
) -> Option<(LayerKind, PathBuf, std::fs::Metadata)> {
    if rel.as_os_str().is_empty() {
        return None;
    }
    let prefixes = ancestors_inclusive(rel);
    if prefixes.is_empty() {
        return None;
    }

    if let Some(ref seed) = cfg.seed_dir {
        let seed_cand = seed.join(rel);
        if let Ok(meta) = std::fs::symlink_metadata(&seed_cand) {
            return Some((LayerKind::Seed, seed_cand, meta));
        }
    }

    let seed_whiteouts = cfg.seed_whiteouts_dir();
    for pref in &prefixes {
        if marker_path(&seed_whiteouts, pref, WHITEOUT_SUFFIX).exists() {
            return None;
        }
    }
    if let Some(ref seed) = cfg.seed_dir {
        for anc in &prefixes[..prefixes.len() - 1] {
            if let Ok(meta) = std::fs::symlink_metadata(seed.join(anc))
                && !meta.file_type().is_dir()
            {
                return None;
            }
        }
    }

    if let Some(ref base) = cfg.baseline_dir {
        let base_cand = base.join(rel);
        if let Ok(meta) = std::fs::symlink_metadata(&base_cand) {
            return Some((LayerKind::Baseline, base_cand, meta));
        }
    }

    None
}

pub(crate) fn lower_layers_blocked_at_dir(cfg: &OverlayMountConfig, rel_dir: &Path) -> bool {
    let prefixes = ancestors_inclusive(rel_dir);
    let whiteouts = cfg.whiteouts_dir();
    let opaque = cfg.opaque_dir();
    for pref in &prefixes {
        if marker_path(&whiteouts, pref, WHITEOUT_SUFFIX).exists() {
            return true;
        }
        if marker_path(&opaque, pref, OPAQUE_SUFFIX).exists() {
            return true;
        }
        if let Ok(meta) = std::fs::symlink_metadata(cfg.upper_dir.join(pref))
            && !meta.file_type().is_dir()
        {
            return true;
        }
    }
    false
}

fn baseline_blocked_at_dir(cfg: &OverlayMountConfig, rel_dir: &Path) -> bool {
    let prefixes = ancestors_inclusive(rel_dir);
    let seed_whiteouts = cfg.seed_whiteouts_dir();
    for pref in &prefixes {
        if marker_path(&seed_whiteouts, pref, WHITEOUT_SUFFIX).exists() {
            return true;
        }
        if let Some(ref seed) = cfg.seed_dir
            && let Ok(meta) = std::fs::symlink_metadata(seed.join(pref))
            && !meta.file_type().is_dir()
        {
            return true;
        }
    }
    false
}

/// Reads all `<name>.oz_wh` marker basenames inside `root/<rel_dir>` in a single `read_dir`
/// pass (returning an empty set immediately if the marker directory does not exist), avoiding
/// O(N) per-entry `stat()` syscalls while merging large baseline directories.
fn read_dir_whiteout_names(root: &Path, rel_dir: &Path) -> HashSet<OsString> {
    let mut names = HashSet::new();
    let dir = if rel_dir.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel_dir)
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return names;
    };
    let suffix_bytes = WHITEOUT_SUFFIX.as_bytes();
    for item in rd.flatten() {
        let fname = item.file_name();
        let bytes = fname.as_encoded_bytes();
        if bytes.len() > suffix_bytes.len() && bytes.ends_with(suffix_bytes) {
            let trimmed = &bytes[..bytes.len() - suffix_bytes.len()];
            // SAFETY: `trimmed` is a prefix of valid `OsStr` encoded bytes cut at an ASCII suffix.
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                names.insert(OsStr::from_bytes(trimmed).to_os_string());
            }
            #[cfg(not(unix))]
            {
                if let Ok(s) = std::str::from_utf8(trimmed) {
                    names.insert(OsString::from(s));
                }
            }
        }
    }
    names
}

/// Collects and deduplicates directory entries across `[upper_dir, seed_dir, baseline_dir]`
/// in deterministic lexicographical order.
pub fn list_merged_dir_entries(
    cfg: &OverlayMountConfig,
    rel_dir: &Path,
) -> std::io::Result<Vec<MergedDirEntry>> {
    let mut entries: BTreeMap<OsString, MergedDirEntry> = BTreeMap::new();

    let upper_dir = if rel_dir.as_os_str().is_empty() {
        cfg.upper_dir.clone()
    } else {
        cfg.upper_dir.join(rel_dir)
    };
    if let Ok(meta) = std::fs::symlink_metadata(&upper_dir)
        && meta.file_type().is_dir()
    {
        for item in std::fs::read_dir(&upper_dir)? {
            let item = item?;
            let name = item.file_name();
            let ft = item.file_type()?;
            entries.insert(
                name.clone(),
                MergedDirEntry {
                    name,
                    file_type: ft,
                    host_path: item.path(),
                    layer: LayerKind::Upper,
                },
            );
        }
    }

    if lower_layers_blocked_at_dir(cfg, rel_dir) {
        return Ok(entries.into_values().collect());
    }

    let whiteout_names = read_dir_whiteout_names(&cfg.whiteouts_dir(), rel_dir);

    if let Some(ref seed) = cfg.seed_dir {
        let seed_path = if rel_dir.as_os_str().is_empty() {
            seed.clone()
        } else {
            seed.join(rel_dir)
        };
        if let Ok(meta) = std::fs::symlink_metadata(&seed_path)
            && meta.file_type().is_dir()
        {
            for item in std::fs::read_dir(&seed_path)? {
                let item = item?;
                let name = item.file_name();
                if entries.contains_key(&name) || whiteout_names.contains(&name) {
                    continue;
                }
                let ft = item.file_type()?;
                entries.insert(
                    name.clone(),
                    MergedDirEntry {
                        name,
                        file_type: ft,
                        host_path: item.path(),
                        layer: LayerKind::Seed,
                    },
                );
            }
        }
    }

    if baseline_blocked_at_dir(cfg, rel_dir) {
        return Ok(entries.into_values().collect());
    }

    let seed_whiteout_names = read_dir_whiteout_names(&cfg.seed_whiteouts_dir(), rel_dir);

    if let Some(ref base) = cfg.baseline_dir {
        let base_path = if rel_dir.as_os_str().is_empty() {
            base.clone()
        } else {
            base.join(rel_dir)
        };
        if let Ok(meta) = std::fs::symlink_metadata(&base_path)
            && meta.file_type().is_dir()
        {
            for item in std::fs::read_dir(&base_path)? {
                let item = item?;
                let name = item.file_name();
                if entries.contains_key(&name)
                    || whiteout_names.contains(&name)
                    || seed_whiteout_names.contains(&name)
                {
                    continue;
                }
                let ft = item.file_type()?;
                entries.insert(
                    name.clone(),
                    MergedDirEntry {
                        name,
                        file_type: ft,
                        host_path: item.path(),
                        layer: LayerKind::Baseline,
                    },
                );
            }
        }
    }

    Ok(entries.into_values().collect())
}

/// Scans `upper_dir` and `meta_dir/whiteouts/` in `O(dirty_files)` time without
/// traversing untouched `seed_dir` or `baseline_dir` trees.
pub fn scan_dirty_paths(cfg: &OverlayMountConfig) -> std::io::Result<Vec<DirtyPathEntry>> {
    let mut dirty: BTreeMap<String, DirtyPathEntry> = BTreeMap::new();

    if cfg.upper_dir.is_dir() {
        for entry in WalkDir::new(&cfg.upper_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            if entry.depth() == 0 || entry.file_type().is_dir() {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(&cfg.upper_dir) else {
                continue;
            };
            let rel_str = rel_to_slash_string(rel);
            if rel_str.is_empty() {
                continue;
            }
            let kind = if resolve_in_lower_layers(cfg, rel).is_some() {
                ChangeKind::Modified
            } else {
                ChangeKind::Added
            };
            let size_bytes = if entry.file_type().is_file() {
                entry.metadata().ok().map(|m| m.len())
            } else {
                None
            };
            dirty.insert(
                rel_str.clone(),
                DirtyPathEntry {
                    path: rel_str,
                    kind,
                    size_bytes,
                },
            );
        }
    }

    let whiteouts = cfg.whiteouts_dir();
    if whiteouts.is_dir() {
        for entry in WalkDir::new(&whiteouts)
            .follow_links(false)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            if entry.depth() == 0 || entry.file_type().is_dir() {
                continue;
            }
            let Ok(rel_marker) = entry.path().strip_prefix(&whiteouts) else {
                continue;
            };
            let marker_str = rel_to_slash_string(rel_marker);
            let Some(rel_str) = marker_str.strip_suffix(WHITEOUT_SUFFIX) else {
                continue;
            };
            if rel_str.is_empty() || dirty.contains_key(rel_str) {
                continue;
            }
            let rel_path = Path::new(rel_str);
            // Omit directory-only tombstones so DirtyPaths reports deleted files/symlinks cleanly.
            if let Some((_, _, meta)) = resolve_in_lower_layers(cfg, rel_path)
                && meta.file_type().is_dir()
            {
                continue;
            }
            dirty.insert(
                rel_str.to_string(),
                DirtyPathEntry {
                    path: rel_str.to_string(),
                    kind: ChangeKind::Deleted,
                    size_bytes: None,
                },
            );
        }
    }

    let opaque_dir = cfg.opaque_dir();
    if opaque_dir.is_dir() {
        for entry in WalkDir::new(&opaque_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            if entry.depth() == 0 || entry.file_type().is_dir() {
                continue;
            }
            let Ok(rel_marker) = entry.path().strip_prefix(&opaque_dir) else {
                continue;
            };
            let marker_str = rel_to_slash_string(rel_marker);
            let Some(opaque_rel_str) = marker_str.strip_suffix(OPAQUE_SUFFIX) else {
                continue;
            };
            if opaque_rel_str.is_empty() {
                continue;
            }
            let opaque_rel = Path::new(opaque_rel_str);
            for lower_root in [cfg.seed_dir.as_ref(), cfg.baseline_dir.as_ref()]
                .into_iter()
                .flatten()
            {
                let sub = lower_root.join(opaque_rel);
                if !sub.is_dir() {
                    continue;
                }
                for lower_ent in WalkDir::new(&sub)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(std::result::Result::ok)
                {
                    if lower_ent.file_type().is_dir() {
                        continue;
                    }
                    let Ok(tail) = lower_ent.path().strip_prefix(&sub) else {
                        continue;
                    };
                    let full_rel = opaque_rel.join(tail);
                    let full_str = rel_to_slash_string(&full_rel);
                    if dirty.contains_key(&full_str) {
                        continue;
                    }
                    // If seed_whiteouts already hid this baseline file, it wasn't part of this workspace.
                    if resolve_in_lower_layers(cfg, &full_rel).is_none() {
                        continue;
                    }
                    dirty.insert(
                        full_str.clone(),
                        DirtyPathEntry {
                            path: full_str,
                            kind: ChangeKind::Deleted,
                            size_bytes: None,
                        },
                    );
                }
            }
        }
    }

    Ok(dirty.into_values().collect())
}

/// Invokes `fusermount3 -u -z` (or `fusermount -u -z`) on `mount_point`.
/// Best-effort cleanup helper used for `ENOTCONN` recovery and forced unmounts.
pub fn lazy_unmount_fuse(mount_point: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::process::{Command, Stdio};

        for bin in ["fusermount3", "fusermount"] {
            match Command::new(bin)
                .arg("-u")
                .arg("-z")
                .arg("--")
                .arg(mount_point)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
            {
                Ok(out) if out.status.success() => {
                    // Brief wait for kernel mount table detachment to settle.
                    for _ in 0..50 {
                        match std::fs::symlink_metadata(mount_point) {
                            Err(e) if oz_core::is_enotconn(&e) => {
                                std::thread::sleep(std::time::Duration::from_millis(10));
                            }
                            _ => break,
                        }
                    }
                    return Ok(());
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if stderr.contains("not found in /etc/mtab")
                        || stderr.contains("not mounted")
                        || stderr.contains("Invalid argument")
                    {
                        return Ok(());
                    }
                    if bin == "fusermount" {
                        return Err(OzError::Other(format!(
                            "fusermount unmount failed for {}: {}",
                            mount_point.display(),
                            stderr.trim()
                        )));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(OzError::Io(e)),
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = mount_point;
        Ok(())
    }
}

/// Active mounted FUSE session handle.
#[cfg(target_os = "linux")]
pub struct MountedOverlay {
    pub config: OverlayMountConfig,
    session: Option<fuser::BackgroundSession>,
}

#[cfg(target_os = "linux")]
impl MountedOverlay {
    /// Unmounts the FUSE session cleanly and joins its background loop thread,
    /// falling back to `fusermount3 -u -z` if requested or if clean unmount fails.
    pub fn unmount(mut self, force: bool) -> Result<()> {
        let mp = self.config.mount_point.clone();
        if let Some(session) = self.session.take() {
            let res = session.umount_and_join();
            if res.is_err() || force {
                let _ = lazy_unmount_fuse(&mp);
                if !force {
                    res?;
                }
            }
        } else if force {
            lazy_unmount_fuse(&mp)?;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for MountedOverlay {
    fn drop(&mut self) {
        if let Some(session) = self.session.take()
            && session.umount_and_join().is_err()
        {
            let _ = lazy_unmount_fuse(&self.config.mount_point);
        }
    }
}

/// Non-Linux compile-time stub for [`MountedOverlay`].
#[cfg(not(target_os = "linux"))]
pub struct MountedOverlay {
    pub config: OverlayMountConfig,
}

#[cfg(not(target_os = "linux"))]
impl MountedOverlay {
    /// Non-Linux stub.
    pub fn unmount(self, _force: bool) -> Result<()> {
        Ok(())
    }
}

/// Mounts the CoW overlay described by `spec` over `/dev/fuse`.
#[cfg(target_os = "linux")]
pub fn mount_overlay(spec: &MountSpec) -> Result<MountedOverlay> {
    let cfg = OverlayMountConfig::from_spec(spec);
    if let Err(err) = std::fs::symlink_metadata(&cfg.mount_point)
        && oz_core::is_enotconn(&err)
    {
        lazy_unmount_fuse(&cfg.mount_point)?;
    }
    cfg.ensure_directories()?;
    let fs = linux_impl::OzoneOverlayFs::new(cfg.clone());
    let fuser_cfg = crate::default_linux_fuser_config(spec);
    let session = fuser::Session::new(fs, &cfg.mount_point, &fuser_cfg)
        .and_then(fuser::Session::spawn)
        .map_err(|err| OzError::MountUnavailable {
            workspace: spec.workspace.clone(),
            path: spec.mount_point.clone(),
            errno: err.raw_os_error(),
            message: format!("failed to mount FUSE3 overlay: {err}"),
        })?;
    Ok(MountedOverlay {
        config: cfg,
        session: Some(session),
    })
}

/// Non-Linux compile-time stub for [`mount_overlay`].
#[cfg(not(target_os = "linux"))]
pub fn mount_overlay(spec: &MountSpec) -> Result<MountedOverlay> {
    Err(OzError::MountUnavailable {
        workspace: spec.workspace.clone(),
        path: spec.mount_point.clone(),
        errno: None,
        message: "Linux FUSE3 overlay is only supported on Linux (ProjFS lands in Milestone 2)"
            .to_string(),
    })
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::*;
    use fuser::{
        AccessFlags, BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
        Generation, INodeNo, LockOwner, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr,
        ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
        ReplyWrite, Request, TimeOrNow, WriteFlags,
    };
    use std::collections::{BTreeSet, HashMap};
    use std::ffi::CString;
    use std::fs::{File, OpenOptions, Permissions};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const ZERO_TTL: Duration = Duration::ZERO;

    #[derive(Debug)]
    struct InodeEntry {
        paths: BTreeSet<PathBuf>,
        lookup_count: u64,
        unlinked: bool,
        is_dir: bool,
    }

    #[derive(Debug)]
    struct InodeTable {
        next_ino: u64,
        by_ino: HashMap<INodeNo, InodeEntry>,
        path_to_ino: BTreeMap<PathBuf, INodeNo>,
    }

    impl InodeTable {
        fn new() -> Self {
            let mut by_ino = HashMap::new();
            let mut root_paths = BTreeSet::new();
            root_paths.insert(PathBuf::new());
            by_ino.insert(
                INodeNo::ROOT,
                InodeEntry {
                    paths: root_paths,
                    lookup_count: u64::MAX,
                    unlinked: false,
                    is_dir: true,
                },
            );
            let mut path_to_ino = BTreeMap::new();
            path_to_ino.insert(PathBuf::new(), INodeNo::ROOT);
            Self {
                next_ino: 2,
                by_ino,
                path_to_ino,
            }
        }

        fn resolve_ino_path(&self, ino: INodeNo) -> std::result::Result<PathBuf, Errno> {
            let entry = self.by_ino.get(&ino).ok_or(Errno::ENOENT)?;
            if entry.unlinked {
                return Err(Errno::ENOENT);
            }
            entry.paths.iter().next().cloned().ok_or(Errno::ENOENT)
        }

        fn lookup_or_insert(&mut self, rel: &Path, is_dir: bool, inc_lookup: bool) -> INodeNo {
            if let Some(&ino) = self.path_to_ino.get(rel)
                && let Some(entry) = self.by_ino.get_mut(&ino)
            {
                if !entry.unlinked && entry.is_dir == is_dir {
                    if inc_lookup {
                        entry.lookup_count = entry.lookup_count.saturating_add(1);
                    }
                    return ino;
                }
                // File type changed behind FUSE (e.g., host-side oz undo); detach old mapping.
                entry.paths.remove(rel);
                if entry.paths.is_empty() {
                    entry.unlinked = true;
                }
                self.path_to_ino.remove(rel);
            }

            let ino = INodeNo(self.next_ino);
            self.next_ino += 1;
            let mut paths = BTreeSet::new();
            paths.insert(rel.to_path_buf());
            self.by_ino.insert(
                ino,
                InodeEntry {
                    paths,
                    lookup_count: u64::from(inc_lookup),
                    unlinked: false,
                    is_dir,
                },
            );
            self.path_to_ino.insert(rel.to_path_buf(), ino);
            ino
        }

        fn add_alias(
            &mut self,
            ino: INodeNo,
            new_rel: PathBuf,
            inc_lookup: bool,
        ) -> std::result::Result<(), Errno> {
            self.detach_path(&new_rel);
            let entry = self.by_ino.get_mut(&ino).ok_or(Errno::ENOENT)?;
            if entry.is_dir {
                return Err(Errno::EPERM);
            }
            entry.paths.insert(new_rel.clone());
            entry.unlinked = false;
            if inc_lookup {
                entry.lookup_count = entry.lookup_count.saturating_add(1);
            }
            self.path_to_ino.insert(new_rel, ino);
            Ok(())
        }

        fn detach_path(&mut self, rel: &Path) {
            if let Some(ino) = self.path_to_ino.remove(rel)
                && let Some(entry) = self.by_ino.get_mut(&ino)
            {
                entry.paths.remove(rel);
                if entry.paths.is_empty() {
                    entry.unlinked = true;
                    if entry.lookup_count == 0 {
                        self.by_ino.remove(&ino);
                    }
                }
            }
        }

        fn forget(&mut self, ino: INodeNo, nlookup: u64) {
            if ino == INodeNo::ROOT {
                return;
            }
            let Some(entry) = self.by_ino.get_mut(&ino) else {
                return;
            };
            entry.lookup_count = entry.lookup_count.saturating_sub(nlookup);
            if entry.lookup_count == 0 {
                let paths = std::mem::take(&mut entry.paths);
                for p in paths {
                    if self.path_to_ino.get(&p) == Some(&ino) {
                        self.path_to_ino.remove(&p);
                    }
                }
                self.by_ino.remove(&ino);
            }
        }

        fn rename_subtree(&mut self, src_rel: &Path, dst_rel: &Path) {
            let dst_targets: Vec<PathBuf> = self
                .path_to_ino
                .keys()
                .filter(|p| *p == dst_rel || p.starts_with(dst_rel))
                .cloned()
                .collect();
            for p in dst_targets {
                self.detach_path(&p);
            }

            let src_entries: Vec<(PathBuf, INodeNo)> = self
                .path_to_ino
                .iter()
                .filter(|(p, _)| *p == src_rel || p.starts_with(src_rel))
                .map(|(p, &ino)| (p.clone(), ino))
                .collect();

            for (old_p, ino) in src_entries {
                self.path_to_ino.remove(&old_p);
                let new_p = if old_p == src_rel {
                    dst_rel.to_path_buf()
                } else if let Ok(tail) = old_p.strip_prefix(src_rel) {
                    dst_rel.join(tail)
                } else {
                    continue;
                };
                self.path_to_ino.insert(new_p.clone(), ino);
                if let Some(entry) = self.by_ino.get_mut(&ino) {
                    entry.paths.remove(&old_p);
                    entry.paths.insert(new_p);
                }
            }
        }
    }

    #[derive(Debug)]
    struct OpenFile {
        ino: INodeNo,
        file: Arc<File>,
        writable: bool,
    }

    type DirSnapshot = Arc<Vec<(INodeNo, FileType, OsString)>>;

    #[derive(Debug)]
    struct InnerState {
        inodes: InodeTable,
        next_fh: u64,
        open_files: HashMap<FileHandle, OpenFile>,
        open_dirs: HashMap<FileHandle, Option<DirSnapshot>>,
        next_tmp: u64,
    }

    pub(crate) struct OzoneOverlayFs {
        cfg: OverlayMountConfig,
        state: Mutex<InnerState>,
    }

    impl OzoneOverlayFs {
        pub(crate) fn new(cfg: OverlayMountConfig) -> Self {
            Self {
                cfg,
                state: Mutex::new(InnerState {
                    inodes: InodeTable::new(),
                    next_fh: 1,
                    open_files: HashMap::new(),
                    open_dirs: HashMap::new(),
                    next_tmp: 1,
                }),
            }
        }

        fn upper_path(&self, rel: &Path) -> PathBuf {
            if rel.as_os_str().is_empty() {
                self.cfg.upper_dir.clone()
            } else {
                self.cfg.upper_dir.join(rel)
            }
        }

        fn ensure_parent_dirs(path: &Path) -> std::result::Result<(), Errno> {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(|e| errno_from_io(&e))?;
            }
            Ok(())
        }

        fn write_marker(path: &Path) -> std::result::Result<(), Errno> {
            Self::ensure_parent_dirs(path)?;
            File::create(path).map_err(|e| errno_from_io(&e))?;
            Ok(())
        }

        fn remove_marker_if_exists(path: &Path) -> std::result::Result<(), Errno> {
            match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(errno_from_io(&e)),
            }
        }

        fn remove_subtree_markers(root: &Path, rel: &Path, suffix: &str) {
            let _ = Self::remove_marker_if_exists(&marker_path(root, rel, suffix));
            let dir_path = root.join(rel);
            if dir_path.is_dir() {
                let _ = std::fs::remove_dir_all(&dir_path);
            }
        }

        fn ensure_upper_dir(&self, rel_dir: &Path) -> std::result::Result<(), Errno> {
            if rel_dir.as_os_str().is_empty() {
                std::fs::create_dir_all(&self.cfg.upper_dir).map_err(|e| errno_from_io(&e))?;
                return Ok(());
            }

            let mut cur_rel = PathBuf::new();
            for comp in rel_dir.components() {
                let Component::Normal(part) = comp else {
                    continue;
                };
                cur_rel.push(part);
                let upper_cand = self.upper_path(&cur_rel);
                match std::fs::symlink_metadata(&upper_cand) {
                    Ok(meta) => {
                        if !meta.file_type().is_dir() {
                            return Err(Errno::ENOTDIR);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        let resolved = resolve_rel_path(&self.cfg, &cur_rel).ok_or(Errno::ENOENT)?;
                        let lower_meta = std::fs::symlink_metadata(&resolved.host_path)
                            .map_err(|err| errno_from_io(&err))?;
                        if !lower_meta.file_type().is_dir() {
                            return Err(Errno::ENOTDIR);
                        }
                        std::fs::create_dir(&upper_cand).map_err(|err| errno_from_io(&err))?;
                        let mode = lower_meta.mode() & 0o7777;
                        let _ = std::fs::set_permissions(&upper_cand, Permissions::from_mode(mode));
                        let _ = copy_timestamps(&upper_cand, &lower_meta);
                    }
                    Err(e) => return Err(errno_from_io(&e)),
                }
            }
            Ok(())
        }

        fn copy_up_entry(
            &self,
            inner: &mut InnerState,
            rel: &Path,
            truncate_to_zero: bool,
        ) -> std::result::Result<(), Errno> {
            if rel.as_os_str().is_empty() {
                return Ok(());
            }
            let upper_target = self.upper_path(rel);
            if let Ok(meta) = std::fs::symlink_metadata(&upper_target) {
                if truncate_to_zero && meta.file_type().is_file() {
                    let f = OpenOptions::new()
                        .write(true)
                        .open(&upper_target)
                        .map_err(|e| errno_from_io(&e))?;
                    f.set_len(0).map_err(|e| errno_from_io(&e))?;
                }
                return Ok(());
            }

            let resolved = resolve_rel_path(&self.cfg, rel).ok_or(Errno::ENOENT)?;
            let lower_meta =
                std::fs::symlink_metadata(&resolved.host_path).map_err(|e| errno_from_io(&e))?;
            if let Some(parent) = rel.parent() {
                self.ensure_upper_dir(parent)?;
            }

            let ft = lower_meta.file_type();
            if ft.is_dir() {
                std::fs::create_dir(&upper_target).map_err(|e| errno_from_io(&e))?;
                let _ = std::fs::set_permissions(
                    &upper_target,
                    Permissions::from_mode(lower_meta.mode() & 0o7777),
                );
                let _ = copy_timestamps(&upper_target, &lower_meta);
            } else if ft.is_symlink() {
                let target =
                    std::fs::read_link(&resolved.host_path).map_err(|e| errno_from_io(&e))?;
                std::os::unix::fs::symlink(&target, &upper_target)
                    .map_err(|e| errno_from_io(&e))?;
                let _ = copy_timestamps(&upper_target, &lower_meta);
            } else if ft.is_file() {
                let tmp_dir = self.cfg.meta_dir.join("tmp");
                std::fs::create_dir_all(&tmp_dir).map_err(|e| errno_from_io(&e))?;
                let tmp_seq = inner.next_tmp;
                inner.next_tmp += 1;
                let tmp_path = tmp_dir.join(format!("copyup.{tmp_seq}"));
                let mut dst = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&tmp_path)
                    .map_err(|e| errno_from_io(&e))?;
                if !truncate_to_zero {
                    let mut src =
                        File::open(&resolved.host_path).map_err(|e| errno_from_io(&e))?;
                    std::io::copy(&mut src, &mut dst).map_err(|e| {
                        let _ = std::fs::remove_file(&tmp_path);
                        errno_from_io(&e)
                    })?;
                }
                drop(dst);
                let _ = std::fs::set_permissions(
                    &tmp_path,
                    Permissions::from_mode(lower_meta.mode() & 0o7777),
                );
                let _ = copy_timestamps(&tmp_path, &lower_meta);
                if let Err(e) = std::fs::rename(&tmp_path, &upper_target) {
                    if e.raw_os_error() == Some(libc::EXDEV) {
                        std::fs::copy(&tmp_path, &upper_target).map_err(|err| {
                            let _ = std::fs::remove_file(&tmp_path);
                            errno_from_io(&err)
                        })?;
                        let _ = std::fs::remove_file(&tmp_path);
                        let _ = std::fs::set_permissions(
                            &upper_target,
                            Permissions::from_mode(lower_meta.mode() & 0o7777),
                        );
                        let _ = copy_timestamps(&upper_target, &lower_meta);
                    } else {
                        let _ = std::fs::remove_file(&tmp_path);
                        return Err(errno_from_io(&e));
                    }
                }
            } else {
                return Err(Errno::EOPNOTSUPP);
            }

            Ok(())
        }

        fn materialize_lower_dir_tree(
            &self,
            inner: &mut InnerState,
            rel_dir: &Path,
            removed_lower_files: &mut Vec<PathBuf>,
        ) -> std::result::Result<(), Errno> {
            self.ensure_upper_dir(rel_dir)?;
            let entries = list_merged_dir_entries(&self.cfg, rel_dir).map_err(|e| errno_from_io(&e))?;
            for entry in entries {
                let child_rel = if rel_dir.as_os_str().is_empty() {
                    PathBuf::from(&entry.name)
                } else {
                    rel_dir.join(&entry.name)
                };
                if entry.layer != LayerKind::Upper && !entry.file_type.is_dir() {
                    removed_lower_files.push(child_rel.clone());
                }
                if entry.file_type.is_dir() {
                    self.materialize_lower_dir_tree(inner, &child_rel, removed_lower_files)?;
                } else {
                    self.copy_up_entry(inner, &child_rel, false)?;
                }
            }
            Ok(())
        }

        fn stat_resolved(
            &self,
            inner: &mut InnerState,
            rel: &Path,
            inc_lookup: bool,
        ) -> std::result::Result<FileAttr, Errno> {
            let resolved = match resolve_rel_path(&self.cfg, rel) {
                Some(r) => r,
                None => {
                    inner.inodes.detach_path(rel);
                    return Err(Errno::ENOENT);
                }
            };
            let meta = std::fs::symlink_metadata(&resolved.host_path).map_err(|e| {
                inner.inodes.detach_path(rel);
                errno_from_io(&e)
            })?;
            let is_dir = meta.file_type().is_dir();
            let ino = if rel.as_os_str().is_empty() {
                INodeNo::ROOT
            } else {
                inner.inodes.lookup_or_insert(rel, is_dir, inc_lookup)
            };
            Ok(metadata_to_file_attr(ino, &meta))
        }
    }

    fn errno_from_io(err: &std::io::Error) -> Errno {
        err.raw_os_error()
            .map(Errno::from_i32)
            .unwrap_or(Errno::EIO)
    }

    fn system_time_from_unix(secs: i64, nsec: i64) -> SystemTime {
        let clamped_ns = u32::try_from(nsec.clamp(0, 999_999_999)).unwrap_or(0);
        if secs >= 0 {
            UNIX_EPOCH + Duration::new(secs as u64, clamped_ns)
        } else {
            let abs_secs = secs.unsigned_abs();
            if clamped_ns == 0 {
                UNIX_EPOCH
                    .checked_sub(Duration::from_secs(abs_secs))
                    .unwrap_or(UNIX_EPOCH)
            } else {
                UNIX_EPOCH
                    .checked_sub(Duration::new(
                        abs_secs.saturating_sub(1),
                        1_000_000_000 - clamped_ns,
                    ))
                    .unwrap_or(UNIX_EPOCH)
            }
        }
    }

    fn metadata_to_file_attr(ino: INodeNo, meta: &std::fs::Metadata) -> FileAttr {
        let kind = FileType::from_std(meta.file_type()).unwrap_or(FileType::RegularFile);
        // Report nlink=1 for directories so traversal tools (e.g. GNU find) do not apply
        // leaf-directory optimizations that assume upperdir subdirs equal merged subdirs.
        let nlink = if kind == FileType::Directory {
            1
        } else {
            meta.nlink() as u32
        };
        FileAttr {
            ino,
            size: meta.len(),
            blocks: meta.blocks(),
            atime: system_time_from_unix(meta.atime(), meta.atime_nsec()),
            mtime: system_time_from_unix(meta.mtime(), meta.mtime_nsec()),
            ctime: system_time_from_unix(meta.ctime(), meta.ctime_nsec()),
            crtime: UNIX_EPOCH,
            kind,
            perm: (meta.mode() & 0o7777) as u16,
            nlink,
            uid: meta.uid(),
            gid: meta.gid(),
            rdev: meta.rdev() as u32,
            blksize: 4096,
            flags: 0,
        }
    }

    fn to_libc_timespec(val: Option<TimeOrNow>) -> libc::timespec {
        match val {
            None => libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            },
            Some(TimeOrNow::Now) => libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_NOW,
            },
            Some(TimeOrNow::SpecificTime(st)) => match st.duration_since(UNIX_EPOCH) {
                Ok(d) => libc::timespec {
                    tv_sec: d.as_secs() as libc::time_t,
                    tv_nsec: d.subsec_nanos() as libc::c_long,
                },
                Err(e) => {
                    let d = e.duration();
                    let secs = d.as_secs();
                    let nsec = d.subsec_nanos();
                    if nsec == 0 {
                        libc::timespec {
                            tv_sec: -(secs as libc::time_t),
                            tv_nsec: 0,
                        }
                    } else {
                        libc::timespec {
                            tv_sec: -(secs as libc::time_t) - 1,
                            tv_nsec: (1_000_000_000 - nsec) as libc::c_long,
                        }
                    }
                }
            },
        }
    }

    fn set_times_path(
        path: &Path,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
    ) -> std::result::Result<(), Errno> {
        if atime.is_none() && mtime.is_none() {
            return Ok(());
        }
        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| Errno::EINVAL)?;
        let times = [to_libc_timespec(atime), to_libc_timespec(mtime)];
        let rc = unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                c_path.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            return Err(errno_from_io(&std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn set_times_fd(
        file: &File,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
    ) -> std::result::Result<(), Errno> {
        if atime.is_none() && mtime.is_none() {
            return Ok(());
        }
        let times = [to_libc_timespec(atime), to_libc_timespec(mtime)];
        let rc = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
        if rc != 0 {
            return Err(errno_from_io(&std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn copy_timestamps(dst: &Path, meta: &std::fs::Metadata) -> std::result::Result<(), Errno> {
        let atime = system_time_from_unix(meta.atime(), meta.atime_nsec());
        let mtime = system_time_from_unix(meta.mtime(), meta.mtime_nsec());
        set_times_path(
            dst,
            Some(TimeOrNow::SpecificTime(atime)),
            Some(TimeOrNow::SpecificTime(mtime)),
        )
    }

    fn lchown_path(path: &Path, uid: Option<u32>, gid: Option<u32>) -> std::result::Result<(), Errno> {
        if uid.is_none() && gid.is_none() {
            return Ok(());
        }
        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| Errno::EINVAL)?;
        let uid_val = uid.map_or(u32::MAX as libc::uid_t, |u| u as libc::uid_t);
        let gid_val = gid.map_or(u32::MAX as libc::gid_t, |g| g as libc::gid_t);
        let rc = unsafe { libc::lchown(c_path.as_ptr(), uid_val, gid_val) };
        if rc != 0 {
            return Err(errno_from_io(&std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn collect_lower_file_paths(
        cfg: &OverlayMountConfig,
        rel_dir: &Path,
        out: &mut BTreeSet<PathBuf>,
    ) {
        let seed_whiteouts = cfg.seed_whiteouts_dir();
        if let Some(ref seed) = cfg.seed_dir {
            let p = seed.join(rel_dir);
            if p.is_dir() {
                for entry in WalkDir::new(&p)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(std::result::Result::ok)
                {
                    if entry.file_type().is_dir() {
                        continue;
                    }
                    if let Ok(tail) = entry.path().strip_prefix(&p) {
                        out.insert(rel_dir.join(tail));
                    }
                }
            }
        }
        if let Some(ref base) = cfg.baseline_dir {
            let p = base.join(rel_dir);
            if p.is_dir() {
                for entry in WalkDir::new(&p)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(std::result::Result::ok)
                {
                    if entry.file_type().is_dir() {
                        continue;
                    }
                    if let Ok(tail) = entry.path().strip_prefix(&p) {
                        let rel = rel_dir.join(tail);
                        if !ancestors_inclusive(&rel).iter().any(|pref| {
                            marker_path(&seed_whiteouts, pref, WHITEOUT_SUFFIX).exists()
                        }) {
                            out.insert(rel);
                        }
                    }
                }
            }
        }
    }

    impl Filesystem for OzoneOverlayFs {
        fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            let parent_rel = match inner.inodes.resolve_ino_path(parent) {
                Ok(p) => p,
                Err(e) => {
                    reply.error(e);
                    return;
                }
            };
            let rel = if name == "." {
                parent_rel
            } else if name == ".." {
                parent_rel
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default()
            } else {
                if !is_valid_child_name(name) {
                    reply.error(Errno::EINVAL);
                    return;
                }
                if parent_rel.as_os_str().is_empty() {
                    PathBuf::from(name)
                } else {
                    parent_rel.join(name)
                }
            };

            match self.stat_resolved(&mut inner, &rel, true) {
                Ok(attr) => reply.entry(&ZERO_TTL, &attr, Generation(0)),
                Err(e) => reply.error(e),
            }
        }

        fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            inner.inodes.forget(ino, nlookup);
        }

        fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            if let Ok(rel) = inner.inodes.resolve_ino_path(ino)
                && let Ok(attr) = self.stat_resolved(&mut inner, &rel, false)
            {
                reply.attr(&ZERO_TTL, &attr);
                return;
            }
            let open_match = fh
                .and_then(|handle| inner.open_files.get(&handle))
                .or_else(|| inner.open_files.values().find(|o| o.ino == ino));
            if let Some(open) = open_match
                && let Ok(meta) = open.file.metadata()
            {
                reply.attr(&ZERO_TTL, &metadata_to_file_attr(ino, &meta));
                return;
            }
            reply.error(Errno::ENOENT);
        }

        fn setattr(
            &self,
            _req: &Request,
            ino: INodeNo,
            mode: Option<u32>,
            uid: Option<u32>,
            gid: Option<u32>,
            size: Option<u64>,
            atime: Option<TimeOrNow>,
            mtime: Option<TimeOrNow>,
            _ctime: Option<SystemTime>,
            fh: Option<FileHandle>,
            _crtime: Option<SystemTime>,
            _chgtime: Option<SystemTime>,
            _bkuptime: Option<SystemTime>,
            _flags: Option<BsdFileFlags>,
            reply: ReplyAttr,
        ) {
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            let rel_res = inner.inodes.resolve_ino_path(ino);

            if let Err(err) = rel_res {
                let open_match = fh
                    .and_then(|handle| inner.open_files.get(&handle))
                    .or_else(|| {
                        inner
                            .open_files
                            .values()
                            .find(|o| o.ino == ino && o.writable)
                            .or_else(|| inner.open_files.values().find(|o| o.ino == ino))
                    });
                if let Some(open) = open_match {
                    let file = Arc::clone(&open.file);
                    let writable = open.writable;
                    drop(inner);
                    if let Some(new_size) = size {
                        if !writable {
                            reply.error(Errno::EACCES);
                            return;
                        }
                        if let Err(e) = file.set_len(new_size) {
                            reply.error(errno_from_io(&e));
                            return;
                        }
                    }
                    if let Some(m) = mode
                        && let Err(e) = file.set_permissions(Permissions::from_mode(m & 0o7777))
                    {
                        reply.error(errno_from_io(&e));
                        return;
                    }
                    if let Err(e) = set_times_fd(&file, atime, mtime) {
                        reply.error(e);
                        return;
                    }
                    match file.metadata() {
                        Ok(meta) => reply.attr(&ZERO_TTL, &metadata_to_file_attr(ino, &meta)),
                        Err(e) => reply.error(errno_from_io(&e)),
                    }
                    return;
                }
                reply.error(err);
                return;
            }

            let rel = rel_res.expect("checked above");
            let truncate_zero = size == Some(0);
            if let Err(e) = self.copy_up_entry(&mut inner, &rel, truncate_zero) {
                reply.error(e);
                return;
            }
            let upper_path = self.upper_path(&rel);

            // If `mode` grants owner write alongside a size change (e.g. chmod +w + truncate on a
            // read-only lower file), apply `mode` before opening `upper_path` for truncation.
            let mut mode_applied = false;
            if let Some(m) = mode
                && size.is_some()
                && (m & 0o200) != 0
            {
                if let Err(e) =
                    std::fs::set_permissions(&upper_path, Permissions::from_mode(m & 0o7777))
                {
                    reply.error(errno_from_io(&e));
                    return;
                }
                mode_applied = true;
            }

            if let Some(new_size) = size
                && !truncate_zero
            {
                let res = if let Some(handle) = fh
                    && let Some(open) = inner.open_files.get(&handle)
                    && open.writable
                {
                    open.file.set_len(new_size)
                } else {
                    OpenOptions::new()
                        .write(true)
                        .open(&upper_path)
                        .and_then(|f| f.set_len(new_size))
                };
                if let Err(e) = res {
                    reply.error(errno_from_io(&e));
                    return;
                }
            }

            if let Err(e) = lchown_path(&upper_path, uid, gid) {
                reply.error(e);
                return;
            }

            if !mode_applied
                && let Some(m) = mode
                && let Err(e) =
                    std::fs::set_permissions(&upper_path, Permissions::from_mode(m & 0o7777))
            {
                reply.error(errno_from_io(&e));
                return;
            }

            if let Err(e) = set_times_path(&upper_path, atime, mtime) {
                reply.error(e);
                return;
            }

            match std::fs::symlink_metadata(&upper_path) {
                Ok(meta) => reply.attr(&ZERO_TTL, &metadata_to_file_attr(ino, &meta)),
                Err(e) => reply.error(errno_from_io(&e)),
            }
        }

        fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
            let inner = self.state.lock().expect("vfsd lock poisoned");
            let Ok(rel) = inner.inodes.resolve_ino_path(ino) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let Some(resolved) = resolve_rel_path(&self.cfg, &rel) else {
                reply.error(Errno::ENOENT);
                return;
            };
            drop(inner);
            match std::fs::read_link(&resolved.host_path) {
                Ok(target) => reply.data(target.as_os_str().as_bytes()),
                Err(e) => reply.error(errno_from_io(&e)),
            }
        }

        fn mknod(
            &self,
            _req: &Request,
            parent: INodeNo,
            name: &OsStr,
            mode: u32,
            umask: u32,
            _rdev: u32,
            reply: ReplyEntry,
        ) {
            let file_type = mode & libc::S_IFMT;
            if file_type != 0 && file_type != libc::S_IFREG {
                reply.error(Errno::EPERM);
                return;
            }
            if !is_valid_child_name(name) {
                reply.error(Errno::EINVAL);
                return;
            }
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            let Ok(parent_rel) = inner.inodes.resolve_ino_path(parent) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let rel = if parent_rel.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                parent_rel.join(name)
            };
            if resolve_rel_path(&self.cfg, &rel).is_some() {
                reply.error(Errno::EEXIST);
                return;
            }
            if let Err(e) = self.ensure_upper_dir(&parent_rel) {
                reply.error(e);
                return;
            }
            let wh_path = marker_path(&self.cfg.whiteouts_dir(), &rel, WHITEOUT_SUFFIX);
            let _ = Self::remove_marker_if_exists(&wh_path);

            let eff_mode = (mode & !umask) & 0o7777;
            let upper_path = self.upper_path(&rel);
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&upper_path)
            {
                Ok(_) => {
                    let _ =
                        std::fs::set_permissions(&upper_path, Permissions::from_mode(eff_mode));
                    match self.stat_resolved(&mut inner, &rel, true) {
                        Ok(attr) => reply.entry(&ZERO_TTL, &attr, Generation(0)),
                        Err(e) => reply.error(e),
                    }
                }
                Err(e) => reply.error(errno_from_io(&e)),
            }
        }

        fn mkdir(
            &self,
            _req: &Request,
            parent: INodeNo,
            name: &OsStr,
            mode: u32,
            umask: u32,
            reply: ReplyEntry,
        ) {
            if !is_valid_child_name(name) {
                reply.error(Errno::EINVAL);
                return;
            }
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            let Ok(parent_rel) = inner.inodes.resolve_ino_path(parent) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let rel = if parent_rel.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                parent_rel.join(name)
            };
            if resolve_rel_path(&self.cfg, &rel).is_some() {
                reply.error(Errno::EEXIST);
                return;
            }
            if let Err(e) = self.ensure_upper_dir(&parent_rel) {
                reply.error(e);
                return;
            }

            let wh_path = marker_path(&self.cfg.whiteouts_dir(), &rel, WHITEOUT_SUFFIX);
            let had_whiteout = wh_path.exists();
            let exists_in_lower = resolve_in_lower_layers(&self.cfg, &rel).is_some();

            let upper_path = self.upper_path(&rel);
            if let Err(e) = std::fs::create_dir(&upper_path) {
                reply.error(errno_from_io(&e));
                return;
            }
            let eff_mode = (mode & !umask) & 0o7777;
            let _ = std::fs::set_permissions(&upper_path, Permissions::from_mode(eff_mode));

            let _ = Self::remove_marker_if_exists(&wh_path);
            if had_whiteout || exists_in_lower {
                let opaque_path = marker_path(&self.cfg.opaque_dir(), &rel, OPAQUE_SUFFIX);
                if let Err(e) = Self::write_marker(&opaque_path) {
                    reply.error(e);
                    return;
                }
            }

            match self.stat_resolved(&mut inner, &rel, true) {
                Ok(attr) => reply.entry(&ZERO_TTL, &attr, Generation(0)),
                Err(e) => reply.error(e),
            }
        }

        fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
            if !is_valid_child_name(name) {
                reply.error(Errno::EINVAL);
                return;
            }
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            let Ok(parent_rel) = inner.inodes.resolve_ino_path(parent) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let rel = if parent_rel.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                parent_rel.join(name)
            };
            let Some(resolved) = resolve_rel_path(&self.cfg, &rel) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let Ok(meta) = std::fs::symlink_metadata(&resolved.host_path) else {
                reply.error(Errno::ENOENT);
                return;
            };
            if meta.file_type().is_dir() {
                reply.error(Errno::EISDIR);
                return;
            }

            let upper_path = self.upper_path(&rel);
            if std::fs::symlink_metadata(&upper_path).is_ok()
                && let Err(e) = std::fs::remove_file(&upper_path)
            {
                reply.error(errno_from_io(&e));
                return;
            }

            if resolve_in_lower_layers(&self.cfg, &rel).is_some()
                && !lower_layers_blocked_at_dir(&self.cfg, &parent_rel)
            {
                let wh_path = marker_path(&self.cfg.whiteouts_dir(), &rel, WHITEOUT_SUFFIX);
                if let Err(e) = Self::write_marker(&wh_path) {
                    reply.error(e);
                    return;
                }
            }

            inner.inodes.detach_path(&rel);
            reply.ok();
        }

        fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
            if !is_valid_child_name(name) {
                reply.error(Errno::EINVAL);
                return;
            }
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            let Ok(parent_rel) = inner.inodes.resolve_ino_path(parent) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let rel = if parent_rel.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                parent_rel.join(name)
            };
            let Some(resolved) = resolve_rel_path(&self.cfg, &rel) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let Ok(meta) = std::fs::symlink_metadata(&resolved.host_path) else {
                reply.error(Errno::ENOENT);
                return;
            };
            if !meta.file_type().is_dir() {
                reply.error(Errno::ENOTDIR);
                return;
            }

            match list_merged_dir_entries(&self.cfg, &rel) {
                Ok(entries) if !entries.is_empty() => {
                    reply.error(Errno::ENOTEMPTY);
                    return;
                }
                Err(e) => {
                    reply.error(errno_from_io(&e));
                    return;
                }
                Ok(_) => {}
            }

            let upper_path = self.upper_path(&rel);
            if std::fs::symlink_metadata(&upper_path).is_ok()
                && let Err(e) = std::fs::remove_dir(&upper_path)
            {
                reply.error(errno_from_io(&e));
                return;
            }

            Self::remove_subtree_markers(&self.cfg.opaque_dir(), &rel, OPAQUE_SUFFIX);

            if resolve_in_lower_layers(&self.cfg, &rel).is_some()
                && !lower_layers_blocked_at_dir(&self.cfg, &parent_rel)
            {
                let wh_path = marker_path(&self.cfg.whiteouts_dir(), &rel, WHITEOUT_SUFFIX);
                if let Err(e) = Self::write_marker(&wh_path) {
                    reply.error(e);
                    return;
                }
            }

            inner.inodes.detach_path(&rel);
            reply.ok();
        }

        fn symlink(
            &self,
            _req: &Request,
            parent: INodeNo,
            link_name: &OsStr,
            target: &Path,
            reply: ReplyEntry,
        ) {
            if !is_valid_child_name(link_name) {
                reply.error(Errno::EINVAL);
                return;
            }
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            let Ok(parent_rel) = inner.inodes.resolve_ino_path(parent) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let rel = if parent_rel.as_os_str().is_empty() {
                PathBuf::from(link_name)
            } else {
                parent_rel.join(link_name)
            };
            if resolve_rel_path(&self.cfg, &rel).is_some() {
                reply.error(Errno::EEXIST);
                return;
            }
            if let Err(e) = self.ensure_upper_dir(&parent_rel) {
                reply.error(e);
                return;
            }

            let wh_path = marker_path(&self.cfg.whiteouts_dir(), &rel, WHITEOUT_SUFFIX);
            let _ = Self::remove_marker_if_exists(&wh_path);

            let upper_path = self.upper_path(&rel);
            if let Err(e) = std::os::unix::fs::symlink(target, &upper_path) {
                reply.error(errno_from_io(&e));
                return;
            }

            match self.stat_resolved(&mut inner, &rel, true) {
                Ok(attr) => reply.entry(&ZERO_TTL, &attr, Generation(0)),
                Err(e) => reply.error(e),
            }
        }

        fn rename(
            &self,
            _req: &Request,
            parent: INodeNo,
            name: &OsStr,
            newparent: INodeNo,
            newname: &OsStr,
            flags: RenameFlags,
            reply: ReplyEmpty,
        ) {
            if flags.intersects(RenameFlags::RENAME_EXCHANGE | RenameFlags::RENAME_WHITEOUT) {
                reply.error(Errno::EINVAL);
                return;
            }
            if !is_valid_child_name(name) || !is_valid_child_name(newname) {
                reply.error(Errno::EINVAL);
                return;
            }

            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            let Ok(src_parent) = inner.inodes.resolve_ino_path(parent) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let Ok(dst_parent) = inner.inodes.resolve_ino_path(newparent) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let src_rel = if src_parent.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                src_parent.join(name)
            };
            let dst_rel = if dst_parent.as_os_str().is_empty() {
                PathBuf::from(newname)
            } else {
                dst_parent.join(newname)
            };

            if src_rel == dst_rel {
                reply.ok();
                return;
            }
            if dst_rel.starts_with(&src_rel) {
                reply.error(Errno::EINVAL);
                return;
            }

            let Some(src_resolved) = resolve_rel_path(&self.cfg, &src_rel) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let Ok(src_meta) = std::fs::symlink_metadata(&src_resolved.host_path) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let src_is_dir = src_meta.file_type().is_dir();

            let dst_resolved = resolve_rel_path(&self.cfg, &dst_rel);
            if let Some(ref dst_res) = dst_resolved {
                if flags.contains(RenameFlags::RENAME_NOREPLACE) {
                    reply.error(Errno::EEXIST);
                    return;
                }
                let Ok(dst_meta) = std::fs::symlink_metadata(&dst_res.host_path) else {
                    reply.error(Errno::EIO);
                    return;
                };
                let dst_is_dir = dst_meta.file_type().is_dir();
                if src_is_dir && !dst_is_dir {
                    reply.error(Errno::ENOTDIR);
                    return;
                }
                if !src_is_dir && dst_is_dir {
                    reply.error(Errno::EISDIR);
                    return;
                }
                if dst_is_dir {
                    match list_merged_dir_entries(&self.cfg, &dst_rel) {
                        Ok(entries) if !entries.is_empty() => {
                            reply.error(Errno::ENOTEMPTY);
                            return;
                        }
                        Err(e) => {
                            reply.error(errno_from_io(&e));
                            return;
                        }
                        Ok(_) => {}
                    }
                }
            }

            if let Err(e) = self.ensure_upper_dir(&dst_parent) {
                reply.error(e);
                return;
            }

            if !src_is_dir {
                if let Err(e) = self.copy_up_entry(&mut inner, &src_rel, false) {
                    reply.error(e);
                    return;
                }
                let _ = Self::remove_marker_if_exists(&marker_path(
                    &self.cfg.whiteouts_dir(),
                    &dst_rel,
                    WHITEOUT_SUFFIX,
                ));
                let upper_src = self.upper_path(&src_rel);
                let upper_dst = self.upper_path(&dst_rel);
                if let Err(e) = std::fs::rename(&upper_src, &upper_dst) {
                    reply.error(errno_from_io(&e));
                    return;
                }
                if resolve_in_lower_layers(&self.cfg, &src_rel).is_some()
                    && !lower_layers_blocked_at_dir(&self.cfg, &src_parent)
                {
                    let wh_path =
                        marker_path(&self.cfg.whiteouts_dir(), &src_rel, WHITEOUT_SUFFIX);
                    if let Err(e) = Self::write_marker(&wh_path) {
                        reply.error(e);
                        return;
                    }
                }
                inner.inodes.rename_subtree(&src_rel, &dst_rel);
                reply.ok();
                return;
            }

            // Directory rename: materialize any lower-only entries into upper/src_rel before moving.
            let mut moved_lower_files = Vec::new();
            if let Err(e) =
                self.materialize_lower_dir_tree(&mut inner, &src_rel, &mut moved_lower_files)
            {
                reply.error(e);
                return;
            }

            let mut all_src_lower_files = BTreeSet::new();
            if !lower_layers_blocked_at_dir(&self.cfg, &src_parent) {
                collect_lower_file_paths(&self.cfg, &src_rel, &mut all_src_lower_files);
            }

            let upper_dst = self.upper_path(&dst_rel);
            if std::fs::symlink_metadata(&upper_dst).is_ok()
                && let Err(e) = std::fs::remove_dir(&upper_dst)
            {
                reply.error(errno_from_io(&e));
                return;
            }

            let dst_wh = marker_path(&self.cfg.whiteouts_dir(), &dst_rel, WHITEOUT_SUFFIX);
            let dst_had_wh = dst_wh.exists();
            let dst_in_lower = resolve_in_lower_layers(&self.cfg, &dst_rel).is_some();
            let _ = Self::remove_marker_if_exists(&dst_wh);
            if dst_had_wh || dst_in_lower {
                let dst_opaque = marker_path(&self.cfg.opaque_dir(), &dst_rel, OPAQUE_SUFFIX);
                if let Err(e) = Self::write_marker(&dst_opaque) {
                    reply.error(e);
                    return;
                }
            }

            let upper_src = self.upper_path(&src_rel);
            if let Err(e) = std::fs::rename(&upper_src, &upper_dst) {
                reply.error(errno_from_io(&e));
                return;
            }

            // Clean up any stale whiteout markers under dst_rel for entries that were moved in.
            if let Ok(new_entries) = WalkDir::new(&upper_dst)
                .follow_links(false)
                .into_iter()
                .collect::<std::result::Result<Vec<_>, _>>()
            {
                for ent in new_entries {
                    if let Ok(tail) = ent.path().strip_prefix(&upper_dst)
                        && !tail.as_os_str().is_empty()
                    {
                        let moved_dst = dst_rel.join(tail);
                        let _ = Self::remove_marker_if_exists(&marker_path(
                            &self.cfg.whiteouts_dir(),
                            &moved_dst,
                            WHITEOUT_SUFFIX,
                        ));
                    }
                }
            }

            Self::remove_subtree_markers(&self.cfg.opaque_dir(), &src_rel, OPAQUE_SUFFIX);
            if resolve_in_lower_layers(&self.cfg, &src_rel).is_some()
                && !lower_layers_blocked_at_dir(&self.cfg, &src_parent)
            {
                let _ = Self::write_marker(&marker_path(
                    &self.cfg.whiteouts_dir(),
                    &src_rel,
                    WHITEOUT_SUFFIX,
                ));
                for lower_file in all_src_lower_files {
                    let _ = Self::write_marker(&marker_path(
                        &self.cfg.whiteouts_dir(),
                        &lower_file,
                        WHITEOUT_SUFFIX,
                    ));
                }
            }

            inner.inodes.rename_subtree(&src_rel, &dst_rel);
            reply.ok();
        }

        fn link(
            &self,
            _req: &Request,
            ino: INodeNo,
            newparent: INodeNo,
            newname: &OsStr,
            reply: ReplyEntry,
        ) {
            if !is_valid_child_name(newname) {
                reply.error(Errno::EINVAL);
                return;
            }
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            let Ok(old_rel) = inner.inodes.resolve_ino_path(ino) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let Ok(new_parent_rel) = inner.inodes.resolve_ino_path(newparent) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let new_rel = if new_parent_rel.as_os_str().is_empty() {
                PathBuf::from(newname)
            } else {
                new_parent_rel.join(newname)
            };
            if resolve_rel_path(&self.cfg, &new_rel).is_some() {
                reply.error(Errno::EEXIST);
                return;
            }
            let Some(old_resolved) = resolve_rel_path(&self.cfg, &old_rel) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let Ok(old_meta) = std::fs::symlink_metadata(&old_resolved.host_path) else {
                reply.error(Errno::ENOENT);
                return;
            };
            if old_meta.file_type().is_dir() {
                reply.error(Errno::EPERM);
                return;
            }

            if let Err(e) = self.copy_up_entry(&mut inner, &old_rel, false) {
                reply.error(e);
                return;
            }
            if let Err(e) = self.ensure_upper_dir(&new_parent_rel) {
                reply.error(e);
                return;
            }

            let _ = Self::remove_marker_if_exists(&marker_path(
                &self.cfg.whiteouts_dir(),
                &new_rel,
                WHITEOUT_SUFFIX,
            ));

            let upper_old = self.upper_path(&old_rel);
            let upper_new = self.upper_path(&new_rel);
            if let Err(e) = std::fs::hard_link(&upper_old, &upper_new) {
                reply.error(errno_from_io(&e));
                return;
            }
            if let Err(e) = inner.inodes.add_alias(ino, new_rel, true) {
                reply.error(e);
                return;
            }
            match std::fs::symlink_metadata(&upper_new) {
                Ok(meta) => reply.entry(&ZERO_TTL, &metadata_to_file_attr(ino, &meta), Generation(0)),
                Err(e) => reply.error(errno_from_io(&e)),
            }
        }

        fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            let Ok(rel) = inner.inodes.resolve_ino_path(ino) else {
                reply.error(Errno::ENOENT);
                return;
            };

            let acc = flags.acc_mode();
            let wants_trunc = (flags.0 & libc::O_TRUNC) != 0;
            let wants_write =
                matches!(acc, OpenAccMode::O_WRONLY | OpenAccMode::O_RDWR) || wants_trunc;

            let file = if wants_write {
                if let Err(e) = self.copy_up_entry(&mut inner, &rel, wants_trunc) {
                    reply.error(e);
                    return;
                }
                let upper_path = self.upper_path(&rel);
                match OpenOptions::new().read(true).write(true).open(&upper_path) {
                    Ok(f) => f,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::PermissionDenied
                            && acc == OpenAccMode::O_WRONLY =>
                    {
                        match OpenOptions::new().write(true).open(&upper_path) {
                            Ok(f) => f,
                            Err(err) => {
                                reply.error(errno_from_io(&err));
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        reply.error(errno_from_io(&e));
                        return;
                    }
                }
            } else {
                let Some(resolved) = resolve_rel_path(&self.cfg, &rel) else {
                    reply.error(Errno::ENOENT);
                    return;
                };
                match OpenOptions::new().read(true).open(&resolved.host_path) {
                    Ok(f) => f,
                    Err(e) => {
                        reply.error(errno_from_io(&e));
                        return;
                    }
                }
            };

            let fh = FileHandle(inner.next_fh);
            inner.next_fh += 1;
            inner.open_files.insert(
                fh,
                OpenFile {
                    ino,
                    file: Arc::new(file),
                    writable: wants_write,
                },
            );
            // Keep FopenFlags empty so host-side writes/undo updates are never stale-cached.
            reply.opened(fh, FopenFlags::empty());
        }

        fn read(
            &self,
            _req: &Request,
            ino: INodeNo,
            fh: FileHandle,
            offset: u64,
            size: u32,
            _flags: OpenFlags,
            _lock_owner: Option<LockOwner>,
            reply: ReplyData,
        ) {
            let file = {
                let inner = self.state.lock().expect("vfsd lock poisoned");
                if let Some(open) = inner.open_files.get(&fh) {
                    Arc::clone(&open.file)
                } else {
                    let Ok(rel) = inner.inodes.resolve_ino_path(ino) else {
                        reply.error(Errno::EBADF);
                        return;
                    };
                    let Some(resolved) = resolve_rel_path(&self.cfg, &rel) else {
                        reply.error(Errno::ENOENT);
                        return;
                    };
                    match File::open(&resolved.host_path) {
                        Ok(f) => Arc::new(f),
                        Err(e) => {
                            reply.error(errno_from_io(&e));
                            return;
                        }
                    }
                }
            };

            let mut buf = vec![0u8; size as usize];
            let mut filled = 0usize;
            while filled < buf.len() {
                match file.read_at(&mut buf[filled..], offset + filled as u64) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        reply.error(errno_from_io(&e));
                        return;
                    }
                }
            }
            reply.data(&buf[..filled]);
        }

        fn write(
            &self,
            _req: &Request,
            _ino: INodeNo,
            fh: FileHandle,
            offset: u64,
            data: &[u8],
            _write_flags: WriteFlags,
            _flags: OpenFlags,
            _lock_owner: Option<LockOwner>,
            reply: ReplyWrite,
        ) {
            let file = {
                let inner = self.state.lock().expect("vfsd lock poisoned");
                let Some(open) = inner.open_files.get(&fh) else {
                    reply.error(Errno::EBADF);
                    return;
                };
                if !open.writable {
                    reply.error(Errno::EBADF);
                    return;
                }
                Arc::clone(&open.file)
            };

            // Direct immediate pwrite to host upper/ fd — zero userspace write buffering.
            if let Err(e) = file.write_all_at(data, offset) {
                reply.error(errno_from_io(&e));
                return;
            }
            reply.written(data.len() as u32);
        }

        fn flush(
            &self,
            _req: &Request,
            _ino: INodeNo,
            _fh: FileHandle,
            _lock_owner: LockOwner,
            reply: ReplyEmpty,
        ) {
            reply.ok();
        }

        fn release(
            &self,
            _req: &Request,
            _ino: INodeNo,
            fh: FileHandle,
            _flags: OpenFlags,
            _lock_owner: Option<LockOwner>,
            _flush: bool,
            reply: ReplyEmpty,
        ) {
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            inner.open_files.remove(&fh);
            reply.ok();
        }

        fn fsync(
            &self,
            _req: &Request,
            _ino: INodeNo,
            fh: FileHandle,
            datasync: bool,
            reply: ReplyEmpty,
        ) {
            let file = {
                let inner = self.state.lock().expect("vfsd lock poisoned");
                let Some(open) = inner.open_files.get(&fh) else {
                    reply.error(Errno::EBADF);
                    return;
                };
                Arc::clone(&open.file)
            };
            let res = if datasync {
                file.sync_data()
            } else {
                file.sync_all()
            };
            match res {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(errno_from_io(&e)),
            }
        }

        fn create(
            &self,
            _req: &Request,
            parent: INodeNo,
            name: &OsStr,
            mode: u32,
            umask: u32,
            flags: i32,
            reply: ReplyCreate,
        ) {
            if !is_valid_child_name(name) {
                reply.error(Errno::EINVAL);
                return;
            }
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            let Ok(parent_rel) = inner.inodes.resolve_ino_path(parent) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let rel = if parent_rel.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                parent_rel.join(name)
            };

            let excl = (flags & libc::O_EXCL) != 0;
            let trunc = (flags & libc::O_TRUNC) != 0;
            let existed = resolve_rel_path(&self.cfg, &rel).is_some();
            if excl && existed {
                reply.error(Errno::EEXIST);
                return;
            }
            if !excl
                && !trunc
                && existed
                && let Err(e) = self.copy_up_entry(&mut inner, &rel, false)
            {
                reply.error(e);
                return;
            }
            if let Err(e) = self.ensure_upper_dir(&parent_rel) {
                reply.error(e);
                return;
            }

            let wh_path = marker_path(&self.cfg.whiteouts_dir(), &rel, WHITEOUT_SUFFIX);
            let _ = Self::remove_marker_if_exists(&wh_path);

            let eff_mode = (mode & !umask) & 0o7777;
            let upper_path = self.upper_path(&rel);

            let file = match OpenOptions::new()
                .create(true)
                .create_new(excl)
                .read(true)
                .write(true)
                .truncate(trunc)
                .mode(0o600)
                .open(&upper_path)
            {
                Ok(f) => f,
                Err(e) => {
                    reply.error(errno_from_io(&e));
                    return;
                }
            };

            // POSIX: O_CREAT must only set permissions when creating a brand-new file;
            // opening an existing executable file with O_CREAT|O_WRONLY must preserve its mode.
            if !existed {
                let _ = std::fs::set_permissions(&upper_path, Permissions::from_mode(eff_mode));
            }

            let attr = match self.stat_resolved(&mut inner, &rel, true) {
                Ok(a) => a,
                Err(e) => {
                    reply.error(e);
                    return;
                }
            };
            let fh = FileHandle(inner.next_fh);
            inner.next_fh += 1;
            inner.open_files.insert(
                fh,
                OpenFile {
                    ino: attr.ino,
                    file: Arc::new(file),
                    writable: true,
                },
            );
            reply.created(&ZERO_TTL, &attr, Generation(0), fh, FopenFlags::empty());
        }

        fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            let Ok(rel) = inner.inodes.resolve_ino_path(ino) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let Some(resolved) = resolve_rel_path(&self.cfg, &rel) else {
                reply.error(Errno::ENOENT);
                return;
            };
            match std::fs::symlink_metadata(&resolved.host_path) {
                Ok(meta) if meta.file_type().is_dir() => {
                    let fh = FileHandle(inner.next_fh);
                    inner.next_fh += 1;
                    inner.open_dirs.insert(fh, None);
                    reply.opened(fh, FopenFlags::empty());
                }
                Ok(_) => reply.error(Errno::ENOTDIR),
                Err(e) => reply.error(errno_from_io(&e)),
            }
        }

        fn readdir(
            &self,
            _req: &Request,
            ino: INodeNo,
            fh: FileHandle,
            offset: u64,
            mut reply: ReplyDirectory,
        ) {
            let mut inner = self.state.lock().expect("vfsd lock poisoned");

            // Reuse the directory snapshot across paginated `readdir` calls (`offset > 0`) on
            // the same directory handle so large directories enumerate in O(M) instead of O(M^2).
            // Re-reading at `offset == 0` (fresh opendir / rewinddir) always refreshes from disk
            // so host-side mutations (`oz undo`) remain immediately visible.
            let snapshot: DirSnapshot = if offset > 0
                && let Some(Some(cached)) = inner.open_dirs.get(&fh)
            {
                Arc::clone(cached)
            } else {
                let Ok(rel_dir) = inner.inodes.resolve_ino_path(ino) else {
                    reply.error(Errno::ENOENT);
                    return;
                };
                let Some(resolved) = resolve_rel_path(&self.cfg, &rel_dir) else {
                    reply.error(Errno::ENOENT);
                    return;
                };
                match std::fs::symlink_metadata(&resolved.host_path) {
                    Ok(meta) if meta.file_type().is_dir() => {}
                    Ok(_) => {
                        reply.error(Errno::ENOTDIR);
                        return;
                    }
                    Err(e) => {
                        reply.error(errno_from_io(&e));
                        return;
                    }
                }

                let merged = match list_merged_dir_entries(&self.cfg, &rel_dir) {
                    Ok(m) => m,
                    Err(e) => {
                        reply.error(errno_from_io(&e));
                        return;
                    }
                };

                let parent_ino = if rel_dir.as_os_str().is_empty() {
                    INodeNo::ROOT
                } else {
                    let parent_rel = rel_dir.parent().map(Path::to_path_buf).unwrap_or_default();
                    if parent_rel.as_os_str().is_empty() {
                        INodeNo::ROOT
                    } else {
                        inner.inodes.lookup_or_insert(&parent_rel, true, false)
                    }
                };

                let mut all_entries: Vec<(INodeNo, FileType, OsString)> =
                    Vec::with_capacity(merged.len() + 2);
                all_entries.push((ino, FileType::Directory, OsString::from(".")));
                all_entries.push((parent_ino, FileType::Directory, OsString::from("..")));

                for item in merged {
                    let child_rel = if rel_dir.as_os_str().is_empty() {
                        PathBuf::from(&item.name)
                    } else {
                        rel_dir.join(&item.name)
                    };
                    let is_dir = item.file_type.is_dir();
                    let child_ino = inner.inodes.lookup_or_insert(&child_rel, is_dir, false);
                    let kind = FileType::from_std(item.file_type).unwrap_or(FileType::RegularFile);
                    all_entries.push((child_ino, kind, item.name));
                }

                let arc = Arc::new(all_entries);
                if let Some(slot) = inner.open_dirs.get_mut(&fh) {
                    *slot = Some(Arc::clone(&arc));
                }
                arc
            };
            drop(inner);

            for (idx, (entry_ino, kind, name)) in snapshot.iter().enumerate().skip(offset as usize) {
                let next_offset = (idx + 1) as u64;
                if reply.add(*entry_ino, next_offset, *kind, name) {
                    break;
                }
            }
            reply.ok();
        }

        fn releasedir(
            &self,
            _req: &Request,
            _ino: INodeNo,
            fh: FileHandle,
            _flags: OpenFlags,
            reply: ReplyEmpty,
        ) {
            let mut inner = self.state.lock().expect("vfsd lock poisoned");
            inner.open_dirs.remove(&fh);
            reply.ok();
        }

        fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
            let Ok(c_path) = CString::new(self.cfg.upper_dir.as_os_str().as_bytes()) else {
                reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096);
                return;
            };
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
            if rc != 0 {
                reply.error(errno_from_io(&std::io::Error::last_os_error()));
                return;
            }
            reply.statfs(
                stat.f_blocks,
                stat.f_bfree,
                stat.f_bavail,
                stat.f_files,
                stat.f_ffree,
                stat.f_bsize as u32,
                stat.f_namemax as u32,
                stat.f_frsize as u32,
            );
        }

        fn access(&self, req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
            let inner = self.state.lock().expect("vfsd lock poisoned");
            let Ok(rel) = inner.inodes.resolve_ino_path(ino) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let Some(resolved) = resolve_rel_path(&self.cfg, &rel) else {
                reply.error(Errno::ENOENT);
                return;
            };
            drop(inner);
            let Ok(meta) = std::fs::symlink_metadata(&resolved.host_path) else {
                reply.error(Errno::ENOENT);
                return;
            };
            if mask == AccessFlags::F_OK || req.uid() == 0 {
                reply.ok();
                return;
            }
            let mode = meta.mode();
            let shift = if req.uid() == meta.uid() {
                6
            } else if req.gid() == meta.gid() {
                3
            } else {
                0
            };
            let bits = (mode >> shift) & 0o7;
            if mask.contains(AccessFlags::R_OK) && (bits & 0o4) == 0 {
                reply.error(Errno::EACCES);
                return;
            }
            if mask.contains(AccessFlags::W_OK) && (bits & 0o2) == 0 {
                reply.error(Errno::EACCES);
                return;
            }
            if mask.contains(AccessFlags::X_OK) && (bits & 0o1) == 0 {
                reply.error(Errno::EACCES);
                return;
            }
            reply.ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_cfg(dir: &Path) -> OverlayMountConfig {
        let cfg = OverlayMountConfig {
            workspace: "ws".to_string(),
            mount_point: dir.join("mnt"),
            upper_dir: dir.join("upper"),
            seed_dir: Some(dir.join("seed")),
            baseline_dir: Some(dir.join("baseline")),
            meta_dir: dir.join("meta"),
            allow_other: false,
            writeback_cache: true,
        };
        std::fs::create_dir_all(cfg.seed_dir.as_ref().expect("seed")).expect("mkdir seed");
        std::fs::create_dir_all(cfg.baseline_dir.as_ref().expect("base")).expect("mkdir base");
        cfg.ensure_directories().expect("ensure dirs");
        cfg
    }

    #[test]
    fn layer_resolution_respects_whiteouts_opaque_and_seed_whiteouts() {
        let dir = TempDir::new().expect("tempdir");
        let cfg = test_cfg(dir.path());

        let base = cfg.baseline_dir.as_ref().expect("base");
        let seed = cfg.seed_dir.as_ref().expect("seed");

        std::fs::create_dir_all(base.join("pkg")).expect("mkdir pkg");
        std::fs::write(base.join("pkg/base_only.txt"), b"b").expect("write");
        std::fs::write(base.join("pkg/shared.txt"), b"from_base").expect("write");
        std::fs::write(base.join("pkg/deleted_in_seed.txt"), b"gone_seed").expect("write");

        std::fs::create_dir_all(seed.join("pkg")).expect("mkdir seed pkg");
        std::fs::write(seed.join("pkg/shared.txt"), b"from_seed").expect("write");

        // seed_whiteout hides deleted_in_seed.txt from baseline without adding to user whiteouts.
        let swh = marker_path(
            &cfg.seed_whiteouts_dir(),
            Path::new("pkg/deleted_in_seed.txt"),
            WHITEOUT_SUFFIX,
        );
        std::fs::create_dir_all(swh.parent().expect("parent")).expect("mkdir");
        std::fs::write(&swh, b"").expect("write swh");

        assert_eq!(
            resolve_rel_path(&cfg, Path::new("pkg/shared.txt"))
                .expect("shared")
                .layer,
            LayerKind::Seed
        );
        assert_eq!(
            resolve_rel_path(&cfg, Path::new("pkg/base_only.txt"))
                .expect("base_only")
                .layer,
            LayerKind::Baseline
        );
        assert!(resolve_rel_path(&cfg, Path::new("pkg/deleted_in_seed.txt")).is_none());

        // User whiteout hides base_only.txt; upper_dir file shadows seed.
        let wh = marker_path(
            &cfg.whiteouts_dir(),
            Path::new("pkg/base_only.txt"),
            WHITEOUT_SUFFIX,
        );
        std::fs::create_dir_all(wh.parent().expect("parent")).expect("mkdir");
        std::fs::write(&wh, b"").expect("write wh");
        std::fs::create_dir_all(cfg.upper_dir.join("pkg")).expect("mkdir upper pkg");
        std::fs::write(cfg.upper_dir.join("pkg/shared.txt"), b"from_upper").expect("write upper");

        assert!(resolve_rel_path(&cfg, Path::new("pkg/base_only.txt")).is_none());
        assert_eq!(
            resolve_rel_path(&cfg, Path::new("pkg/shared.txt"))
                .expect("upper")
                .layer,
            LayerKind::Upper
        );

        let merged = list_merged_dir_entries(&cfg, Path::new("pkg")).expect("readdir");
        let names: Vec<String> = merged
            .iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["shared.txt"]);

        // Opaque dir marker hides all lower files under pkg/.
        std::fs::remove_file(cfg.upper_dir.join("pkg/shared.txt")).expect("rm upper");
        std::fs::remove_file(&wh).expect("rm wh");
        let opq = marker_path(&cfg.opaque_dir(), Path::new("pkg"), OPAQUE_SUFFIX);
        std::fs::write(&opq, b"").expect("write opaque");
        assert!(resolve_rel_path(&cfg, Path::new("pkg/shared.txt")).is_none());
        assert!(resolve_rel_path(&cfg, Path::new("pkg/base_only.txt")).is_none());
        assert!(
            list_merged_dir_entries(&cfg, Path::new("pkg"))
                .expect("readdir opaque")
                .is_empty()
        );
    }

    #[test]
    fn dirty_scan_reports_added_modified_and_deleted_in_o_dirty() {
        let dir = TempDir::new().expect("tempdir");
        let cfg = test_cfg(dir.path());
        let base = cfg.baseline_dir.as_ref().expect("base");

        std::fs::write(base.join("tracked.txt"), b"orig").expect("write");
        std::fs::write(base.join("removed.txt"), b"bye").expect("write");
        std::fs::create_dir_all(base.join("old_dir")).expect("mkdir");
        std::fs::write(base.join("old_dir/inner.txt"), b"x").expect("write");

        std::fs::write(cfg.upper_dir.join("tracked.txt"), b"edited").expect("write");
        std::fs::write(cfg.upper_dir.join("brand_new.txt"), b"hi").expect("write");
        std::fs::write(
            marker_path(&cfg.whiteouts_dir(), Path::new("removed.txt"), WHITEOUT_SUFFIX),
            b"",
        )
        .expect("wh");
        std::fs::create_dir_all(cfg.whiteouts_dir().join("old_dir")).expect("mkdir");
        std::fs::write(
            marker_path(
                &cfg.whiteouts_dir(),
                Path::new("old_dir/inner.txt"),
                WHITEOUT_SUFFIX,
            ),
            b"",
        )
        .expect("wh inner");
        std::fs::write(
            marker_path(&cfg.whiteouts_dir(), Path::new("old_dir"), WHITEOUT_SUFFIX),
            b"",
        )
        .expect("wh dir");

        let dirty = scan_dirty_paths(&cfg).expect("scan dirty");
        let summary: Vec<(String, ChangeKind)> =
            dirty.into_iter().map(|d| (d.path, d.kind)).collect();
        assert_eq!(
            summary,
            vec![
                ("brand_new.txt".to_string(), ChangeKind::Added),
                ("old_dir/inner.txt".to_string(), ChangeKind::Deleted),
                ("removed.txt".to_string(), ChangeKind::Deleted),
                ("tracked.txt".to_string(), ChangeKind::Modified),
            ]
        );
    }

    #[test]
    fn list_merged_dir_entries_handles_large_dirs_spaces_unicode_and_whiteouts() {
        let dir = TempDir::new().expect("tempdir");
        let cfg = test_cfg(dir.path());
        let base = cfg.baseline_dir.as_ref().expect("base");
        let seed = cfg.seed_dir.as_ref().expect("seed");

        let sub_base = base.join("big dir");
        std::fs::create_dir_all(&sub_base).expect("mkdir big dir");
        for i in 0..500 {
            std::fs::write(sub_base.join(format!("item_{i:04}.txt")), b"v").expect("write");
        }
        std::fs::write(sub_base.join("café report.txt"), b"unicode").expect("write unicode");
        std::fs::write(sub_base.join("seed_hidden.txt"), b"hidden").expect("write seed_hidden");

        let sub_seed = seed.join("big dir");
        std::fs::create_dir_all(&sub_seed).expect("mkdir seed big dir");
        std::fs::write(sub_seed.join("item_0001.txt"), b"seed-override").expect("write seed");

        // Whiteout 3 entries in user whiteouts and 1 entry in seed_whiteouts.
        std::fs::create_dir_all(cfg.whiteouts_dir().join("big dir")).expect("mkdir wh");
        for idx in [0, 10, 499] {
            std::fs::write(
                marker_path(
                    &cfg.whiteouts_dir(),
                    &PathBuf::from(format!("big dir/item_{idx:04}.txt")),
                    WHITEOUT_SUFFIX,
                ),
                b"",
            )
            .expect("write wh");
        }
        std::fs::create_dir_all(cfg.seed_whiteouts_dir().join("big dir")).expect("mkdir swh");
        std::fs::write(
            marker_path(
                &cfg.seed_whiteouts_dir(),
                Path::new("big dir/seed_hidden.txt"),
                WHITEOUT_SUFFIX,
            ),
            b"",
        )
        .expect("write swh");

        let merged = list_merged_dir_entries(&cfg, Path::new("big dir")).expect("merge");
        // 500 + café + seed_hidden - 3 user whiteouts - 1 seed whiteout = 498
        assert_eq!(merged.len(), 498);
        assert!(merged.iter().any(|e| e.name == "café report.txt"));
        assert!(!merged.iter().any(|e| e.name == "item_0000.txt"));
        assert!(!merged.iter().any(|e| e.name == "item_0010.txt"));
        assert!(!merged.iter().any(|e| e.name == "item_0499.txt"));
        assert!(!merged.iter().any(|e| e.name == "seed_hidden.txt"));
        let overridden = merged
            .iter()
            .find(|e| e.name == "item_0001.txt")
            .expect("item_0001");
        assert_eq!(overridden.layer, LayerKind::Seed);
    }
}
