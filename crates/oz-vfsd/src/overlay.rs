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
#[cfg(unix)]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::FileType as StdFileType;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

/// Marker suffix for deleted entries inside `meta_dir/whiteouts/` and `meta_dir/seed_whiteouts/`.
pub const WHITEOUT_SUFFIX: &str = ".oz_wh";
/// Marker suffix for replaced/opaque directories inside `meta_dir/opaque/`.
pub const OPAQUE_SUFFIX: &str = ".oz_opaque";

/// Resolved configuration for a single `CoW` overlay mount.
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
pub(crate) fn resolve_seed_dir(spec: &MountSpec) -> Option<PathBuf> {
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
pub(crate) struct ResolvedEntry {
    pub layer: LayerKind,
    pub host_path: PathBuf,
}

/// Single merged directory entry returned by [`list_merged_dir_entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergedDirEntry {
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

#[cfg(target_os = "linux")]
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
pub(crate) fn resolve_rel_path(cfg: &OverlayMountConfig, rel: &Path) -> Option<ResolvedEntry> {
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
pub(crate) fn list_merged_dir_entries(
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

/// Unescapes `/proc/self/mountinfo` octal sequences (`\040`, `\011`, `\012`, `\134`, etc.).
#[cfg(unix)]
pub(crate) fn unescape_mountinfo_field(raw: &str) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let val =
                ((bytes[i + 1] - b'0') << 6) | ((bytes[i + 2] - b'0') << 3) | (bytes[i + 3] - b'0');
            out.push(val);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    OsString::from_vec(out)
}

/// Extracts active FUSE mountpoint paths from `/proc/self/mountinfo` text.
#[cfg(unix)]
pub(crate) fn parse_fuse_mountpoints_from_mountinfo(raw: &str) -> Vec<PathBuf> {
    let mut mounts = Vec::new();
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        let Some(mp_escaped) = fields.nth(4) else {
            continue;
        };
        let Some((_, after_sep)) = line.split_once(" - ") else {
            continue;
        };
        let fs_type = after_sep.split_whitespace().next().unwrap_or("");
        if fs_type == "fuse" || fs_type.starts_with("fuse.") {
            mounts.push(PathBuf::from(unescape_mountinfo_field(mp_escaped)));
        }
    }
    mounts
}

/// Returns `true` when `mount_point` appears as an active FUSE mount in `/proc/self/mountinfo`.
#[cfg(unix)]
#[must_use]
pub fn is_fuse_mount_in_proc_mountinfo(mount_point: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let canon_target = std::fs::canonicalize(mount_point).ok();
    parse_fuse_mountpoints_from_mountinfo(&raw)
        .into_iter()
        .any(|mp| mp == mount_point || canon_target.as_ref().is_some_and(|c| &mp == c))
}

/// Invokes `fusermount3 -u -z` (or `fusermount -u -z`) on `mount_point`.
/// Best-effort cleanup helper used for `ENOTCONN` recovery and forced unmounts.
pub fn lazy_unmount_fuse(mount_point: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::process::{Command, Stdio};

        let mut last_err: Option<String> = None;
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
                        || stderr.contains("is not a mount point")
                        || stderr.contains("No such file or directory")
                        || stderr.contains("Invalid argument")
                    {
                        return Ok(());
                    }
                    last_err = Some(stderr.trim().to_string());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(OzError::Io(e)),
            }
        }
        if let Some(err_msg) = last_err {
            let still_attached = std::fs::symlink_metadata(mount_point)
                .err()
                .is_some_and(|e| oz_core::is_enotconn(&e))
                || is_fuse_mount_in_proc_mountinfo(mount_point);
            if still_attached {
                return Err(OzError::Other(format!(
                    "fusermount unmount failed for {}: {err_msg}",
                    mount_point.display()
                )));
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

/// Mounts the `CoW` overlay described by `spec` over `/dev/fuse`.
#[cfg(target_os = "linux")]
pub fn mount_overlay(spec: &MountSpec) -> Result<MountedOverlay> {
    let cfg = OverlayMountConfig::from_spec(spec);
    let stale_enotconn = std::fs::symlink_metadata(&cfg.mount_point)
        .err()
        .is_some_and(|err| oz_core::is_enotconn(&err));
    if stale_enotconn || is_fuse_mount_in_proc_mountinfo(&cfg.mount_point) {
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
    use super::{
        BTreeMap, Component, OPAQUE_SUFFIX, OsStr, OsString, OverlayMountConfig, Path, PathBuf,
        WHITEOUT_SUFFIX, WalkDir, ancestors_inclusive, is_valid_child_name,
        list_merged_dir_entries, lower_layers_blocked_at_dir, marker_path, resolve_in_lower_layers,
        resolve_rel_path,
    };
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
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
        next_id: u64,
        entries: HashMap<INodeNo, InodeEntry>,
        by_path: BTreeMap<PathBuf, INodeNo>,
    }

    impl InodeTable {
        fn new() -> Self {
            let mut entries = HashMap::new();
            let mut root_paths = BTreeSet::new();
            root_paths.insert(PathBuf::new());
            entries.insert(
                INodeNo::ROOT,
                InodeEntry {
                    paths: root_paths,
                    lookup_count: u64::MAX,
                    unlinked: false,
                    is_dir: true,
                },
            );
            let mut by_path = BTreeMap::new();
            by_path.insert(PathBuf::new(), INodeNo::ROOT);
            Self {
                next_id: 2,
                entries,
                by_path,
            }
        }

        fn resolve_ino_path(&self, ino: INodeNo) -> std::result::Result<PathBuf, Errno> {
            let entry = self.entries.get(&ino).ok_or(Errno::ENOENT)?;
            if entry.unlinked {
                return Err(Errno::ENOENT);
            }
            entry.paths.iter().next().cloned().ok_or(Errno::ENOENT)
        }

        fn lookup_or_insert(&mut self, rel: &Path, is_dir: bool, inc_lookup: bool) -> INodeNo {
            if let Some(&ino) = self.by_path.get(rel)
                && let Some(entry) = self.entries.get_mut(&ino)
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
                self.by_path.remove(rel);
            }

            let ino = INodeNo(self.next_id);
            self.next_id += 1;
            let mut paths = BTreeSet::new();
            paths.insert(rel.to_path_buf());
            self.entries.insert(
                ino,
                InodeEntry {
                    paths,
                    lookup_count: u64::from(inc_lookup),
                    unlinked: false,
                    is_dir,
                },
            );
            self.by_path.insert(rel.to_path_buf(), ino);
            ino
        }

        fn add_alias(
            &mut self,
            ino: INodeNo,
            new_rel: PathBuf,
            inc_lookup: bool,
        ) -> std::result::Result<(), Errno> {
            self.detach_path(&new_rel);
            let entry = self.entries.get_mut(&ino).ok_or(Errno::ENOENT)?;
            if entry.is_dir {
                return Err(Errno::EPERM);
            }
            entry.paths.insert(new_rel.clone());
            entry.unlinked = false;
            if inc_lookup {
                entry.lookup_count = entry.lookup_count.saturating_add(1);
            }
            self.by_path.insert(new_rel, ino);
            Ok(())
        }

        fn detach_path(&mut self, rel: &Path) {
            if let Some(ino) = self.by_path.remove(rel)
                && let Some(entry) = self.entries.get_mut(&ino)
            {
                entry.paths.remove(rel);
                if entry.paths.is_empty() {
                    entry.unlinked = true;
                    if entry.lookup_count == 0 {
                        self.entries.remove(&ino);
                    }
                }
            }
        }

        fn forget(&mut self, ino: INodeNo, nlookup: u64) {
            if ino == INodeNo::ROOT {
                return;
            }
            let Some(entry) = self.entries.get_mut(&ino) else {
                return;
            };
            entry.lookup_count = entry.lookup_count.saturating_sub(nlookup);
            if entry.lookup_count == 0 {
                let paths = std::mem::take(&mut entry.paths);
                for p in paths {
                    if self.by_path.get(&p) == Some(&ino) {
                        self.by_path.remove(&p);
                    }
                }
                self.entries.remove(&ino);
            }
        }

        fn rename_subtree(&mut self, src_rel: &Path, dst_rel: &Path) {
            let dst_targets: Vec<PathBuf> = self
                .by_path
                .keys()
                .filter(|p| *p == dst_rel || p.starts_with(dst_rel))
                .cloned()
                .collect();
            for p in dst_targets {
                self.detach_path(&p);
            }

            let src_entries: Vec<(PathBuf, INodeNo)> = self
                .by_path
                .iter()
                .filter(|(p, _)| *p == src_rel || p.starts_with(src_rel))
                .map(|(p, &ino)| (p.clone(), ino))
                .collect();

            for (old_p, ino) in src_entries {
                self.by_path.remove(&old_p);
                let new_p = if old_p == src_rel {
                    dst_rel.to_path_buf()
                } else if let Ok(tail) = old_p.strip_prefix(src_rel) {
                    dst_rel.join(tail)
                } else {
                    continue;
                };
                self.by_path.insert(new_p.clone(), ino);
                if let Some(entry) = self.entries.get_mut(&ino) {
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

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum PathLockMode {
        Shared,
        Exclusive,
    }

    #[derive(Debug, Default)]
    struct PathSlot {
        readers: usize,
        writer: bool,
    }

    /// Hierarchical per-path lock table separating slow blocking filesystem I/O (copy-up,
    /// directory tree materialization, rename, rmdir) from the short-lived in-memory
    /// [`InnerState`] mutex.
    ///
    /// Strict lock ordering (acyclic — deadlock-free):
    /// 1. Level 1: [`PathLockGuard`] acquired atomically (all-or-nothing) across proper
    ///    ancestors (`Shared`) and target path(s) (`Exclusive`). Callers must never hold
    ///    `OzoneOverlayFs::state` or another [`PathLockGuard`] when acquiring.
    /// 2. Level 2: `OzoneOverlayFs::state` (`Mutex<InnerState>`) for fast, pure in-memory
    ///    `InodeTable` / `open_files` / `open_dirs` operations only.
    #[derive(Debug, Default)]
    struct PathLockTable {
        slots: Mutex<HashMap<PathBuf, PathSlot>>,
        cvar: Condvar,
    }

    pub(super) struct PathLockGuard<'a> {
        table: &'a PathLockTable,
        acquired: Vec<(PathBuf, PathLockMode)>,
    }

    impl Drop for PathLockGuard<'_> {
        fn drop(&mut self) {
            let mut slots = self
                .table
                .slots
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for (path, mode) in &self.acquired {
                if let Some(slot) = slots.get_mut(path) {
                    match mode {
                        PathLockMode::Shared => {
                            slot.readers = slot.readers.saturating_sub(1);
                        }
                        PathLockMode::Exclusive => {
                            slot.writer = false;
                        }
                    }
                    if slot.readers == 0 && !slot.writer {
                        slots.remove(path);
                    }
                }
            }
            self.table.cvar.notify_all();
        }
    }

    impl PathLockTable {
        fn add_path_requests(reqs: &mut BTreeMap<PathBuf, PathLockMode>, target: &Path) {
            if target.as_os_str().is_empty() {
                reqs.insert(PathBuf::new(), PathLockMode::Exclusive);
                return;
            }
            let prefixes = ancestors_inclusive(target);
            if let Some((last, proper_ancestors)) = prefixes.split_last() {
                for anc in proper_ancestors {
                    reqs.entry(anc.clone()).or_insert(PathLockMode::Shared);
                }
                // Exclusive always dominates Shared when two locked paths overlap an ancestor.
                reqs.insert(last.clone(), PathLockMode::Exclusive);
            }
        }

        fn add_path_shared_requests(reqs: &mut BTreeMap<PathBuf, PathLockMode>, target: &Path) {
            if target.as_os_str().is_empty() {
                reqs.entry(PathBuf::new()).or_insert(PathLockMode::Shared);
                return;
            }
            for anc in ancestors_inclusive(target) {
                reqs.entry(anc).or_insert(PathLockMode::Shared);
            }
        }

        fn lock_path(&self, target: &Path) -> PathLockGuard<'_> {
            let mut reqs = BTreeMap::new();
            Self::add_path_requests(&mut reqs, target);
            self.acquire(reqs)
        }

        fn lock_path_shared(&self, target: &Path) -> PathLockGuard<'_> {
            let mut reqs = BTreeMap::new();
            Self::add_path_shared_requests(&mut reqs, target);
            self.acquire(reqs)
        }

        fn lock_two_paths(&self, first: &Path, second: &Path) -> PathLockGuard<'_> {
            let mut reqs = BTreeMap::new();
            Self::add_path_requests(&mut reqs, first);
            Self::add_path_requests(&mut reqs, second);
            self.acquire(reqs)
        }

        fn acquire(&self, reqs: BTreeMap<PathBuf, PathLockMode>) -> PathLockGuard<'_> {
            let ordered: Vec<(PathBuf, PathLockMode)> = reqs.into_iter().collect();
            let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                let conflict = ordered.iter().any(|(path, mode)| {
                    slots.get(path).is_some_and(|slot| match mode {
                        PathLockMode::Shared => slot.writer,
                        PathLockMode::Exclusive => slot.writer || slot.readers > 0,
                    })
                });
                if !conflict {
                    for (path, mode) in &ordered {
                        let slot = slots.entry(path.clone()).or_default();
                        match mode {
                            PathLockMode::Shared => slot.readers += 1,
                            PathLockMode::Exclusive => slot.writer = true,
                        }
                    }
                    return PathLockGuard {
                        table: self,
                        acquired: ordered,
                    };
                }
                slots = self
                    .cvar
                    .wait(slots)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }

        #[cfg(test)]
        fn active_slot_count(&self) -> usize {
            self.slots
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len()
        }
    }

    #[derive(Debug)]
    struct InnerState {
        inodes: InodeTable,
        next_fh: u64,
        open_files: HashMap<FileHandle, OpenFile>,
        open_dirs: HashMap<FileHandle, Option<DirSnapshot>>,
    }

    struct LockedRenamePaths<'a> {
        src_parent: PathBuf,
        src_rel: PathBuf,
        dst_parent: PathBuf,
        dst_rel: PathBuf,
        _guard: PathLockGuard<'a>,
    }

    pub(crate) struct OzoneOverlayFs {
        cfg: OverlayMountConfig,
        path_locks: PathLockTable,
        state: Mutex<InnerState>,
        next_tmp: AtomicU64,
    }

    impl OzoneOverlayFs {
        pub(crate) fn new(cfg: OverlayMountConfig) -> Self {
            Self {
                cfg,
                path_locks: PathLockTable::default(),
                state: Mutex::new(InnerState {
                    inodes: InodeTable::new(),
                    next_fh: 1,
                    open_files: HashMap::new(),
                    open_dirs: HashMap::new(),
                }),
                next_tmp: AtomicU64::new(1),
            }
        }

        fn lock_state(&self) -> MutexGuard<'_, InnerState> {
            self.state.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn child_rel_path(parent_rel: &Path, name: &OsStr) -> PathBuf {
            if parent_rel.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                parent_rel.join(name)
            }
        }

        /// Resolves `ino` to its current virtual path and acquires its exclusive path lock,
        /// retrying if a concurrent rename moved `ino` while waiting on `path_locks`.
        fn lock_ino_path(
            &self,
            ino: INodeNo,
        ) -> std::result::Result<(PathBuf, PathLockGuard<'_>), Errno> {
            loop {
                let rel = self.lock_state().inodes.resolve_ino_path(ino)?;
                let guard = self.path_locks.lock_path(&rel);
                match self.lock_state().inodes.resolve_ino_path(ino) {
                    Ok(current) if current == rel => return Ok((rel, guard)),
                    Ok(_) => drop(guard),
                    Err(e) => return Err(e),
                }
            }
        }

        /// Resolves `parent` and acquires the exclusive path lock for `parent/name`,
        /// retrying if a concurrent rename moved `parent` while waiting on `path_locks`.
        fn lock_parent_child(
            &self,
            parent: INodeNo,
            name: &OsStr,
        ) -> std::result::Result<(PathBuf, PathBuf, PathLockGuard<'_>), Errno> {
            loop {
                let parent_rel = self.lock_state().inodes.resolve_ino_path(parent)?;
                let rel = Self::child_rel_path(&parent_rel, name);
                let guard = self.path_locks.lock_path(&rel);
                match self.lock_state().inodes.resolve_ino_path(parent) {
                    Ok(current) if current == parent_rel => return Ok((parent_rel, rel, guard)),
                    Ok(_) => drop(guard),
                    Err(e) => return Err(e),
                }
            }
        }

        /// Resolves `(parent, newparent)` and atomically acquires path locks on both
        /// `parent/name` and `newparent/newname`, retrying if either moved concurrently.
        /// Returns `Ok(None)` for a POSIX no-op self-rename (`src_rel == dst_rel`).
        fn lock_rename_paths(
            &self,
            parent: INodeNo,
            name: &OsStr,
            newparent: INodeNo,
            newname: &OsStr,
        ) -> std::result::Result<Option<LockedRenamePaths<'_>>, Errno> {
            loop {
                let (src_parent, dst_parent) = {
                    let inner = self.lock_state();
                    let src_parent = inner.inodes.resolve_ino_path(parent)?;
                    let dst_parent = inner.inodes.resolve_ino_path(newparent)?;
                    (src_parent, dst_parent)
                };
                let src_rel = Self::child_rel_path(&src_parent, name);
                let dst_rel = Self::child_rel_path(&dst_parent, newname);
                if src_rel == dst_rel {
                    return Ok(None);
                }
                if dst_rel.starts_with(&src_rel) {
                    return Err(Errno::EINVAL);
                }
                let guard = self.path_locks.lock_two_paths(&src_rel, &dst_rel);
                let valid = {
                    let inner = self.lock_state();
                    let cur_src = inner.inodes.resolve_ino_path(parent)?;
                    let cur_dst = inner.inodes.resolve_ino_path(newparent)?;
                    cur_src == src_parent && cur_dst == dst_parent
                };
                if valid {
                    return Ok(Some(LockedRenamePaths {
                        src_parent,
                        src_rel,
                        dst_parent,
                        dst_rel,
                        _guard: guard,
                    }));
                }
                drop(guard);
            }
        }

        /// Resolves `(ino, newparent)` and atomically acquires path locks on `old_rel` and
        /// `newparent/newname`, retrying if either moved concurrently.
        fn lock_link_paths(
            &self,
            ino: INodeNo,
            newparent: INodeNo,
            newname: &OsStr,
        ) -> std::result::Result<(PathBuf, PathBuf, PathBuf, PathLockGuard<'_>), Errno> {
            loop {
                let (old_rel, new_parent_rel) = {
                    let inner = self.lock_state();
                    let old_rel = inner.inodes.resolve_ino_path(ino)?;
                    let new_parent_rel = inner.inodes.resolve_ino_path(newparent)?;
                    (old_rel, new_parent_rel)
                };
                let new_rel = Self::child_rel_path(&new_parent_rel, newname);
                let guard = self.path_locks.lock_two_paths(&old_rel, &new_rel);
                let valid = {
                    let inner = self.lock_state();
                    let cur_old = inner.inodes.resolve_ino_path(ino)?;
                    let cur_parent = inner.inodes.resolve_ino_path(newparent)?;
                    cur_old == old_rel && cur_parent == new_parent_rel
                };
                if valid {
                    return Ok((old_rel, new_parent_rel, new_rel, guard));
                }
                drop(guard);
            }
        }

        /// Resolves `parent/name` (handling `.` and `..`) and acquires a shared path lock
        /// covering both `parent` and the resolved target path, retrying if `parent` moved
        /// concurrently while waiting on `path_locks`.
        fn lock_lookup_path(
            &self,
            parent: INodeNo,
            name: &OsStr,
        ) -> std::result::Result<(PathBuf, PathLockGuard<'_>), Errno> {
            loop {
                let parent_rel = self.lock_state().inodes.resolve_ino_path(parent)?;
                let (rel, lock_target) = if name == "." {
                    (parent_rel.clone(), parent_rel.clone())
                } else if name == ".." {
                    let target = parent_rel
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_default();
                    // Locking `parent_rel` as Shared covers `parent_rel` and all of its
                    // ancestors (including `target` when non-empty).
                    (target, parent_rel.clone())
                } else {
                    let child = Self::child_rel_path(&parent_rel, name);
                    (child.clone(), child)
                };
                let guard = self.path_locks.lock_path_shared(&lock_target);
                match self.lock_state().inodes.resolve_ino_path(parent) {
                    Ok(current) if current == parent_rel => return Ok((rel, guard)),
                    Ok(_) => drop(guard),
                    Err(e) => return Err(e),
                }
            }
        }

        /// Stats `ino` while holding a shared path lock (with inode-path revalidation on
        /// concurrent renames), falling back to an open file descriptor when unlinked.
        fn getattr_inner(
            &self,
            ino: INodeNo,
            fh: Option<FileHandle>,
        ) -> std::result::Result<FileAttr, Errno> {
            let locked = loop {
                let candidate = {
                    let inner = self.lock_state();
                    if let Ok(rel) = inner.inodes.resolve_ino_path(ino) {
                        Ok(rel)
                    } else {
                        let open_file = fh
                            .and_then(|handle| inner.open_files.get(&handle))
                            .or_else(|| inner.open_files.values().find(|o| o.ino == ino))
                            .map(|open| Arc::clone(&open.file));
                        Err(open_file)
                    }
                };
                let rel = match candidate {
                    Ok(rel) => rel,
                    Err(open_file) => break Err(open_file),
                };
                let guard = self.path_locks.lock_path_shared(&rel);
                let still_valid =
                    self.lock_state().inodes.resolve_ino_path(ino).as_ref() == Ok(&rel);
                if still_valid {
                    break Ok((rel, guard));
                }
                drop(guard);
            };

            match locked {
                Ok((rel, _path_guard)) => {
                    if let Ok(attr) = self.stat_resolved_unlocked(&rel, false) {
                        return Ok(attr);
                    }
                    let open_file = {
                        let inner = self.lock_state();
                        fh.and_then(|handle| inner.open_files.get(&handle))
                            .or_else(|| inner.open_files.values().find(|o| o.ino == ino))
                            .map(|open| Arc::clone(&open.file))
                    };
                    if let Some(file) = open_file
                        && let Ok(meta) = file.metadata()
                    {
                        return Ok(metadata_to_file_attr(ino, &meta));
                    }
                    Err(Errno::ENOENT)
                }
                Err(Some(file)) => file
                    .metadata()
                    .map(|meta| metadata_to_file_attr(ino, &meta))
                    .map_err(|_| Errno::ENOENT),
                Err(None) => Err(Errno::ENOENT),
            }
        }

        #[cfg(test)]
        pub(super) fn test_copy_up_entry(
            &self,
            rel: &Path,
            truncate_to_zero: bool,
        ) -> std::result::Result<(), Errno> {
            let _guard = self.path_locks.lock_path(rel);
            self.copy_up_entry_unlocked(rel, truncate_to_zero)
        }

        #[cfg(test)]
        pub(super) fn test_lock_path(&self, rel: &Path) -> PathLockGuard<'_> {
            self.path_locks.lock_path(rel)
        }

        #[cfg(test)]
        pub(super) fn test_lock_two_paths(&self, first: &Path, second: &Path) -> PathLockGuard<'_> {
            self.path_locks.lock_two_paths(first, second)
        }

        #[cfg(test)]
        pub(super) fn test_stat_resolved(
            &self,
            rel: &Path,
            inc_lookup: bool,
        ) -> std::result::Result<FileAttr, Errno> {
            let _guard = self.path_locks.lock_path_shared(rel);
            self.stat_resolved_unlocked(rel, inc_lookup)
        }

        #[cfg(test)]
        pub(super) fn test_getattr_ino(
            &self,
            ino: INodeNo,
        ) -> std::result::Result<FileAttr, Errno> {
            self.getattr_inner(ino, None)
        }

        #[cfg(test)]
        pub(super) fn test_rename_subtree_inodes(&self, src_rel: &Path, dst_rel: &Path) {
            self.lock_state().inodes.rename_subtree(src_rel, dst_rel);
        }

        #[cfg(test)]
        pub(super) fn test_resolve_ino_path(
            &self,
            ino: INodeNo,
        ) -> std::result::Result<PathBuf, Errno> {
            self.lock_state().inodes.resolve_ino_path(ino)
        }

        #[cfg(test)]
        pub(super) fn test_stat_under_held_lock(
            &self,
            rel: &Path,
            inc_lookup: bool,
        ) -> std::result::Result<FileAttr, Errno> {
            self.stat_resolved_unlocked(rel, inc_lookup)
        }

        #[cfg(test)]
        pub(super) fn test_read_file_via_ino(
            &self,
            ino: INodeNo,
        ) -> std::result::Result<Vec<u8>, Errno> {
            let rel = self.lock_state().inodes.resolve_ino_path(ino)?;
            let resolved = resolve_rel_path(&self.cfg, &rel).ok_or(Errno::ENOENT)?;
            std::fs::read(&resolved.host_path).map_err(|e| errno_from_io(&e))
        }

        #[cfg(test)]
        pub(super) fn test_active_path_locks(&self) -> usize {
            self.path_locks.active_slot_count()
        }

        #[cfg(test)]
        pub(super) fn test_poison_state(&self) {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = self.state.lock();
                panic!("intentional poison for recovery test");
            }));
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
                        let resolved =
                            resolve_rel_path(&self.cfg, &cur_rel).ok_or(Errno::ENOENT)?;
                        let lower_meta = std::fs::symlink_metadata(&resolved.host_path)
                            .map_err(|err| errno_from_io(&err))?;
                        if !lower_meta.file_type().is_dir() {
                            return Err(Errno::ENOTDIR);
                        }
                        match std::fs::create_dir(&upper_cand) {
                            Ok(()) => {
                                let mode = lower_meta.mode() & 0o7777;
                                let _ = std::fs::set_permissions(
                                    &upper_cand,
                                    Permissions::from_mode(mode),
                                );
                                let _ = copy_timestamps(&upper_cand, &lower_meta);
                            }
                            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                                let existing = std::fs::symlink_metadata(&upper_cand)
                                    .map_err(|e| errno_from_io(&e))?;
                                if !existing.file_type().is_dir() {
                                    return Err(Errno::ENOTDIR);
                                }
                            }
                            Err(err) => return Err(errno_from_io(&err)),
                        }
                    }
                    Err(e) => return Err(errno_from_io(&e)),
                }
            }
            Ok(())
        }

        /// Copies `rel` from lower layers into `upper_dir` without holding `self.state`.
        /// Caller must hold an exclusive [`PathLockGuard`] covering `rel`.
        fn copy_up_entry_unlocked(
            &self,
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
                match std::fs::create_dir(&upper_target) {
                    Ok(()) => {
                        let _ = std::fs::set_permissions(
                            &upper_target,
                            Permissions::from_mode(lower_meta.mode() & 0o7777),
                        );
                        let _ = copy_timestamps(&upper_target, &lower_meta);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        let existing = std::fs::symlink_metadata(&upper_target)
                            .map_err(|err| errno_from_io(&err))?;
                        if !existing.file_type().is_dir() {
                            return Err(Errno::ENOTDIR);
                        }
                    }
                    Err(e) => return Err(errno_from_io(&e)),
                }
            } else if ft.is_symlink() {
                let target =
                    std::fs::read_link(&resolved.host_path).map_err(|e| errno_from_io(&e))?;
                std::os::unix::fs::symlink(&target, &upper_target)
                    .map_err(|e| errno_from_io(&e))?;
                let _ = copy_timestamps(&upper_target, &lower_meta);
            } else if ft.is_file() {
                let tmp_dir = self.cfg.meta_dir.join("tmp");
                std::fs::create_dir_all(&tmp_dir).map_err(|e| errno_from_io(&e))?;
                let tmp_seq = self.next_tmp.fetch_add(1, Ordering::Relaxed);
                let tmp_path = tmp_dir.join(format!("copyup.{tmp_seq}"));
                let mut dst = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&tmp_path)
                    .map_err(|e| errno_from_io(&e))?;
                if !truncate_to_zero {
                    let mut src = File::open(&resolved.host_path).map_err(|e| errno_from_io(&e))?;
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

        /// Recursively copies all lower-layer entries under `rel_dir` into `upper_dir` without
        /// holding `self.state`. Caller must hold an exclusive [`PathLockGuard`] on `rel_dir`.
        fn materialize_lower_dir_tree_unlocked(
            &self,
            rel_dir: &Path,
        ) -> std::result::Result<(), Errno> {
            self.ensure_upper_dir(rel_dir)?;
            let entries =
                list_merged_dir_entries(&self.cfg, rel_dir).map_err(|e| errno_from_io(&e))?;
            for entry in entries {
                let child_rel = Self::child_rel_path(rel_dir, &entry.name);
                if entry.file_type.is_dir() {
                    self.materialize_lower_dir_tree_unlocked(&child_rel)?;
                } else {
                    self.copy_up_entry_unlocked(&child_rel, false)?;
                }
            }
            Ok(())
        }

        /// Resolves and stats `rel` outside `self.state`, acquiring `self.state` only briefly
        /// for the pure in-memory [`InodeTable`] lookup/insert or detach.
        ///
        /// Caller must hold a shared or exclusive [`PathLockGuard`] covering `rel` so
        /// `detach_path` / `lookup_or_insert` cannot race with concurrent `rename` or `create`.
        fn stat_resolved_unlocked(
            &self,
            rel: &Path,
            inc_lookup: bool,
        ) -> std::result::Result<FileAttr, Errno> {
            let Some(resolved) = resolve_rel_path(&self.cfg, rel) else {
                self.lock_state().inodes.detach_path(rel);
                return Err(Errno::ENOENT);
            };
            let meta = match std::fs::symlink_metadata(&resolved.host_path) {
                Ok(m) => m,
                Err(e) => {
                    self.lock_state().inodes.detach_path(rel);
                    return Err(errno_from_io(&e));
                }
            };
            let is_dir = meta.file_type().is_dir();
            let ino = if rel.as_os_str().is_empty() {
                INodeNo::ROOT
            } else {
                self.lock_state()
                    .inodes
                    .lookup_or_insert(rel, is_dir, inc_lookup)
            };
            Ok(metadata_to_file_attr(ino, &meta))
        }
    }

    fn errno_from_io(err: &std::io::Error) -> Errno {
        err.raw_os_error().map_or(Errno::EIO, Errno::from_i32)
    }

    fn system_time_from_unix(secs: i64, nsec: i64) -> SystemTime {
        let clamped_ns = u32::try_from(nsec.clamp(0, 999_999_999)).unwrap_or(0);
        if let Ok(secs_u64) = u64::try_from(secs) {
            UNIX_EPOCH + Duration::new(secs_u64, clamped_ns)
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
            u32::try_from(meta.nlink()).unwrap_or(u32::MAX)
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
            rdev: u32::try_from(meta.rdev()).unwrap_or(0),
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
                    tv_sec: libc::time_t::try_from(d.as_secs()).unwrap_or(libc::time_t::MAX),
                    tv_nsec: libc::c_long::from(d.subsec_nanos()),
                },
                Err(e) => {
                    let d = e.duration();
                    let secs = libc::time_t::try_from(d.as_secs()).unwrap_or(libc::time_t::MAX);
                    let nsec = d.subsec_nanos();
                    if nsec == 0 {
                        libc::timespec {
                            tv_sec: -secs,
                            tv_nsec: 0,
                        }
                    } else {
                        libc::timespec {
                            tv_sec: -secs - 1,
                            tv_nsec: libc::c_long::from(1_000_000_000 - nsec),
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
        // SAFETY: `c_path` is a valid NUL-terminated C string and `times` is an initialized
        // 2-element `[libc::timespec; 2]` array valid for the duration of `utimensat`.
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
        // SAFETY: `file.as_raw_fd()` is an open file descriptor owned by `file`, and `times`
        // points to a valid initialized 2-element `[libc::timespec; 2]` array.
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

    fn lchown_path(
        path: &Path,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> std::result::Result<(), Errno> {
        if uid.is_none() && gid.is_none() {
            return Ok(());
        }
        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| Errno::EINVAL)?;
        let uid_val = uid.map_or(u32::MAX as libc::uid_t, |u| u as libc::uid_t);
        let gid_val = gid.map_or(u32::MAX as libc::gid_t, |g| g as libc::gid_t);
        // SAFETY: `c_path` is a valid NUL-terminated C string; `uid_val`/`gid_val` use
        // `(uid_t)-1` / `(gid_t)-1` (`u32::MAX`) to leave omitted IDs unchanged per POSIX.
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
            if name != "." && name != ".." && !is_valid_child_name(name) {
                reply.error(Errno::EINVAL);
                return;
            }
            let (rel, _path_guard) = match self.lock_lookup_path(parent, name) {
                Ok(v) => v,
                Err(e) => {
                    reply.error(e);
                    return;
                }
            };

            match self.stat_resolved_unlocked(&rel, true) {
                Ok(attr) => reply.entry(&ZERO_TTL, &attr, Generation(0)),
                Err(e) => reply.error(e),
            }
        }

        fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
            self.lock_state().inodes.forget(ino, nlookup);
        }

        fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
            match self.getattr_inner(ino, fh) {
                Ok(attr) => reply.attr(&ZERO_TTL, &attr),
                Err(e) => reply.error(e),
            }
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
            let locked = loop {
                let candidate = {
                    let inner = self.lock_state();
                    match inner.inodes.resolve_ino_path(ino) {
                        Ok(rel) => Ok(rel),
                        Err(err) => {
                            let open_match = fh
                                .and_then(|handle| inner.open_files.get(&handle))
                                .or_else(|| {
                                    inner
                                        .open_files
                                        .values()
                                        .find(|o| o.ino == ino && o.writable)
                                        .or_else(|| {
                                            inner.open_files.values().find(|o| o.ino == ino)
                                        })
                                })
                                .map(|open| (Arc::clone(&open.file), open.writable));
                            Err((err, open_match))
                        }
                    }
                };
                let rel = match candidate {
                    Ok(rel) => rel,
                    Err(unlinked) => break Err(unlinked),
                };
                let guard = self.path_locks.lock_path(&rel);
                let recheck = {
                    let inner = self.lock_state();
                    if inner.inodes.resolve_ino_path(ino).as_ref() == Ok(&rel) {
                        let writable_fh_file = fh
                            .and_then(|handle| inner.open_files.get(&handle))
                            .filter(|open| open.writable)
                            .map(|open| Arc::clone(&open.file));
                        Some(writable_fh_file)
                    } else {
                        None
                    }
                };
                if let Some(writable_fh_file) = recheck {
                    break Ok((rel, guard, writable_fh_file));
                }
                drop(guard);
            };

            let (rel, _path_guard, writable_fh_file) = match locked {
                Ok(v) => v,
                Err((_err, Some((file, writable)))) => {
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
                Err((err, None)) => {
                    reply.error(err);
                    return;
                }
            };

            let truncate_zero = size == Some(0);
            if let Err(e) = self.copy_up_entry_unlocked(&rel, truncate_zero) {
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
                let res = if let Some(ref open_file) = writable_fh_file {
                    open_file.set_len(new_size)
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
            let Ok(rel) = self.lock_state().inodes.resolve_ino_path(ino) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let Some(resolved) = resolve_rel_path(&self.cfg, &rel) else {
                reply.error(Errno::ENOENT);
                return;
            };
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
            let (parent_rel, rel, _path_guard) = match self.lock_parent_child(parent, name) {
                Ok(v) => v,
                Err(e) => {
                    reply.error(e);
                    return;
                }
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
                    let _ = std::fs::set_permissions(&upper_path, Permissions::from_mode(eff_mode));
                    match self.stat_resolved_unlocked(&rel, true) {
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
            let (parent_rel, rel, _path_guard) = match self.lock_parent_child(parent, name) {
                Ok(v) => v,
                Err(e) => {
                    reply.error(e);
                    return;
                }
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

            match self.stat_resolved_unlocked(&rel, true) {
                Ok(attr) => reply.entry(&ZERO_TTL, &attr, Generation(0)),
                Err(e) => reply.error(e),
            }
        }

        fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
            if !is_valid_child_name(name) {
                reply.error(Errno::EINVAL);
                return;
            }
            let (parent_rel, rel, _path_guard) = match self.lock_parent_child(parent, name) {
                Ok(v) => v,
                Err(e) => {
                    reply.error(e);
                    return;
                }
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

            self.lock_state().inodes.detach_path(&rel);
            reply.ok();
        }

        fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
            if !is_valid_child_name(name) {
                reply.error(Errno::EINVAL);
                return;
            }
            let (parent_rel, rel, _path_guard) = match self.lock_parent_child(parent, name) {
                Ok(v) => v,
                Err(e) => {
                    reply.error(e);
                    return;
                }
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

            self.lock_state().inodes.detach_path(&rel);
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
            let (parent_rel, rel, _path_guard) = match self.lock_parent_child(parent, link_name) {
                Ok(v) => v,
                Err(e) => {
                    reply.error(e);
                    return;
                }
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

            match self.stat_resolved_unlocked(&rel, true) {
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

            let LockedRenamePaths {
                src_parent,
                src_rel,
                dst_parent,
                dst_rel,
                _guard: _path_guard,
            } = match self.lock_rename_paths(parent, name, newparent, newname) {
                Ok(Some(v)) => v,
                Ok(None) => {
                    reply.ok();
                    return;
                }
                Err(e) => {
                    reply.error(e);
                    return;
                }
            };

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
                let upper_src = self.upper_path(&src_rel);
                let upper_dst = self.upper_path(&dst_rel);
                if let (Ok(u_src_meta), Ok(u_dst_meta)) = (
                    std::fs::symlink_metadata(&upper_src),
                    std::fs::symlink_metadata(&upper_dst),
                ) && u_src_meta.dev() == u_dst_meta.dev()
                    && u_src_meta.ino() == u_dst_meta.ino()
                {
                    // POSIX: renaming two hardlinks pointing to the same inode is a no-op.
                    reply.ok();
                    return;
                }
                if let Err(e) = self.copy_up_entry_unlocked(&src_rel, false) {
                    reply.error(e);
                    return;
                }
                let _ = Self::remove_marker_if_exists(&marker_path(
                    &self.cfg.whiteouts_dir(),
                    &dst_rel,
                    WHITEOUT_SUFFIX,
                ));
                if let Err(e) = std::fs::rename(&upper_src, &upper_dst) {
                    reply.error(errno_from_io(&e));
                    return;
                }
                if resolve_in_lower_layers(&self.cfg, &src_rel).is_some()
                    && !lower_layers_blocked_at_dir(&self.cfg, &src_parent)
                {
                    let wh_path = marker_path(&self.cfg.whiteouts_dir(), &src_rel, WHITEOUT_SUFFIX);
                    if let Err(e) = Self::write_marker(&wh_path) {
                        reply.error(e);
                        return;
                    }
                }
                self.lock_state().inodes.rename_subtree(&src_rel, &dst_rel);
                reply.ok();
                return;
            }

            // Directory rename: materialize any lower-only entries into upper/src_rel before moving.
            if let Err(e) = self.materialize_lower_dir_tree_unlocked(&src_rel) {
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

            self.lock_state().inodes.rename_subtree(&src_rel, &dst_rel);
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
            let (old_rel, new_parent_rel, new_rel, _path_guard) =
                match self.lock_link_paths(ino, newparent, newname) {
                    Ok(v) => v,
                    Err(e) => {
                        reply.error(e);
                        return;
                    }
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

            if let Err(e) = self.copy_up_entry_unlocked(&old_rel, false) {
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
            if let Err(e) = self.lock_state().inodes.add_alias(ino, new_rel, true) {
                reply.error(e);
                return;
            }
            match std::fs::symlink_metadata(&upper_new) {
                Ok(meta) => {
                    reply.entry(&ZERO_TTL, &metadata_to_file_attr(ino, &meta), Generation(0));
                }
                Err(e) => reply.error(errno_from_io(&e)),
            }
        }

        fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
            let acc = flags.acc_mode();
            let wants_trunc = (flags.0 & libc::O_TRUNC) != 0;
            let wants_write =
                matches!(acc, OpenAccMode::O_WRONLY | OpenAccMode::O_RDWR) || wants_trunc;

            let file = if wants_write {
                let (rel, _path_guard) = match self.lock_ino_path(ino) {
                    Ok(v) => v,
                    Err(e) => {
                        reply.error(e);
                        return;
                    }
                };
                if let Err(e) = self.copy_up_entry_unlocked(&rel, wants_trunc) {
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
                let Ok(rel) = self.lock_state().inodes.resolve_ino_path(ino) else {
                    reply.error(Errno::ENOENT);
                    return;
                };
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

            let fh = {
                let mut inner = self.lock_state();
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
                fh
            };
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
            let file_or_rel = {
                let inner = self.lock_state();
                if let Some(open) = inner.open_files.get(&fh) {
                    Ok(Arc::clone(&open.file))
                } else if let Ok(rel) = inner.inodes.resolve_ino_path(ino) {
                    Err(rel)
                } else {
                    reply.error(Errno::EBADF);
                    return;
                }
            };
            let file = match file_or_rel {
                Ok(f) => f,
                Err(rel) => {
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
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
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
            let Ok(written) = u32::try_from(data.len()) else {
                reply.error(Errno::EINVAL);
                return;
            };
            let file = {
                let inner = self.lock_state();
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
            reply.written(written);
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
            // Drop the removed `OpenFile` outside `self.state` so `close(fd)` never runs locked.
            let _closed = { self.lock_state().open_files.remove(&fh) };
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
                let inner = self.lock_state();
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
            let (parent_rel, rel, _path_guard) = match self.lock_parent_child(parent, name) {
                Ok(v) => v,
                Err(e) => {
                    reply.error(e);
                    return;
                }
            };

            let excl = (flags & libc::O_EXCL) != 0;
            let trunc = (flags & libc::O_TRUNC) != 0;
            let existed = resolve_rel_path(&self.cfg, &rel).is_some();
            if excl && existed {
                reply.error(Errno::EEXIST);
                return;
            }
            if !excl
                && existed
                && let Err(e) = self.copy_up_entry_unlocked(&rel, trunc)
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
            let acc_mode = flags & libc::O_ACCMODE;

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
                Err(e)
                    if existed
                        && e.kind() == std::io::ErrorKind::PermissionDenied
                        && acc_mode == libc::O_WRONLY =>
                {
                    match OpenOptions::new()
                        .write(true)
                        .truncate(trunc)
                        .open(&upper_path)
                    {
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
            };

            // POSIX: O_CREAT must only set permissions when creating a brand-new file;
            // opening an existing executable file with O_CREAT|O_WRONLY must preserve its mode.
            if !existed {
                let _ = std::fs::set_permissions(&upper_path, Permissions::from_mode(eff_mode));
            }

            let meta = match std::fs::symlink_metadata(&upper_path) {
                Ok(m) => m,
                Err(e) => {
                    reply.error(errno_from_io(&e));
                    return;
                }
            };
            let (attr, fh) = {
                let mut inner = self.lock_state();
                let ino = inner
                    .inodes
                    .lookup_or_insert(&rel, meta.file_type().is_dir(), true);
                let attr = metadata_to_file_attr(ino, &meta);
                let fh = FileHandle(inner.next_fh);
                inner.next_fh += 1;
                inner.open_files.insert(
                    fh,
                    OpenFile {
                        ino,
                        file: Arc::new(file),
                        writable: true,
                    },
                );
                (attr, fh)
            };
            reply.created(&ZERO_TTL, &attr, Generation(0), fh, FopenFlags::empty());
        }

        fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
            let Ok(rel) = self.lock_state().inodes.resolve_ino_path(ino) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let Some(resolved) = resolve_rel_path(&self.cfg, &rel) else {
                reply.error(Errno::ENOENT);
                return;
            };
            match std::fs::symlink_metadata(&resolved.host_path) {
                Ok(meta) if meta.file_type().is_dir() => {
                    let fh = {
                        let mut inner = self.lock_state();
                        let fh = FileHandle(inner.next_fh);
                        inner.next_fh += 1;
                        inner.open_dirs.insert(fh, None);
                        fh
                    };
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
            // Reuse the directory snapshot across paginated `readdir` calls (`offset > 0`) on
            // the same directory handle so large directories enumerate in O(M) instead of O(M^2).
            // Re-reading at `offset == 0` (fresh opendir / rewinddir) always refreshes from disk
            // so host-side mutations (`oz undo`) remain immediately visible.
            let cached_or_rel = {
                let inner = self.lock_state();
                if offset > 0
                    && let Some(Some(cached)) = inner.open_dirs.get(&fh)
                {
                    Ok(Arc::clone(cached))
                } else {
                    match inner.inodes.resolve_ino_path(ino) {
                        Ok(rel_dir) => Err(rel_dir),
                        Err(e) => {
                            reply.error(e);
                            return;
                        }
                    }
                }
            };

            let snapshot: DirSnapshot = match cached_or_rel {
                Ok(cached) => cached,
                Err(rel_dir) => {
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

                    let mut inner = self.lock_state();
                    let parent_ino = if rel_dir.as_os_str().is_empty() {
                        INodeNo::ROOT
                    } else {
                        let parent_rel =
                            rel_dir.parent().map(Path::to_path_buf).unwrap_or_default();
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
                        let child_rel = Self::child_rel_path(&rel_dir, &item.name);
                        let is_dir = item.file_type.is_dir();
                        let child_ino = inner.inodes.lookup_or_insert(&child_rel, is_dir, false);
                        let kind =
                            FileType::from_std(item.file_type).unwrap_or(FileType::RegularFile);
                        all_entries.push((child_ino, kind, item.name));
                    }

                    let arc = Arc::new(all_entries);
                    if let Some(slot) = inner.open_dirs.get_mut(&fh) {
                        *slot = Some(Arc::clone(&arc));
                    }
                    arc
                }
            };

            if let Ok(skip_count) = usize::try_from(offset) {
                for (idx, (entry_ino, kind, name)) in snapshot.iter().enumerate().skip(skip_count) {
                    let next_offset = (idx + 1) as u64;
                    if reply.add(*entry_ino, next_offset, *kind, name) {
                        break;
                    }
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
            let _closed = { self.lock_state().open_dirs.remove(&fh) };
            reply.ok();
        }

        fn fsyncdir(
            &self,
            _req: &Request,
            ino: INodeNo,
            _fh: FileHandle,
            datasync: bool,
            reply: ReplyEmpty,
        ) {
            let Ok(rel_dir) = self.lock_state().inodes.resolve_ino_path(ino) else {
                reply.error(Errno::ENOENT);
                return;
            };
            if resolve_rel_path(&self.cfg, &rel_dir).is_none() {
                reply.error(Errno::ENOENT);
                return;
            }
            let upper_dir_path = self.upper_path(&rel_dir);
            if let Ok(meta) = std::fs::symlink_metadata(&upper_dir_path)
                && meta.file_type().is_dir()
                && let Ok(dir_file) = File::open(&upper_dir_path)
            {
                let res = if datasync {
                    dir_file.sync_data()
                } else {
                    dir_file.sync_all()
                };
                match res {
                    Ok(()) => reply.ok(),
                    Err(e) => reply.error(errno_from_io(&e)),
                }
                return;
            }
            reply.ok();
        }

        fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
            let Ok(c_path) = CString::new(self.cfg.upper_dir.as_os_str().as_bytes()) else {
                reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096);
                return;
            };
            // SAFETY: `libc::statvfs` is a plain C struct of integer fields where all-zero bits
            // is a valid initial representation before being populated by `libc::statvfs`.
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            // SAFETY: `c_path` is a valid NUL-terminated C string and `&raw mut stat` is a valid,
            // aligned, writable `libc::statvfs` output struct.
            let rc = unsafe { libc::statvfs(c_path.as_ptr(), &raw mut stat) };
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
                u32::try_from(stat.f_bsize).unwrap_or(u32::MAX),
                u32::try_from(stat.f_namemax).unwrap_or(255),
                u32::try_from(stat.f_frsize).unwrap_or(u32::MAX),
            );
        }

        fn access(&self, req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
            let Ok(rel) = self.lock_state().inodes.resolve_ino_path(ino) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let Some(resolved) = resolve_rel_path(&self.cfg, &rel) else {
                reply.error(Errno::ENOENT);
                return;
            };
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
        assert_eq!(
            list_merged_dir_entries(&cfg, Path::new("pkg")).expect("readdir opaque"),
            &[]
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
            marker_path(
                &cfg.whiteouts_dir(),
                Path::new("removed.txt"),
                WHITEOUT_SUFFIX,
            ),
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

    #[cfg(unix)]
    #[test]
    fn mountinfo_parser_unescapes_spaces_tabs_and_filters_fuse_types() {
        let sample = "\
275 30 0:50 / /proc/sys/fs/binfmt_misc rw,relatime - binfmt_misc binfmt_misc rw\n\
307 815 0:71 / /home/user/My\\040Repo.oz/ws\\011tab rw,nosuid - fuse.ozone ozone:ws rw\n\
308 815 0:72 / /mnt/plain\\134dir rw - ext4 /dev/nvme0n1p1 rw\n\
309 815 0:73 / /tmp/raw-fuse rw - fuse portal rw\n";

        let parsed = parse_fuse_mountpoints_from_mountinfo(sample);
        assert_eq!(
            parsed,
            vec![
                PathBuf::from("/home/user/My Repo.oz/ws\ttab"),
                PathBuf::from("/tmp/raw-fuse"),
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn overlay_copy_up_inode_table_and_lazy_unmount_non_mount_path_behave_cleanly() {
        let dir = TempDir::new().expect("tempdir");
        let cfg = test_cfg(dir.path());
        let base = cfg.baseline_dir.as_ref().expect("base");
        std::fs::write(base.join("exec.sh"), b"#!/bin/sh\necho v1\n").expect("write exec");
        std::fs::set_permissions(
            base.join("exec.sh"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .expect("chmod");

        // Ensure copy-up + truncation preserves 0o755 executable permissions without touching baseline.
        let fs = linux_impl::OzoneOverlayFs::new(cfg.clone());
        fs.test_copy_up_entry(Path::new("exec.sh"), true)
            .expect("copy_up truncate");
        let upper_exec = cfg.upper_dir.join("exec.sh");
        assert_eq!(std::fs::read(&upper_exec).expect("read upper"), b"");
        let upper_mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&upper_exec).expect("meta").permissions(),
        ) & 0o777;
        assert_eq!(upper_mode, 0o755);
        assert_eq!(
            std::fs::read_to_string(base.join("exec.sh")).expect("base intact"),
            "#!/bin/sh\necho v1\n"
        );

        // Verify lazy_unmount_fuse does not hang or panic on a non-existent path.
        let missing = dir.path().join("not-a-mount");
        lazy_unmount_fuse(&missing).expect("lazy unmount missing path");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn concurrent_copy_up_does_not_block_parallel_lookups_reads_or_unrelated_copy_ups() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Barrier};
        use std::time::Duration;

        let dir = TempDir::new().expect("tempdir");
        let cfg = test_cfg(dir.path());
        let base = cfg.baseline_dir.as_ref().expect("base");

        std::fs::create_dir_all(base.join("dir_a")).expect("mkdir dir_a");
        std::fs::create_dir_all(base.join("dir_b")).expect("mkdir dir_b");
        std::fs::write(base.join("dir_a/heavy.bin"), vec![0xABu8; 64 * 1024]).expect("write heavy");
        std::fs::write(base.join("dir_a/sibling.txt"), b"sibling-data").expect("write sibling");
        std::fs::write(base.join("dir_a/child.txt"), b"child-data").expect("write child");
        std::fs::write(base.join("dir_b/other.txt"), b"other-data").expect("write other");

        let fs = Arc::new(linux_impl::OzoneOverlayFs::new(cfg.clone()));

        // Phase 1: Hold an exclusive path lock on `dir_a/heavy.bin` (simulating a slow copy-up)
        // and prove deterministically via barriers that lookups, reads, sibling copy-ups, and
        // unrelated copy-ups all finish BEFORE `dir_a/heavy.bin` is released.
        let locked_barrier = Arc::new(Barrier::new(2));
        let release_barrier = Arc::new(Barrier::new(2));
        let fs_holder = Arc::clone(&fs);
        let b_locked = Arc::clone(&locked_barrier);
        let b_release = Arc::clone(&release_barrier);

        let holder = std::thread::spawn(move || {
            let _guard = fs_holder.test_lock_path(Path::new("dir_a/heavy.bin"));
            b_locked.wait();
            b_release.wait();
        });

        locked_barrier.wait();

        // Parallel lookups, getattr, and reads across unrelated (`dir_b/other.txt`) and sibling
        // (`dir_a/sibling.txt`, sharing `dir_a`) paths must complete immediately without blocking.
        let other_attr = fs
            .test_stat_resolved(Path::new("dir_b/other.txt"), true)
            .expect("lookup unrelated path while heavy copy-up holds path lock");
        assert_eq!(
            fs.test_getattr_ino(other_attr.ino)
                .expect("getattr unrelated path")
                .size,
            b"other-data".len() as u64
        );
        assert_eq!(
            fs.test_read_file_via_ino(other_attr.ino)
                .expect("read unrelated path"),
            b"other-data"
        );
        let sibling_attr = fs
            .test_stat_resolved(Path::new("dir_a/sibling.txt"), true)
            .expect("lookup sibling path while heavy copy-up holds Shared(dir_a)");
        assert_eq!(
            fs.test_getattr_ino(sibling_attr.ino)
                .expect("getattr sibling path")
                .size,
            b"sibling-data".len() as u64
        );
        assert_eq!(
            fs.test_read_file_via_ino(sibling_attr.ino)
                .expect("read sibling path"),
            b"sibling-data"
        );

        // Unrelated copy-up and sibling copy-up under the same parent (`dir_a` held Shared)
        // must both complete without stalling on `dir_a/heavy.bin`.
        fs.test_copy_up_entry(Path::new("dir_b/other.txt"), false)
            .expect("copy-up unrelated file");
        fs.test_copy_up_entry(Path::new("dir_a/sibling.txt"), false)
            .expect("copy-up sibling file");
        assert_eq!(
            std::fs::read(cfg.upper_dir.join("dir_b/other.txt")).expect("read upper other"),
            b"other-data"
        );
        assert_eq!(
            std::fs::read(cfg.upper_dir.join("dir_a/sibling.txt")).expect("read upper sibling"),
            b"sibling-data"
        );

        release_barrier.wait();
        holder.join().expect("join holder");

        // Lookup on `dir_a/heavy.bin` succeeds immediately once its exclusive path lock drops.
        let heavy_attr = fs
            .test_stat_resolved(Path::new("dir_a/heavy.bin"), true)
            .expect("lookup heavy after copy-up finishes");
        assert_eq!(
            fs.test_read_file_via_ino(heavy_attr.ino)
                .expect("read heavy baseline")
                .len(),
            64 * 1024
        );

        // Phase 2: Exclusive lock on parent directory `dir_a` (e.g. directory rename/rmdir)
        // must serialize against a descendant copy-up (`dir_a/child.txt`) until released.
        let dir_guard = fs.test_lock_path(Path::new("dir_a"));
        let child_done = Arc::new(AtomicBool::new(false));
        let fs_child = Arc::clone(&fs);
        let done_flag = Arc::clone(&child_done);
        let child_thread = std::thread::spawn(move || {
            fs_child
                .test_copy_up_entry(Path::new("dir_a/child.txt"), false)
                .expect("child copy-up");
            done_flag.store(true, Ordering::SeqCst);
        });

        std::thread::sleep(Duration::from_millis(30));
        assert!(
            !child_done.load(Ordering::SeqCst),
            "descendant copy-up must wait while ancestor directory is exclusively locked"
        );
        drop(dir_guard);
        child_thread.join().expect("join child");
        assert!(child_done.load(Ordering::SeqCst));
        assert_eq!(
            std::fs::read(cfg.upper_dir.join("dir_a/child.txt")).expect("read upper child"),
            b"child-data"
        );
        assert_eq!(fs.test_active_path_locks(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn concurrent_copy_up_ab_ba_locks_and_lookups_are_deadlock_free_and_preserve_data() {
        use std::sync::Arc;

        let dir = TempDir::new().expect("tempdir");
        let cfg = test_cfg(dir.path());
        let base = cfg.baseline_dir.as_ref().expect("base");

        std::fs::create_dir_all(base.join("nested/deep")).expect("mkdir nested");
        for i in 0..12 {
            let payload = format!("payload-{i:02}-{}", "x".repeat(4096));
            std::fs::write(base.join(format!("nested/deep/file_{i:02}.txt")), &payload)
                .expect("write nested file");
            std::fs::set_permissions(
                base.join(format!("nested/deep/file_{i:02}.txt")),
                std::os::unix::fs::PermissionsExt::from_mode(if i % 2 == 0 {
                    0o755
                } else {
                    0o644
                }),
            )
            .expect("chmod");
        }

        let fs = Arc::new(linux_impl::OzoneOverlayFs::new(cfg.clone()));
        let mut handles = Vec::new();

        // 4 threads concurrently copying up overlapping + distinct files under `nested/deep/`
        // (stressing both same-path serialization and concurrent `ensure_upper_dir` creation).
        for tid in 0..4usize {
            let fs_c = Arc::clone(&fs);
            handles.push(std::thread::spawn(move || {
                for round in 0..3 {
                    for i in 0..12 {
                        let idx = (i + tid * 3 + round) % 12;
                        let rel = PathBuf::from(format!("nested/deep/file_{idx:02}.txt"));
                        fs_c.test_copy_up_entry(&rel, false)
                            .expect("concurrent copy_up must succeed");
                    }
                }
            }));
        }

        // 2 threads performing continuous parallel lookups and reads.
        for tid in 0..2usize {
            let fs_r = Arc::clone(&fs);
            handles.push(std::thread::spawn(move || {
                for iter in 0..40 {
                    let idx = (iter + tid * 5) % 12;
                    let rel = PathBuf::from(format!("nested/deep/file_{idx:02}.txt"));
                    let attr = fs_r
                        .test_stat_resolved(&rel, false)
                        .expect("parallel lookup must succeed");
                    let data = fs_r
                        .test_read_file_via_ino(attr.ino)
                        .expect("parallel read must succeed");
                    let expected = format!("payload-{idx:02}-{}", "x".repeat(4096));
                    assert_eq!(data, expected.as_bytes());
                }
            }));
        }

        // 2 threads hammering opposite-order AB-BA and ancestor/descendant `lock_two_paths`.
        for tid in 0..2usize {
            let fs_l = Arc::clone(&fs);
            handles.push(std::thread::spawn(move || {
                for _ in 0..60 {
                    let (a, b) = if tid == 0 {
                        (
                            Path::new("nested/deep/file_00.txt"),
                            Path::new("nested/deep/file_01.txt"),
                        )
                    } else {
                        (
                            Path::new("nested/deep/file_01.txt"),
                            Path::new("nested/deep/file_00.txt"),
                        )
                    };
                    drop(fs_l.test_lock_two_paths(a, b));

                    let (c, d) = if tid == 0 {
                        (Path::new("nested"), Path::new("other_dir"))
                    } else {
                        (Path::new("other_dir"), Path::new("nested/deep/file_02.txt"))
                    };
                    let _g2 = fs_l.test_lock_two_paths(c, d);
                }
            }));
        }

        for h in handles {
            h.join().expect("worker thread panicked");
        }

        for i in 0..12 {
            let rel = format!("nested/deep/file_{i:02}.txt");
            let expected = format!("payload-{i:02}-{}", "x".repeat(4096));
            let upper_p = cfg.upper_dir.join(&rel);
            assert_eq!(
                std::fs::read_to_string(&upper_p).expect("read upper"),
                expected
            );
            let mode = std::os::unix::fs::PermissionsExt::mode(
                &std::fs::metadata(&upper_p).expect("meta").permissions(),
            ) & 0o777;
            assert_eq!(mode, if i % 2 == 0 { 0o755 } else { 0o644 });
        }
        assert_eq!(
            fs.test_active_path_locks(),
            0,
            "path lock table must reclaim all idle slots"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn concurrent_getattr_during_rename_and_lookup_during_create_never_corrupt_inodes() {
        use std::sync::{Arc, Barrier};
        use std::time::Duration;

        let dir = TempDir::new().expect("tempdir");
        let cfg = test_cfg(dir.path());
        let base = cfg.baseline_dir.as_ref().expect("base");
        std::fs::write(base.join("a.txt"), b"hello-rename").expect("write a.txt");

        let fs = Arc::new(linux_impl::OzoneOverlayFs::new(cfg.clone()));
        let attr_a = fs
            .test_stat_resolved(Path::new("a.txt"), true)
            .expect("initial lookup a.txt");

        // Regression 1: `getattr(ino_a)` runs in the critical window after `rename("a.txt", "b.txt")`
        // has moved the file on disk and written the whiteout for `"a.txt"`, BEFORE updating
        // `inodes.rename_subtree("a.txt", "b.txt")`. Shared path locking + revalidation must
        // prevent `getattr` from detaching `"a.txt"` and orphaning `ino_a`.
        let rename_ready = Arc::new(Barrier::new(2));
        let fs_r = Arc::clone(&fs);
        let cfg_r = cfg.clone();
        let ready_r = Arc::clone(&rename_ready);
        let rename_thread = std::thread::spawn(move || {
            let _guard = fs_r.test_lock_two_paths(Path::new("a.txt"), Path::new("b.txt"));
            std::fs::write(cfg_r.upper_dir.join("b.txt"), b"hello-rename").expect("write upper b");
            let wh_a = marker_path(&cfg_r.whiteouts_dir(), Path::new("a.txt"), WHITEOUT_SUFFIX);
            std::fs::write(&wh_a, b"").expect("write wh a");
            // Signal `getattr` thread while disk state has `"a.txt"` gone and `"b.txt"` present,
            // but `InodeTable` still maps `ino_a -> "a.txt"`.
            ready_r.wait();
            std::thread::sleep(Duration::from_millis(30));
            fs_r.test_rename_subtree_inodes(Path::new("a.txt"), Path::new("b.txt"));
        });

        rename_ready.wait();
        let attr_after = fs
            .test_getattr_ino(attr_a.ino)
            .expect("getattr must serialize with rename and follow renamed inode");
        rename_thread.join().expect("join rename thread");

        assert_eq!(attr_after.ino, attr_a.ino);
        assert_eq!(attr_after.size, b"hello-rename".len() as u64);
        assert_eq!(
            fs.test_resolve_ino_path(attr_a.ino)
                .expect("ino_a must remain attached at b.txt"),
            PathBuf::from("b.txt")
        );
        assert_eq!(
            fs.test_read_file_via_ino(attr_a.ino)
                .expect("read renamed inode"),
            b"hello-rename"
        );

        // Regression 2: concurrent `lookup("fresh.txt")` during `create("fresh.txt")` must
        // serialize on the path lock and observe the newly created inode rather than detaching it.
        let create_ready = Arc::new(Barrier::new(2));
        let fs_c = Arc::clone(&fs);
        let cfg_c = cfg.clone();
        let ready_c = Arc::clone(&create_ready);
        let create_thread = std::thread::spawn(move || {
            let _guard = fs_c.test_lock_path(Path::new("fresh.txt"));
            ready_c.wait();
            std::thread::sleep(Duration::from_millis(30));
            std::fs::write(cfg_c.upper_dir.join("fresh.txt"), b"created").expect("write fresh");
            fs_c.test_stat_under_held_lock(Path::new("fresh.txt"), true)
                .expect("create inserts inode")
        });

        create_ready.wait();
        let lookup_attr = fs
            .test_stat_resolved(Path::new("fresh.txt"), true)
            .expect("lookup serialized behind create must see fresh.txt");
        let created_attr = create_thread.join().expect("join create thread");

        assert_eq!(lookup_attr.ino, created_attr.ino);
        assert_eq!(
            fs.test_resolve_ino_path(created_attr.ino)
                .expect("newly created inode must not be detached"),
            PathBuf::from("fresh.txt")
        );
        assert_eq!(fs.test_active_path_locks(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn poisoned_state_lock_recovers_gracefully_without_panicking() {
        let dir = TempDir::new().expect("tempdir");
        let cfg = test_cfg(dir.path());
        let base = cfg.baseline_dir.as_ref().expect("base");
        std::fs::write(base.join("survivor.txt"), b"still-here").expect("write");

        let fs = linux_impl::OzoneOverlayFs::new(cfg.clone());
        fs.test_poison_state();

        let attr = fs
            .test_stat_resolved(Path::new("survivor.txt"), true)
            .expect("lookup recovers from poisoned state lock");
        assert_eq!(
            fs.test_read_file_via_ino(attr.ino).expect("read recovers"),
            b"still-here"
        );
        fs.test_copy_up_entry(Path::new("survivor.txt"), false)
            .expect("copy_up succeeds after poison recovery");
        assert_eq!(
            std::fs::read(cfg.upper_dir.join("survivor.txt")).expect("read upper"),
            b"still-here"
        );
    }
}
