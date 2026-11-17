// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Windows ProjFS (`ProjectedFSLib.dll`) virtualization engine and un-gated CoW layer adapter.
//!
//! Mirrors Linux FUSE3's `[upper_dir, seed_dir, baseline_dir]` overlay layout and keeps
//! `.git/ozone/ws/<ws>/upper/` + `.git/ozone/ws/<ws>/meta/{whiteouts,opaque}` synchronized
//! on real NTFS disk so `oz-vfsd::scan_dirty_paths`, `oz-core::status`, and `oz-oplog`
//! work identically across Linux FUSE3 and Windows ProjFS.

use crate::overlay::{
    LayerKind, MergedDirEntry, OPAQUE_SUFFIX, OverlayMountConfig, ResolvedEntry, WHITEOUT_SUFFIX,
    ancestors_inclusive, list_merged_dir_entries, lower_layers_blocked_at_dir, marker_path,
    resolve_in_lower_layers, resolve_rel_path, scan_dirty_paths,
};
use crate::protocol::{DirtyPathEntry, MountSpec};
use oz_core::{OzError, Result, normalize_lexical};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{File, FileTimes, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

const UNIX_TO_WINDOWS_EPOCH_SECS: i128 = 11_644_473_600;
const TICKS_PER_SECOND: i128 = 10_000_000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;

pub const PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN: u32 = 0x0000_0001;
pub const PRJ_CB_DATA_FLAG_ENUM_RETURN_SINGLE_ENTRY: u32 = 0x0000_0002;

pub const PRJ_NOTIFY_NEW_FILE_CREATED: u32 = 0x0000_0004;
pub const PRJ_NOTIFY_FILE_OVERWRITTEN: u32 = 0x0000_0008;
pub const PRJ_NOTIFY_PRE_DELETE: u32 = 0x0000_0010;
pub const PRJ_NOTIFY_FILE_RENAMED: u32 = 0x0000_0080;
pub const PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED: u32 = 0x0000_0400;
pub const PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED: u32 = 0x0000_0800;

/// Converts a [`SystemTime`] to a Windows 100ns-tick `FILETIME` (`1601-01-01` epoch).
#[must_use]
pub fn system_time_to_filetime(st: SystemTime) -> i64 {
    let (unix_secs, subsec_nanos): (i128, i128) = match st.duration_since(UNIX_EPOCH) {
        Ok(d) => (i128::from(d.as_secs()), i128::from(d.subsec_nanos())),
        Err(e) => {
            let d = e.duration();
            let secs = i128::from(d.as_secs());
            let nanos = i128::from(d.subsec_nanos());
            if nanos == 0 {
                (-secs, 0)
            } else {
                (-secs - 1, 1_000_000_000 - nanos)
            }
        }
    };
    let ticks =
        (unix_secs + UNIX_TO_WINDOWS_EPOCH_SECS) * TICKS_PER_SECOND + (subsec_nanos / 100);
    ticks.clamp(0, i128::from(i64::MAX)) as i64
}

/// Converts a Unix timestamp (`unix_secs`, `subsec_nanos`) to a Windows `FILETIME` value.
#[must_use]
pub fn unix_to_filetime(unix_secs: i64, subsec_nanos: u32) -> i64 {
    let ticks = (i128::from(unix_secs) + UNIX_TO_WINDOWS_EPOCH_SECS) * TICKS_PER_SECOND
        + i128::from(subsec_nanos / 100);
    ticks.clamp(0, i128::from(i64::MAX)) as i64
}

/// Computes a deterministic 16-byte ProjFS virtualization instance GUID from `(git_dir, workspace)`.
#[must_use]
pub fn compute_instance_guid(git_dir: &Path, workspace: &str) -> [u8; 16] {
    let normalized_git = normalize_lexical(git_dir);
    let git_str = normalized_git.to_string_lossy().replace('\\', "/");
    let git_trimmed = git_str.trim_end_matches('/');

    // 128-bit FNV-1a (RFC-stable across platforms and Rust toolchain versions).
    const FNV_OFFSET_128: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const FNV_PRIME_128: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

    let mut hash = FNV_OFFSET_128;
    for byte in b"ozone-projfs-instance-v1\0"
        .iter()
        .chain(git_trimmed.as_bytes())
        .chain(b"\0")
        .chain(workspace.as_bytes())
    {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME_128);
    }

    let mut bytes = hash.to_be_bytes();
    // Stamp RFC 4122 v5/variant bits so Windows treats it as a canonical GUID.
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
}

/// Case-insensitive UTF-16 uppercase ordering matching Windows `PrjFileNameCompare`.
#[must_use]
pub fn prj_file_name_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let up_a = utf16_upper_units(a);
    let up_b = utf16_upper_units(b);
    up_a.cmp(&up_b).then_with(|| a.cmp(b))
}

fn utf16_upper_units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    for unit in s.encode_utf16() {
        if let Some(ch) = char::from_u32(u32::from(unit)) {
            for up in ch.to_uppercase() {
                let mut buf = [0u16; 2];
                out.extend_from_slice(up.encode_utf16(&mut buf));
            }
        } else {
            out.push(unit);
        }
    }
    out
}

/// Case-insensitive Windows wildcard matcher supporting `*`, `?`, and DOS `<`, `>`, `"`
/// with `FsRtlIsNameInExpression` / `PrjFileNameMatch` semantics.
#[must_use]
pub fn prj_file_name_match(name: &str, search_expr: &str) -> bool {
    if search_expr.is_empty() || search_expr == "*" {
        return true;
    }

    let name_chars: Vec<char> = name.chars().flat_map(char::to_uppercase).collect();
    let pat_chars: Vec<char> = search_expr.chars().flat_map(char::to_uppercase).collect();
    let n = name_chars.len();
    let m = pat_chars.len();
    let last_dot_idx = name_chars.iter().rposition(|&c| c == '.');

    let mut next_row = vec![false; n + 1];
    next_row[n] = true;

    for pi in (0..m).rev() {
        let mut curr_row = vec![false; n + 1];
        match pat_chars[pi] {
            '*' => {
                for ni in (0..=n).rev() {
                    curr_row[ni] = next_row[ni] || (ni < n && curr_row[ni + 1]);
                }
            }
            '<' => {
                for ni in (0..=n).rev() {
                    curr_row[ni] = next_row[ni]
                        || (ni < n && Some(ni) != last_dot_idx && curr_row[ni + 1]);
                }
            }
            '?' => {
                curr_row[..n].copy_from_slice(&next_row[1..=n]);
            }
            '>' => {
                curr_row[n] = next_row[n];
                for ni in 0..n {
                    curr_row[ni] = if name_chars[ni] == '.' {
                        next_row[ni]
                    } else {
                        next_row[ni + 1]
                    };
                }
            }
            '"' => {
                curr_row[n] = next_row[n];
                for ni in 0..n {
                    curr_row[ni] = name_chars[ni] == '.' && next_row[ni + 1];
                }
            }
            ch => {
                for ni in 0..n {
                    curr_row[ni] = name_chars[ni] == ch && next_row[ni + 1];
                }
            }
        }
        next_row = curr_row;
    }

    next_row[0]
}

fn is_hidden_marker_name(name: &str) -> bool {
    name.ends_with(WHITEOUT_SUFFIX)
        || name.ends_with(OPAQUE_SUFFIX)
        || name.ends_with(".oz_opq")
}

/// Normalizes a virtual relative path (accepting both `/` and `\` separators) while rejecting `..`.
pub fn normalize_rel_path(rel: impl AsRef<Path>) -> io::Result<PathBuf> {
    let raw = rel.as_ref().to_string_lossy();
    if raw.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains NUL byte",
        ));
    }
    let mut out = PathBuf::new();
    for seg in raw.split(['/', '\\']) {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." || seg.contains(':') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid virtual relative path component '{seg}'"),
            ));
        }
        out.push(seg);
    }
    Ok(out)
}

fn rel_to_windows_string(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(p) => Some(p.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\\")
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

fn is_dot_git_path(rel: &Path) -> bool {
    rel.components()
        .find_map(|c| match c {
            Component::Normal(p) => Some(p == OsStr::new(".git")),
            _ => None,
        })
        .unwrap_or(false)
}

/// Directory entry metadata returned by [`ProjFsLayerEngine::enumerate_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjFsDirEntry {
    pub name: String,
    pub is_dir: bool,
    pub file_size: u64,
    pub creation_time: i64,
    pub last_access_time: i64,
    pub last_write_time: i64,
    pub change_time: i64,
    pub file_attributes: u32,
    pub layer: LayerKind,
    pub host_path: PathBuf,
}

impl ProjFsDirEntry {
    /// Converts entry metadata into the C-ABI `PRJ_FILE_BASIC_INFO` layout.
    #[must_use]
    pub fn to_basic_info(&self) -> PrjFileBasicInfo {
        PrjFileBasicInfo {
            is_directory: u8::from(self.is_dir),
            file_size: if self.is_dir {
                0
            } else {
                i64::try_from(self.file_size).unwrap_or(i64::MAX)
            },
            creation_time: self.creation_time,
            last_access_time: self.last_access_time,
            last_write_time: self.last_write_time,
            change_time: self.change_time,
            file_attributes: self.file_attributes,
        }
    }
}

/// Placeholder metadata returned by [`ProjFsLayerEngine::placeholder_info`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjFsPlaceholderInfo {
    pub rel_path: String,
    pub windows_rel_path: String,
    pub host_path: PathBuf,
    pub layer: LayerKind,
    pub is_dir: bool,
    pub file_size: u64,
    pub creation_time: i64,
    pub last_access_time: i64,
    pub last_write_time: i64,
    pub change_time: i64,
    pub file_attributes: u32,
}

impl ProjFsPlaceholderInfo {
    /// Converts placeholder metadata into the C-ABI `PRJ_FILE_BASIC_INFO` layout.
    #[must_use]
    pub fn to_basic_info(&self) -> PrjFileBasicInfo {
        PrjFileBasicInfo {
            is_directory: u8::from(self.is_dir),
            file_size: if self.is_dir {
                0
            } else {
                i64::try_from(self.file_size).unwrap_or(i64::MAX)
            },
            creation_time: self.creation_time,
            last_access_time: self.last_access_time,
            last_write_time: self.last_write_time,
            change_time: self.change_time,
            file_attributes: self.file_attributes,
        }
    }
}

/// Un-gated CoW virtualization engine backing Windows ProjFS over `[upper, seed, baseline, repo_root]`.
#[derive(Debug, Clone)]
pub struct ProjFsLayerEngine {
    pub config: OverlayMountConfig,
    pub repo_root: Option<PathBuf>,
}

impl ProjFsLayerEngine {
    /// Wraps an [`OverlayMountConfig`] directly.
    #[must_use]
    pub fn new(config: OverlayMountConfig) -> Self {
        Self {
            config,
            repo_root: None,
        }
    }

    /// Builds a [`ProjFsLayerEngine`] from an IPC [`MountSpec`], enabling `repo_root` fallback
    /// when neither `seed_dir` nor `baseline_dir` is present.
    #[must_use]
    pub fn from_spec(spec: &MountSpec) -> Self {
        Self {
            config: OverlayMountConfig::from_spec(spec),
            repo_root: Some(spec.repo_root.clone()).filter(|p| p.is_dir()),
        }
    }

    /// Attaches an optional `repo_root` fallback path.
    #[must_use]
    pub fn with_repo_root(mut self, repo_root: impl Into<PathBuf>) -> Self {
        self.repo_root = Some(repo_root.into());
        self
    }

    fn effective_config(&self) -> (OverlayMountConfig, bool) {
        if self.config.seed_dir.is_none()
            && self.config.baseline_dir.is_none()
            && let Some(ref rr) = self.repo_root
            && rr.is_dir()
        {
            let mut eff = self.config.clone();
            eff.baseline_dir = Some(rr.clone());
            return (eff, true);
        }
        (self.config.clone(), false)
    }

    fn resolve_entry(&self, norm_rel: &Path) -> Option<ResolvedEntry> {
        let (eff_cfg, using_repo_fallback) = self.effective_config();
        if using_repo_fallback && is_dot_git_path(norm_rel) {
            return None;
        }
        if let Some(name) = norm_rel.file_name().and_then(|n| n.to_str())
            && is_hidden_marker_name(name)
        {
            return None;
        }
        resolve_rel_path(&eff_cfg, norm_rel)
    }

    fn resolve_in_lower(&self, norm_rel: &Path) -> Option<(LayerKind, PathBuf, std::fs::Metadata)> {
        let (eff_cfg, using_repo_fallback) = self.effective_config();
        if using_repo_fallback && is_dot_git_path(norm_rel) {
            return None;
        }
        resolve_in_lower_layers(&eff_cfg, norm_rel)
    }

    /// Lists merged directory entries across `[upper, seed, baseline, repo_root]`, sorted in
    /// case-insensitive `PrjFileNameCompare` order, wildcard-matched, hiding `.oz_wh`/`.oz_opq`
    /// markers, and preserving exact source/baseline `mtime` as Windows `FILETIME`.
    pub fn enumerate_dir(
        &self,
        rel_dir: impl AsRef<Path>,
        search_expr: Option<&str>,
    ) -> io::Result<Vec<ProjFsDirEntry>> {
        let norm_dir = normalize_rel_path(rel_dir)?;
        let (eff_cfg, using_repo_fallback) = self.effective_config();

        if !norm_dir.as_os_str().is_empty() {
            if using_repo_fallback && is_dot_git_path(&norm_dir) {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            let Some(resolved) = resolve_rel_path(&eff_cfg, &norm_dir) else {
                return Err(io::Error::from(io::ErrorKind::NotFound));
            };
            let meta = std::fs::metadata(&resolved.host_path)
                .or_else(|_| std::fs::symlink_metadata(&resolved.host_path))?;
            if !meta.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("{} is not a directory", norm_dir.display()),
                ));
            }
        }

        let merged: Vec<MergedDirEntry> = list_merged_dir_entries(&eff_cfg, &norm_dir)?;
        let is_root = norm_dir.as_os_str().is_empty();
        let filter_expr = search_expr.filter(|s| !s.is_empty() && *s != "*");

        let mut out = Vec::with_capacity(merged.len());
        for entry in merged {
            let name = entry.name.to_string_lossy().into_owned();
            if is_hidden_marker_name(&name) {
                continue;
            }
            if using_repo_fallback && is_root && name == ".git" {
                continue;
            }
            if let Some(expr) = filter_expr
                && !prj_file_name_match(&name, expr)
            {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&entry.host_path)
                .or_else(|_| std::fs::symlink_metadata(&entry.host_path))
            else {
                continue;
            };
            let (is_dir, file_size, ctime, atime, mtime, attrs) = extract_file_times_and_attrs(&meta);
            out.push(ProjFsDirEntry {
                name,
                is_dir,
                file_size,
                creation_time: ctime,
                last_access_time: atime,
                last_write_time: mtime,
                change_time: mtime,
                file_attributes: attrs,
                layer: entry.layer,
                host_path: entry.host_path,
            });
        }

        out.sort_by(|a, b| prj_file_name_compare(&a.name, &b.name));
        Ok(out)
    }

    /// Resolves placeholder metadata for `rel_path` across layers.
    pub fn placeholder_info(
        &self,
        rel_path: impl AsRef<Path>,
    ) -> io::Result<Option<ProjFsPlaceholderInfo>> {
        let norm_rel = normalize_rel_path(rel_path)?;
        let Some(resolved) = self.resolve_entry(&norm_rel) else {
            return Ok(None);
        };
        let meta = match std::fs::metadata(&resolved.host_path)
            .or_else(|_| std::fs::symlink_metadata(&resolved.host_path))
        {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let (is_dir, file_size, ctime, atime, mtime, attrs) = extract_file_times_and_attrs(&meta);
        Ok(Some(ProjFsPlaceholderInfo {
            rel_path: rel_to_slash_string(&norm_rel),
            windows_rel_path: rel_to_windows_string(&norm_rel),
            host_path: resolved.host_path,
            layer: resolved.layer,
            is_dir,
            file_size,
            creation_time: ctime,
            last_access_time: atime,
            last_write_time: mtime,
            change_time: mtime,
            file_attributes: attrs,
        }))
    }

    /// Reads up to `length` bytes starting at `byte_offset` from the resolved layer for `rel_path`.
    pub fn read_hydration_data(
        &self,
        rel_path: impl AsRef<Path>,
        byte_offset: u64,
        length: usize,
    ) -> io::Result<Vec<u8>> {
        let norm_rel = normalize_rel_path(rel_path)?;
        if norm_rel.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot read hydration data for virtualization root",
            ));
        }
        let Some(resolved) = self.resolve_entry(&norm_rel) else {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        };
        let meta = std::fs::metadata(&resolved.host_path)
            .or_else(|_| std::fs::symlink_metadata(&resolved.host_path))?;
        if meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is a directory", norm_rel.display()),
            ));
        }
        if length == 0 {
            return Ok(Vec::new());
        }

        let mut file = File::open(&resolved.host_path)?;
        file.seek(SeekFrom::Start(byte_offset))?;
        let mut buf = vec![0u8; length];
        let mut total = 0usize;
        while total < length {
            match file.read(&mut buf[total..]) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        buf.truncate(total);
        Ok(buf)
    }

    /// Persists a created/modified file or directory from `mount_point/<rel_path>` into
    /// `upper_dir/<rel_path>`, clearing any `.oz_wh` tombstone and recording `.oz_opaque`
    /// if a previously deleted or lower-layer directory was recreated.
    pub fn sync_written_file(&self, rel_path: impl AsRef<Path>) -> io::Result<()> {
        let norm_rel = normalize_rel_path(rel_path)?;
        if norm_rel.as_os_str().is_empty() {
            return Ok(());
        }
        let src = self.config.mount_point.join(&norm_rel);
        let src_meta = match std::fs::symlink_metadata(&src) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };

        self.ensure_upper_ancestors(&norm_rel)?;

        let wh_path = marker_path(&self.config.whiteouts_dir(), &norm_rel, WHITEOUT_SUFFIX);
        let had_whiteout = wh_path.exists();
        remove_file_if_exists(&wh_path)?;

        let upper_target = self.config.upper_dir.join(&norm_rel);
        if src_meta.is_dir() {
            if let Ok(existing_meta) = std::fs::symlink_metadata(&upper_target)
                && !existing_meta.is_dir()
            {
                std::fs::remove_file(&upper_target)?;
            }
            std::fs::create_dir_all(&upper_target)?;
            if had_whiteout || self.resolve_in_lower(&norm_rel).is_some() {
                write_marker_file(&marker_path(
                    &self.config.opaque_dir(),
                    &norm_rel,
                    OPAQUE_SUFFIX,
                ))?;
            }
            return Ok(());
        }

        if let Ok(existing_meta) = std::fs::symlink_metadata(&upper_target)
            && existing_meta.is_dir()
        {
            std::fs::remove_dir_all(&upper_target)?;
        }
        self.copy_file_to_upper(&src, &upper_target, src_meta.modified().ok())
    }

    /// Removes `rel_path` from `upper_dir` and writes `.oz_wh` tombstones in `meta/whiteouts/`
    /// when the path exists in lower layers (`seed/` / `baseline/` / `repo_root`).
    pub fn record_deleted_path(&self, rel_path: impl AsRef<Path>) -> io::Result<()> {
        let norm_rel = normalize_rel_path(rel_path)?;
        if norm_rel.as_os_str().is_empty() {
            return Ok(());
        }

        let upper_target = self.config.upper_dir.join(&norm_rel);
        if let Ok(meta) = std::fs::symlink_metadata(&upper_target) {
            if meta.is_dir() {
                std::fs::remove_dir_all(&upper_target)?;
            } else {
                std::fs::remove_file(&upper_target)?;
            }
        }

        remove_subtree_markers(&self.config.opaque_dir(), &norm_rel, OPAQUE_SUFFIX)?;

        let parent_rel = norm_rel.parent().map(Path::to_path_buf).unwrap_or_default();
        let (eff_cfg, _) = self.effective_config();
        if let Some((_, _, lower_meta)) = self.resolve_in_lower(&norm_rel)
            && !lower_layers_blocked_at_dir(&eff_cfg, &parent_rel)
        {
            write_marker_file(&marker_path(
                &self.config.whiteouts_dir(),
                &norm_rel,
                WHITEOUT_SUFFIX,
            ))?;
            if lower_meta.is_dir() {
                for child_rel in self.collect_lower_files_under(&norm_rel) {
                    write_marker_file(&marker_path(
                        &self.config.whiteouts_dir(),
                        &child_rel,
                        WHITEOUT_SUFFIX,
                    ))?;
                }
            }
        }

        Ok(())
    }

    /// Mirrors a file or directory rename from `old_rel` to `new_rel` into `upper_dir` and
    /// `meta/{whiteouts,opaque}` so `scan_dirty_paths` observes the rename identically to FUSE3.
    pub fn record_renamed_path(
        &self,
        old_rel: impl AsRef<Path>,
        new_rel: impl AsRef<Path>,
    ) -> io::Result<()> {
        let old_norm = normalize_rel_path(old_rel)?;
        let new_norm = normalize_rel_path(new_rel)?;

        if old_norm.as_os_str().is_empty() && new_norm.as_os_str().is_empty() {
            return Ok(());
        }
        if old_norm.as_os_str().is_empty() {
            return self.sync_written_file(&new_norm);
        }
        if new_norm.as_os_str().is_empty() {
            return self.record_deleted_path(&old_norm);
        }
        if old_norm == new_norm {
            return Ok(());
        }

        // Case-only renames on case-insensitive Windows NTFS must not create a whiteout
        // that would shadow the renamed target in `whiteouts/`.
        if cfg!(windows)
            && rel_to_slash_string(&old_norm).eq_ignore_ascii_case(&rel_to_slash_string(&new_norm))
        {
            return self.handle_case_only_rename(&old_norm, &new_norm);
        }

        let mount_new = self.config.mount_point.join(&new_norm);
        let old_resolved = self.resolve_entry(&old_norm);
        let old_is_dir = old_resolved
            .as_ref()
            .and_then(|r| std::fs::symlink_metadata(&r.host_path).ok())
            .is_some_and(|m| m.is_dir())
            || std::fs::symlink_metadata(&mount_new).is_ok_and(|m| m.is_dir());

        self.ensure_upper_ancestors(&new_norm)?;
        let dst_wh = marker_path(&self.config.whiteouts_dir(), &new_norm, WHITEOUT_SUFFIX);
        let dst_had_wh = dst_wh.exists();
        remove_file_if_exists(&dst_wh)?;

        if old_is_dir {
            let upper_new = self.config.upper_dir.join(&new_norm);
            if let Ok(m) = std::fs::symlink_metadata(&upper_new)
                && !m.is_dir()
            {
                std::fs::remove_file(&upper_new)?;
            }
            std::fs::create_dir_all(&upper_new)?;

            if dst_had_wh || self.resolve_in_lower(&new_norm).is_some() {
                write_marker_file(&marker_path(
                    &self.config.opaque_dir(),
                    &new_norm,
                    OPAQUE_SUFFIX,
                ))?;
            }

            if old_resolved.is_some() {
                self.copy_merged_tree_to_upper(&old_norm, &new_norm)?;
            }
            if mount_new.is_dir() {
                self.sync_real_files_from_mount_tree(&mount_new, &new_norm)?;
            }
            self.clear_child_whiteouts_for_upper_dir(&new_norm)?;
            return self.record_deleted_path(&old_norm);
        }

        let upper_new = self.config.upper_dir.join(&new_norm);
        if let Ok(m) = std::fs::symlink_metadata(&upper_new)
            && m.is_dir()
        {
            std::fs::remove_dir_all(&upper_new)?;
        }

        if is_real_non_placeholder_file(&mount_new) {
            let mtime = std::fs::symlink_metadata(&mount_new)
                .ok()
                .and_then(|m| m.modified().ok());
            self.copy_file_to_upper(&mount_new, &upper_new, mtime)?;
        } else if let Some(ref resolved) = old_resolved {
            let mtime = std::fs::metadata(&resolved.host_path)
                .ok()
                .and_then(|m| m.modified().ok());
            self.copy_file_to_upper(&resolved.host_path, &upper_new, mtime)?;
        }

        self.record_deleted_path(&old_norm)
    }

    /// Scans `upper_dir` and `meta_dir` using [`scan_dirty_paths`].
    pub fn scan_dirty(&self) -> io::Result<Vec<DirtyPathEntry>> {
        scan_dirty_paths(&self.config)
    }

    fn handle_case_only_rename(&self, old_norm: &Path, new_norm: &Path) -> io::Result<()> {
        let upper_old = self.config.upper_dir.join(old_norm);
        let upper_new = self.config.upper_dir.join(new_norm);
        if std::fs::symlink_metadata(&upper_old).is_ok() {
            std::fs::rename(&upper_old, &upper_new)?;
        } else {
            let mount_new = self.config.mount_point.join(new_norm);
            if is_real_non_placeholder_file(&mount_new) {
                let mtime = std::fs::symlink_metadata(&mount_new)
                    .ok()
                    .and_then(|m| m.modified().ok());
                self.ensure_upper_ancestors(new_norm)?;
                self.copy_file_to_upper(&mount_new, &upper_new, mtime)?;
            } else if let Some(resolved) = self.resolve_entry(old_norm) {
                let mtime = std::fs::metadata(&resolved.host_path)
                    .ok()
                    .and_then(|m| m.modified().ok());
                self.ensure_upper_ancestors(new_norm)?;
                self.copy_file_to_upper(&resolved.host_path, &upper_new, mtime)?;
            }
        }
        Ok(())
    }

    fn ensure_upper_ancestors(&self, rel: &Path) -> io::Result<()> {
        let Some(parent) = rel.parent() else {
            return std::fs::create_dir_all(&self.config.upper_dir);
        };
        std::fs::create_dir_all(&self.config.upper_dir)?;
        for anc in ancestors_inclusive(parent) {
            let wh_path = marker_path(&self.config.whiteouts_dir(), &anc, WHITEOUT_SUFFIX);
            let had_wh = wh_path.exists();
            remove_file_if_exists(&wh_path)?;

            let upper_anc = self.config.upper_dir.join(&anc);
            if let Ok(m) = std::fs::symlink_metadata(&upper_anc)
                && !m.is_dir()
            {
                std::fs::remove_file(&upper_anc)?;
            }
            std::fs::create_dir_all(&upper_anc)?;
            if had_wh {
                write_marker_file(&marker_path(&self.config.opaque_dir(), &anc, OPAQUE_SUFFIX))?;
            }
        }
        Ok(())
    }

    fn copy_merged_tree_to_upper(&self, src_dir: &Path, dst_dir: &Path) -> io::Result<()> {
        let entries = match self.enumerate_dir(src_dir, None) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let src_child = src_dir.join(&entry.name);
            let dst_child = dst_dir.join(&entry.name);
            if entry.is_dir {
                std::fs::create_dir_all(self.config.upper_dir.join(&dst_child))?;
                self.copy_merged_tree_to_upper(&src_child, &dst_child)?;
            } else {
                let mtime = std::fs::metadata(&entry.host_path)
                    .ok()
                    .and_then(|m| m.modified().ok());
                let upper_dst = self.config.upper_dir.join(&dst_child);
                self.copy_file_to_upper(&entry.host_path, &upper_dst, mtime)?;
            }
        }
        Ok(())
    }

    fn sync_real_files_from_mount_tree(&self, mount_dir: &Path, dst_rel_root: &Path) -> io::Result<()> {
        for entry in WalkDir::new(mount_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            let Ok(tail) = entry.path().strip_prefix(mount_dir) else {
                continue;
            };
            if tail.as_os_str().is_empty() {
                continue;
            }
            let target_rel = dst_rel_root.join(tail);
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(self.config.upper_dir.join(&target_rel))?;
                continue;
            }
            if is_real_non_placeholder_file(entry.path()) {
                let mtime = entry.metadata().ok().and_then(|m| m.modified().ok());
                self.copy_file_to_upper(
                    entry.path(),
                    &self.config.upper_dir.join(&target_rel),
                    mtime,
                )?;
            }
        }
        Ok(())
    }

    fn clear_child_whiteouts_for_upper_dir(&self, dst_rel: &Path) -> io::Result<()> {
        let upper_dst = self.config.upper_dir.join(dst_rel);
        for ent in WalkDir::new(&upper_dst)
            .follow_links(false)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            if let Ok(tail) = ent.path().strip_prefix(&upper_dst)
                && !tail.as_os_str().is_empty()
            {
                let child_rel = dst_rel.join(tail);
                remove_file_if_exists(&marker_path(
                    &self.config.whiteouts_dir(),
                    &child_rel,
                    WHITEOUT_SUFFIX,
                ))?;
            }
        }
        Ok(())
    }

    fn collect_lower_files_under(&self, rel_dir: &Path) -> BTreeSet<PathBuf> {
        let mut out = BTreeSet::new();
        let (eff_cfg, using_repo_fallback) = self.effective_config();
        let seed_whiteouts = eff_cfg.seed_whiteouts_dir();

        if let Some(ref seed) = eff_cfg.seed_dir {
            let sub = seed.join(rel_dir);
            if sub.is_dir() {
                for ent in WalkDir::new(&sub)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(std::result::Result::ok)
                {
                    if ent.file_type().is_dir() {
                        continue;
                    }
                    if let Ok(tail) = ent.path().strip_prefix(&sub) {
                        out.insert(rel_dir.join(tail));
                    }
                }
            }
        }

        if let Some(ref base) = eff_cfg.baseline_dir {
            let sub = base.join(rel_dir);
            if sub.is_dir() {
                for ent in WalkDir::new(&sub)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(std::result::Result::ok)
                {
                    if ent.file_type().is_dir() {
                        continue;
                    }
                    if let Ok(tail) = ent.path().strip_prefix(&sub) {
                        let full = rel_dir.join(tail);
                        if using_repo_fallback && is_dot_git_path(&full) {
                            continue;
                        }
                        if !ancestors_inclusive(&full).iter().any(|pref| {
                            marker_path(&seed_whiteouts, pref, WHITEOUT_SUFFIX).exists()
                        }) {
                            out.insert(full);
                        }
                    }
                }
            }
        }

        out
    }

    fn copy_file_to_upper(
        &self,
        src: &Path,
        dst: &Path,
        mtime: Option<SystemTime>,
    ) -> io::Result<()> {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp_dir = self.config.meta_dir.join("tmp");
        std::fs::create_dir_all(&tmp_dir)?;
        let seq = NEXT_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp_path = tmp_dir.join(format!("projfs.{}.{seq}", std::process::id()));

        let mut reader = File::open(src)?;
        let mut writer = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp_path)?;
        if let Err(err) = io::copy(&mut reader, &mut writer) {
            drop(writer);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
        if let Some(ts) = mtime {
            let _ = writer.set_times(FileTimes::new().set_modified(ts));
        }
        drop(writer);

        if std::fs::rename(&tmp_path, dst).is_err() {
            std::fs::copy(&tmp_path, dst).inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp_path);
            })?;
            let _ = std::fs::remove_file(&tmp_path);
            if let Some(ts) = mtime
                && let Ok(f) = OpenOptions::new().write(true).open(dst)
            {
                let _ = f.set_times(FileTimes::new().set_modified(ts));
            }
        }
        Ok(())
    }
}

static NEXT_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn is_real_non_placeholder_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
        let attrs = meta.file_attributes();
        if (attrs & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_OFFLINE)) != 0 {
            return false;
        }
    }
    true
}

fn extract_file_times_and_attrs(meta: &std::fs::Metadata) -> (bool, u64, i64, i64, i64, u32) {
    let is_dir = meta.is_dir();
    let file_size = if is_dir { 0 } else { meta.len() };
    let mtime_st = meta.modified().unwrap_or(UNIX_EPOCH);
    let mtime = system_time_to_filetime(mtime_st);
    let ctime = meta.created().map(system_time_to_filetime).unwrap_or(mtime);
    let atime = meta.accessed().map(system_time_to_filetime).unwrap_or(mtime);
    let attrs = if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_ARCHIVE
    };
    (is_dir, file_size, ctime, atime, mtime, attrs)
}

fn write_marker_file(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    File::create(path)?;
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn remove_subtree_markers(root: &Path, rel: &Path, suffix: &str) -> io::Result<()> {
    remove_file_if_exists(&marker_path(root, rel, suffix))?;
    let dir_path = root.join(rel);
    if dir_path.is_dir() {
        std::fs::remove_dir_all(dir_path)?;
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Un-gated C-ABI structs & enumeration session state (validated on Linux & Windows)
// ----------------------------------------------------------------------------

/// Windows `GUID` ABI representation (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrjGuid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl PrjGuid {
    /// Constructs a [`PrjGuid`] from RFC 4122 big-endian bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self {
            data1: u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            data2: u16::from_be_bytes([bytes[4], bytes[5]]),
            data3: u16::from_be_bytes([bytes[6], bytes[7]]),
            data4: [
                bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                bytes[15],
            ],
        }
    }

    /// Converts back to canonical 16-byte representation.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 16] {
        let d1 = self.data1.to_be_bytes();
        let d2 = self.data2.to_be_bytes();
        let d3 = self.data3.to_be_bytes();
        [
            d1[0], d1[1], d1[2], d1[3], d2[0], d2[1], d3[0], d3[1], self.data4[0], self.data4[1],
            self.data4[2], self.data4[3], self.data4[4], self.data4[5], self.data4[6],
            self.data4[7],
        ]
    }
}

/// C-ABI `PRJ_FILE_BASIC_INFO` struct.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrjFileBasicInfo {
    pub is_directory: u8,
    pub file_size: i64,
    pub creation_time: i64,
    pub last_access_time: i64,
    pub last_write_time: i64,
    pub change_time: i64,
    pub file_attributes: u32,
}

/// C-ABI `PRJ_PLACEHOLDER_VERSION_INFO` struct.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PrjPlaceholderVersionInfo {
    pub provider_id: [u8; 128],
    pub content_id: [u8; 128],
}

/// C-ABI `PRJ_PLACEHOLDER_INFO` struct.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PrjPlaceholderInfo {
    pub file_basic_info: PrjFileBasicInfo,
    pub ea_buffer_size: u32,
    pub offset_to_first_ea: u32,
    pub security_buffer_size: u32,
    pub offset_to_security_descriptor: u32,
    pub streams_info_buffer_size: u32,
    pub offset_to_first_stream_info: u32,
    pub version_info: PrjPlaceholderVersionInfo,
    pub variable_data: [u8; 1],
}

impl PrjPlaceholderInfo {
    /// Builds a zero-EA/security/streams `PRJ_PLACEHOLDER_INFO` header from `basic`.
    #[must_use]
    pub const fn from_basic(basic: PrjFileBasicInfo) -> Self {
        Self {
            file_basic_info: basic,
            ea_buffer_size: 0,
            offset_to_first_ea: 0,
            security_buffer_size: 0,
            offset_to_security_descriptor: 0,
            streams_info_buffer_size: 0,
            offset_to_first_stream_info: 0,
            version_info: PrjPlaceholderVersionInfo {
                provider_id: [0u8; 128],
                content_id: [0u8; 128],
            },
            variable_data: [0u8; 1],
        }
    }
}

/// C-ABI `PRJ_CALLBACK_DATA` struct.
#[repr(C)]
pub struct PrjCallbackData {
    pub size: u32,
    pub flags: u32,
    pub namespace_virtualization_context: *mut core::ffi::c_void,
    pub command_id: i32,
    pub file_id: PrjGuid,
    pub data_stream_id: PrjGuid,
    pub file_path_name: *const u16,
    pub version_info: *const PrjPlaceholderVersionInfo,
    pub triggering_process_id: u32,
    pub triggering_process_image_file_name: *const u16,
    pub instance_context: *mut core::ffi::c_void,
}

/// C-ABI `PRJ_NOTIFICATION_MAPPING` struct.
#[repr(C)]
pub struct PrjNotificationMapping {
    pub notification_bit_mask: u32,
    pub notification_root: *const u16,
}

/// C-ABI `PRJ_STARTVIRTUALIZING_OPTIONS` struct.
#[repr(C)]
pub struct PrjStartVirtualizingOptions {
    pub flags: u32,
    pub pool_thread_count: u32,
    pub concurrent_thread_count: u32,
    pub notification_mappings: *mut PrjNotificationMapping,
    pub notification_mappings_count: u32,
}

/// C-ABI `PRJ_CALLBACKS` struct.
#[repr(C)]
pub struct PrjCallbacks {
    pub start_directory_enumeration:
        Option<unsafe extern "system" fn(*const PrjCallbackData, *const PrjGuid) -> i32>,
    pub end_directory_enumeration:
        Option<unsafe extern "system" fn(*const PrjCallbackData, *const PrjGuid) -> i32>,
    pub get_directory_enumeration: Option<
        unsafe extern "system" fn(
            *const PrjCallbackData,
            *const PrjGuid,
            *const u16,
            *mut core::ffi::c_void,
        ) -> i32,
    >,
    pub get_placeholder_info: Option<unsafe extern "system" fn(*const PrjCallbackData) -> i32>,
    pub get_file_data:
        Option<unsafe extern "system" fn(*const PrjCallbackData, u64, u32) -> i32>,
    pub query_file_name: Option<unsafe extern "system" fn(*const PrjCallbackData) -> i32>,
    pub notification: Option<
        unsafe extern "system" fn(
            *const PrjCallbackData,
            u8,
            u32,
            *const u16,
            *mut core::ffi::c_void,
        ) -> i32,
    >,
    pub cancel_command: Option<unsafe extern "system" fn(*const PrjCallbackData)>,
}

/// Per-enumeration cache state shared by ProjFS directory enumeration callbacks.
#[derive(Debug, Default)]
pub struct ProjFsEnumCache {
    sessions: std::collections::HashMap<[u8; 16], EnumSessionState>,
}

#[derive(Debug, Default)]
struct EnumSessionState {
    populated: bool,
    entries: Vec<ProjFsDirEntry>,
    index: usize,
}

impl ProjFsEnumCache {
    pub fn start(&mut self, enum_id: [u8; 16]) {
        self.sessions.insert(enum_id, EnumSessionState::default());
    }

    pub fn end(&mut self, enum_id: &[u8; 16]) {
        self.sessions.remove(enum_id);
    }

    pub fn prepare(
        &mut self,
        enum_id: [u8; 16],
        restart: bool,
        populate: impl FnOnce() -> io::Result<Vec<ProjFsDirEntry>>,
    ) -> io::Result<&mut [ProjFsDirEntry]> {
        let session = self.sessions.entry(enum_id).or_default();
        if restart || !session.populated {
            session.entries = populate()?;
            session.index = 0;
            session.populated = true;
        }
        Ok(&mut session.entries)
    }

    pub fn cursor_slice(&mut self, enum_id: &[u8; 16]) -> (&[ProjFsDirEntry], &mut usize) {
        let session = self.sessions.entry(*enum_id).or_default();
        (&session.entries, &mut session.index)
    }
}

/// Steps through `entries[*idx..]` invoking `fill`, returning `ERROR_INSUFFICIENT_BUFFER_HR`
/// when `fill` runs out of buffer space on the very first entry (`added == 0`) or `PRJ_S_OK`
/// once at least one entry has been emitted (or enumeration reaches EOF).
pub fn step_dir_enum_buffer(
    entries: &[ProjFsDirEntry],
    idx: &mut usize,
    single: bool,
    mut fill: impl FnMut(&ProjFsDirEntry) -> i32,
) -> i32 {
    let mut added = 0usize;
    while *idx < entries.len() {
        let hr = fill(&entries[*idx]);
        if hr == ERROR_INSUFFICIENT_BUFFER_HR {
            return if added == 0 {
                ERROR_INSUFFICIENT_BUFFER_HR
            } else {
                PRJ_S_OK
            };
        }
        if hr < 0 {
            return hr;
        }
        *idx += 1;
        added += 1;
        if single {
            break;
        }
    }
    PRJ_S_OK
}

#[allow(dead_code)]
const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;
#[allow(dead_code)]
const PRJ_S_OK: i32 = 0;
#[allow(dead_code)]
const PRJ_E_FAIL: i32 = 0x8000_4005_u32 as i32;
#[allow(dead_code)]
const PRJ_E_OUTOFMEMORY: i32 = 0x8007_000E_u32 as i32;
#[allow(dead_code)]
const ERROR_FILE_NOT_FOUND_HR: i32 = 0x8007_0002_u32 as i32;
#[allow(dead_code)]
const ERROR_INSUFFICIENT_BUFFER_HR: i32 = 0x8007_007A_u32 as i32;
#[allow(dead_code)]
const ERROR_REPARSE_ENCOUNTERED_HR: i32 = 0x8007_112B_u32 as i32;
#[allow(dead_code)]
const ERROR_ALREADY_EXISTS_HR: i32 = 0x8007_00B7_u32 as i32;
#[allow(dead_code)]
const PRJ_NOTIFY_MASK: u32 = PRJ_NOTIFY_NEW_FILE_CREATED
    | PRJ_NOTIFY_FILE_OVERWRITTEN
    | PRJ_NOTIFY_PRE_DELETE
    | PRJ_NOTIFY_FILE_RENAMED
    | PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED
    | PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED;

/// Dispatches a ProjFS notification mask to the un-gated CoW persistence engine.
pub fn dispatch_projfs_notification(
    engine: &ProjFsLayerEngine,
    notification: u32,
    rel_path: &str,
    dest_path: Option<&str>,
) -> io::Result<()> {
    match notification {
        PRJ_NOTIFY_NEW_FILE_CREATED
        | PRJ_NOTIFY_FILE_OVERWRITTEN
        | PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED => engine.sync_written_file(rel_path),
        PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED => engine.record_deleted_path(rel_path),
        PRJ_NOTIFY_FILE_RENAMED => {
            engine.record_renamed_path(rel_path, dest_path.unwrap_or_default())
        }
        PRJ_NOTIFY_PRE_DELETE => Ok(()),
        _ => Ok(()),
    }
}

/// Maps a [`std::io::Error`] into a standard Win32 `HRESULT`.
#[must_use]
pub fn hresult_from_io_error(err: &io::Error) -> i32 {
    match err.kind() {
        io::ErrorKind::NotFound => ERROR_FILE_NOT_FOUND_HR,
        io::ErrorKind::PermissionDenied => 0x8007_0005_u32 as i32,
        io::ErrorKind::InvalidInput => 0x8007_0057_u32 as i32,
        _ => PRJ_E_FAIL,
    }
}

#[allow(dead_code)]
fn catch_hresult(f: impl FnOnce() -> i32) -> i32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(PRJ_E_FAIL)
}

#[allow(dead_code)]
fn encode_wide_z(os: &OsStr) -> Vec<u16> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        os.encode_wide().chain(std::iter::once(0)).collect()
    }
    #[cfg(not(windows))]
    {
        os.to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }
}

#[allow(dead_code)]
unsafe fn decode_wide_z(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) })
}

// Thin (<120 lines) Windows-only dynamic ProjectedFSLib.dll wrapper.
#[cfg(target_os = "windows")]
mod win32 {
    use super::*;
    use core::ffi::{c_char, c_void};
    use std::ffi::CStr;
    use std::sync::{Arc, Mutex};

    unsafe extern "system" {
        fn LoadLibraryExW(lp_lib: *const u16, h_file: *mut c_void, dw_flags: u32) -> *mut c_void;
        fn FreeLibrary(h_module: *mut c_void) -> i32;
        fn GetProcAddress(h_module: *mut c_void, lp_proc: *const c_char) -> *const c_void;
    }

    struct DllHandle(*mut c_void);
    impl Drop for DllHandle {
        fn drop(&mut self) { if !self.0.is_null() { unsafe { FreeLibrary(self.0); } } }
    }
    struct AlignedBuf(*mut c_void, unsafe extern "system" fn(*mut c_void));
    impl Drop for AlignedBuf {
        fn drop(&mut self) { if !self.0.is_null() { unsafe { (self.1)(self.0); } } }
    }

    pub(super) struct ProjFsApi {
        _dll: DllHandle,
        mark_dir: unsafe extern "system" fn(*const u16, *const u16, *const PrjPlaceholderVersionInfo, *const PrjGuid) -> i32,
        start_virt: unsafe extern "system" fn(*const u16, *const PrjCallbacks, *const c_void, *const PrjStartVirtualizingOptions, *mut *mut c_void) -> i32,
        stop_virt: unsafe extern "system" fn(*mut c_void),
        alloc_buf: unsafe extern "system" fn(*mut c_void, usize) -> *mut c_void,
        free_buf: unsafe extern "system" fn(*mut c_void),
        write_placeholder: unsafe extern "system" fn(*mut c_void, *const u16, *const PrjPlaceholderInfo, u32) -> i32,
        write_file_data: unsafe extern "system" fn(*mut c_void, *const PrjGuid, *mut c_void, u64, u32) -> i32,
        fill_dir_entry: unsafe extern "system" fn(*const u16, *mut PrjFileBasicInfo, *mut c_void) -> i32,
        file_name_match: unsafe extern "system" fn(*const u16, *const u16) -> u8,
        file_name_compare: unsafe extern "system" fn(*const u16, *const u16) -> i32,
    }

    impl ProjFsApi {
        fn load(ws: &str, mp: &Path) -> Result<Self> {
            let dll_w = encode_wide_z(OsStr::new("ProjectedFSLib.dll"));
            let handle = unsafe { LoadLibraryExW(dll_w.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
            if handle.is_null() {
                let err = io::Error::last_os_error();
                return Err(OzError::MountUnavailable {
                    workspace: ws.to_string(), path: mp.to_path_buf(), errno: err.raw_os_error(),
                    message: format!("failed to load ProjectedFSLib.dll (enable Client-ProjFS via `oz setup`): {err}"),
                });
            }
            let dll = DllHandle(handle);
            let sym = |name: &CStr| -> Result<*const c_void> {
                let p = unsafe { GetProcAddress(dll.0, name.as_ptr()) };
                (!p.is_null()).then_some(p).ok_or_else(|| OzError::MountUnavailable {
                    workspace: ws.to_string(), path: mp.to_path_buf(), errno: None,
                    message: format!("missing ProjectedFSLib.dll export {}", name.to_string_lossy()),
                })
            };
            unsafe {
                Ok(Self {
                    mark_dir: std::mem::transmute_copy(&sym(c"PrjMarkDirectoryAsPlaceholder")?),
                    start_virt: std::mem::transmute_copy(&sym(c"PrjStartVirtualizing")?),
                    stop_virt: std::mem::transmute_copy(&sym(c"PrjStopVirtualizing")?),
                    alloc_buf: std::mem::transmute_copy(&sym(c"PrjAllocateAlignedBuffer")?),
                    free_buf: std::mem::transmute_copy(&sym(c"PrjFreeAlignedBuffer")?),
                    write_placeholder: std::mem::transmute_copy(&sym(c"PrjWritePlaceholderInfo")?),
                    write_file_data: std::mem::transmute_copy(&sym(c"PrjWriteFileData")?),
                    fill_dir_entry: std::mem::transmute_copy(&sym(c"PrjFillDirEntryBuffer")?),
                    file_name_match: std::mem::transmute_copy(&sym(c"PrjFileNameMatch")?),
                    file_name_compare: std::mem::transmute_copy(&sym(c"PrjFileNameCompare")?),
                    _dll: dll,
                })
            }
        }
    }

    pub(super) struct WinState { engine: ProjFsLayerEngine, api: Arc<ProjFsApi>, enums: Mutex<ProjFsEnumCache> }
    unsafe fn ctx<'a>(cb: *const PrjCallbackData) -> Option<&'a WinState> {
        (!cb.is_null()).then(|| unsafe { &*cb }).and_then(|c| (!c.instance_context.is_null()).then(|| unsafe { &*(c.instance_context as *const WinState) }))
    }

    unsafe extern "system" fn start_enum_cb(cb: *const PrjCallbackData, id: *const PrjGuid) -> i32 {
        catch_hresult(|| match (unsafe { ctx(cb) }, (!id.is_null()).then(|| unsafe { *id })) {
            (Some(st), Some(g)) => { st.enums.lock().expect("projfs enum lock").start(g.to_bytes()); PRJ_S_OK }
            _ => PRJ_E_FAIL,
        })
    }
    unsafe extern "system" fn end_enum_cb(cb: *const PrjCallbackData, id: *const PrjGuid) -> i32 {
        catch_hresult(|| match (unsafe { ctx(cb) }, (!id.is_null()).then(|| unsafe { *id })) {
            (Some(st), Some(g)) => { st.enums.lock().expect("projfs enum lock").end(&g.to_bytes()); PRJ_S_OK }
            _ => PRJ_E_FAIL,
        })
    }
    unsafe extern "system" fn get_enum_cb(cb: *const PrjCallbackData, id: *const PrjGuid, expr: *const u16, buf: *mut c_void) -> i32 {
        catch_hresult(|| {
            let (Some(st), Some(g)) = (unsafe { ctx(cb) }, (!id.is_null()).then(|| unsafe { *id })) else { return PRJ_E_FAIL; };
            if buf.is_null() { return PRJ_E_FAIL; }
            let cb_ref = unsafe { &*cb };
            let rel_dir = unsafe { decode_wide_z(cb_ref.file_path_name) };
            let expr_s = (!expr.is_null()).then(|| unsafe { decode_wide_z(expr) });
            let restart = (cb_ref.flags & PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN) != 0;
            let single = (cb_ref.flags & PRJ_CB_DATA_FLAG_ENUM_RETURN_SINGLE_ENTRY) != 0;
            let mut enums = st.enums.lock().expect("projfs enum lock");
            let id_bytes = g.to_bytes();
            if let Err(e) = enums.prepare(id_bytes, restart, || {
                let mut rows = st.engine.enumerate_dir(&rel_dir, None)?;
                if let Some(ref pat) = expr_s && !pat.is_empty() && pat != "*" {
                    let pat_w = encode_wide_z(OsStr::new(pat));
                    rows.retain(|r| unsafe { (st.api.file_name_match)(encode_wide_z(OsStr::new(&r.name)).as_ptr(), pat_w.as_ptr()) != 0 });
                }
                rows.sort_by(|a, b| unsafe { (st.api.file_name_compare)(encode_wide_z(OsStr::new(&a.name)).as_ptr(), encode_wide_z(OsStr::new(&b.name)).as_ptr()).cmp(&0) });
                Ok(rows)
            }) { return hresult_from_io_error(&e); }
            let (entries, idx) = enums.cursor_slice(&id_bytes);
            step_dir_enum_buffer(entries, idx, single, |entry| {
                let mut basic = entry.to_basic_info();
                unsafe { (st.api.fill_dir_entry)(encode_wide_z(OsStr::new(&entry.name)).as_ptr(), &mut basic, buf) }
            })
        })
    }
    unsafe extern "system" fn get_placeholder_cb(cb: *const PrjCallbackData) -> i32 {
        catch_hresult(|| {
            let Some(st) = (unsafe { ctx(cb) }) else { return PRJ_E_FAIL; };
            let cb_ref = unsafe { &*cb };
            match st.engine.placeholder_info(unsafe { decode_wide_z(cb_ref.file_path_name) }) {
                Ok(Some(info)) => {
                    let ph = PrjPlaceholderInfo::from_basic(info.to_basic_info());
                    let dst_w = encode_wide_z(OsStr::new(&info.windows_rel_path));
                    unsafe { (st.api.write_placeholder)(cb_ref.namespace_virtualization_context, dst_w.as_ptr(), &ph, std::mem::size_of::<PrjPlaceholderInfo>() as u32) }
                }
                Ok(None) => ERROR_FILE_NOT_FOUND_HR,
                Err(e) => hresult_from_io_error(&e),
            }
        })
    }
    unsafe extern "system" fn get_file_data_cb(cb: *const PrjCallbackData, byte_offset: u64, length: u32) -> i32 {
        catch_hresult(|| {
            if length == 0 { return PRJ_S_OK; }
            let Some(st) = (unsafe { ctx(cb) }) else { return PRJ_E_FAIL; };
            let cb_ref = unsafe { &*cb };
            let rel = unsafe { decode_wide_z(cb_ref.file_path_name) };
            let cap = (length as usize).min(64 * 1024);
            let raw = unsafe { (st.api.alloc_buf)(cb_ref.namespace_virtualization_context, cap) };
            if raw.is_null() { return PRJ_E_OUTOFMEMORY; }
            let buf = AlignedBuf(raw, st.api.free_buf);
            let (mut off, mut rem) = (byte_offset, length as usize);
            while rem > 0 {
                let bytes = match st.engine.read_hydration_data(&rel, off, rem.min(cap)) {
                    Ok(b) if b.is_empty() => break,
                    Ok(b) => b,
                    Err(e) => return hresult_from_io_error(&e),
                };
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.0.cast::<u8>(), bytes.len()); }
                let hr = unsafe { (st.api.write_file_data)(cb_ref.namespace_virtualization_context, &cb_ref.data_stream_id, buf.0, off, bytes.len() as u32) };
                if hr < 0 { return hr; }
                off += bytes.len() as u64;
                rem -= bytes.len();
            }
            PRJ_S_OK
        })
    }
    unsafe extern "system" fn notify_cb(cb: *const PrjCallbackData, _is_dir: u8, kind: u32, dest: *const u16, _params: *mut c_void) -> i32 {
        catch_hresult(|| {
            if let Some(st) = unsafe { ctx(cb) } {
                let rel = unsafe { decode_wide_z((*cb).file_path_name) };
                let dst = (!dest.is_null()).then(|| unsafe { decode_wide_z(dest) });
                let _ = dispatch_projfs_notification(&st.engine, kind, &rel, dst.as_deref());
            }
            PRJ_S_OK
        })
    }

    pub(super) fn mount(spec: &MountSpec) -> Result<MountedProjFs> {
        let cfg = OverlayMountConfig::from_spec(spec);
        cfg.ensure_directories()?;
        let api = Arc::new(ProjFsApi::load(&spec.workspace, &cfg.mount_point)?);
        let root_w = encode_wide_z(cfg.mount_point.as_os_str());
        let guid = PrjGuid::from_bytes(compute_instance_guid(&spec.git_dir, &spec.workspace));
        let mark_hr = unsafe { (api.mark_dir)(root_w.as_ptr(), std::ptr::null(), std::ptr::null(), &guid) };
        if mark_hr < 0 && mark_hr != ERROR_REPARSE_ENCOUNTERED_HR && mark_hr != ERROR_ALREADY_EXISTS_HR {
            return Err(OzError::MountUnavailable { workspace: spec.workspace.clone(), path: cfg.mount_point, errno: Some(mark_hr), message: format!("PrjMarkDirectoryAsPlaceholder failed (HRESULT 0x{:08x})", mark_hr as u32) });
        }
        let state_ptr = Box::into_raw(Box::new(WinState { engine: ProjFsLayerEngine::from_spec(spec), api: Arc::clone(&api), enums: Mutex::new(ProjFsEnumCache::default()) }));
        let empty_w: [u16; 1] = [0];
        let mut mapping = PrjNotificationMapping { notification_bit_mask: PRJ_NOTIFY_MASK, notification_root: empty_w.as_ptr() };
        let opts = PrjStartVirtualizingOptions { flags: 0, pool_thread_count: 0, concurrent_thread_count: 0, notification_mappings: &mut mapping, notification_mappings_count: 1 };
        let cbs = PrjCallbacks { start_directory_enumeration: Some(start_enum_cb), end_directory_enumeration: Some(end_enum_cb), get_directory_enumeration: Some(get_enum_cb), get_placeholder_info: Some(get_placeholder_cb), get_file_data: Some(get_file_data_cb), query_file_name: None, notification: Some(notify_cb), cancel_command: None };
        let mut ns_ctx: *mut c_void = std::ptr::null_mut();
        let hr = unsafe { (api.start_virt)(root_w.as_ptr(), &cbs, state_ptr.cast(), &opts, &mut ns_ctx) };
        if hr < 0 || ns_ctx.is_null() {
            unsafe { drop(Box::from_raw(state_ptr)); }
            return Err(OzError::MountUnavailable { workspace: spec.workspace.clone(), path: cfg.mount_point, errno: Some(hr), message: format!("PrjStartVirtualizing failed (HRESULT 0x{:08x})", hr as u32) });
        }
        Ok(MountedProjFs { config: cfg, ns_ctx, state_ptr, api })
    }

    pub(super) fn stop_and_free(m: &mut MountedProjFs) {
        if !m.ns_ctx.is_null() { unsafe { (m.api.stop_virt)(m.ns_ctx); } m.ns_ctx = std::ptr::null_mut(); }
        if !m.state_ptr.is_null() { unsafe { drop(Box::from_raw(m.state_ptr)); } m.state_ptr = std::ptr::null_mut(); }
    }
}

/// Active Windows ProjFS virtualization session handle.
#[cfg(target_os = "windows")]
pub struct MountedProjFs {
    pub config: OverlayMountConfig,
    ns_ctx: *mut core::ffi::c_void,
    state_ptr: *mut win32::WinState,
    api: std::sync::Arc<win32::ProjFsApi>,
}

#[cfg(target_os = "windows")]
unsafe impl Send for MountedProjFs {}
#[cfg(target_os = "windows")]
unsafe impl Sync for MountedProjFs {}

#[cfg(target_os = "windows")]
impl MountedProjFs {
    /// Stops ProjFS virtualization cleanly, waiting for in-flight callbacks to drain.
    pub fn unmount(mut self, _force: bool) -> Result<()> {
        win32::stop_and_free(&mut self);
        Ok(())
    }
}

#[cfg(target_os = "windows")]
impl Drop for MountedProjFs {
    fn drop(&mut self) {
        win32::stop_and_free(self);
    }
}

/// Starts a Windows ProjFS virtualization instance for `spec`.
#[cfg(target_os = "windows")]
pub fn mount_projfs(spec: &MountSpec) -> Result<MountedProjFs> {
    win32::mount(spec)
}

/// Non-Windows compile-time stub for [`MountedProjFs`].
#[cfg(not(target_os = "windows"))]
pub struct MountedProjFs {
    pub config: OverlayMountConfig,
}

#[cfg(not(target_os = "windows"))]
impl MountedProjFs {
    /// Non-Windows stub.
    pub fn unmount(self, _force: bool) -> Result<()> {
        Ok(())
    }
}

/// Non-Windows compile-time stub for [`mount_projfs`].
#[cfg(not(target_os = "windows"))]
pub fn mount_projfs(spec: &MountSpec) -> Result<MountedProjFs> {
    Err(OzError::MountUnavailable {
        workspace: spec.workspace.clone(),
        path: spec.mount_point.clone(),
        errno: None,
        message: "Windows ProjFS virtualization is only supported on Windows".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn validates_c_abi_struct_layouts_for_win64_projfs() {
        assert_eq!(size_of::<PrjGuid>(), 16);
        assert_eq!(align_of::<PrjGuid>(), 4);
        assert_eq!(size_of::<PrjFileBasicInfo>(), 56);
        assert_eq!(offset_of!(PrjFileBasicInfo, file_size), 8);
        assert_eq!(offset_of!(PrjFileBasicInfo, file_attributes), 48);
        assert_eq!(size_of::<PrjPlaceholderVersionInfo>(), 256);
        assert_eq!(size_of::<PrjPlaceholderInfo>(), 344);
        assert_eq!(offset_of!(PrjPlaceholderInfo, ea_buffer_size), 56);
        assert_eq!(offset_of!(PrjPlaceholderInfo, version_info), 80);
        assert_eq!(offset_of!(PrjPlaceholderInfo, variable_data), 336);
        assert_eq!(offset_of!(PrjCallbackData, flags), 4);
        assert_eq!(offset_of!(PrjCallbackData, command_id), 16);
        assert_eq!(offset_of!(PrjCallbackData, file_id), 20);
        assert_eq!(offset_of!(PrjCallbackData, data_stream_id), 36);
        assert_eq!(offset_of!(PrjCallbackData, file_path_name), 56);
        assert_eq!(offset_of!(PrjCallbackData, triggering_process_id), 72);
        assert_eq!(offset_of!(PrjCallbackData, instance_context), 88);
    }

    #[test]
    fn converts_unix_timestamp_to_windows_filetime_exactly() {
        let st = UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 500_000_000);
        assert_eq!(system_time_to_filetime(st), 133_444_736_005_000_000);
        assert_eq!(unix_to_filetime(0, 0), 116_444_736_000_000_000);
    }

    #[test]
    fn wildcard_matching_supports_windows_and_dos_wildcards() {
        assert!(prj_file_name_match("Cargo.toml", "*"));
        assert!(prj_file_name_match("Cargo.toml", "*.TOML"));
        assert!(prj_file_name_match("lib.rs", "???.rs"));
        assert!(!prj_file_name_match("main.rs", "???.rs"));

        // DOS_STAR (<) matches across non-final dots, never past last dot
        assert!(prj_file_name_match("a.b.txt", "<.txt"));
        assert!(prj_file_name_match("a.txt", "<.txt"));
        assert!(!prj_file_name_match("foo.txt", "foo<"));
        assert!(prj_file_name_match("foobar", "foo<"));

        // DOS_QM (>) matches one non-dot char or 0 chars before '.' / EOS
        assert!(prj_file_name_match("a.txt", "a>>.txt"));
        assert!(prj_file_name_match("ab.txt", "a>>.txt"));
        assert!(prj_file_name_match("abc.txt", "a>>.txt"));
        assert!(!prj_file_name_match("abcd.txt", "a>>.txt"));
        assert!(prj_file_name_match("a", "a>"));
        assert!(!prj_file_name_match("a.b", "a>b"));

        // DOS_DOT (") matches '.' or EOS
        assert!(prj_file_name_match("foo", "foo\"<"));
        assert!(prj_file_name_match("foo.", "foo\"<"));
        assert!(prj_file_name_match("foo.txt", "foo\"<"));
        assert!(!prj_file_name_match("foobar", "foo\"<"));
    }

    #[test]
    fn computes_deterministic_instance_guid() {
        let g1 = compute_instance_guid(Path::new("/repo/.git"), "ws-1");
        let g2 = compute_instance_guid(Path::new("/repo/.git/"), "ws-1");
        let g3 = compute_instance_guid(Path::new("/repo/.git"), "ws-2");
        assert_eq!(g1, g2);
        assert_ne!(g1, g3);
        assert_eq!(g1[6] >> 4, 5);
        assert_eq!(g1[8] >> 6, 2);
    }

    #[test]
    fn step_dir_enum_buffer_handles_insufficient_buffer_on_first_vs_later_entry() {
        let make_entry = |name: &str| ProjFsDirEntry {
            name: name.to_string(),
            is_dir: false,
            file_size: 4,
            creation_time: 0,
            last_access_time: 0,
            last_write_time: 0,
            change_time: 0,
            file_attributes: FILE_ATTRIBUTE_ARCHIVE,
            layer: LayerKind::Baseline,
            host_path: PathBuf::from(name),
        };
        let entries = vec![make_entry("a.txt"), make_entry("b.txt"), make_entry("c.txt")];
        let mut idx = 0usize;

        // First entry overflows buffer (`added == 0`) -> must return ERROR_INSUFFICIENT_BUFFER_HR
        // and leave `idx` unchanged so ProjFS retries with a larger buffer.
        let hr0 = step_dir_enum_buffer(&entries, &mut idx, false, |_| ERROR_INSUFFICIENT_BUFFER_HR);
        assert_eq!(hr0, ERROR_INSUFFICIENT_BUFFER_HR);
        assert_eq!(idx, 0);

        // Buffer fits 2 entries before returning ERROR_INSUFFICIENT_BUFFER_HR -> returns PRJ_S_OK
        // with `idx == 2` so the next call resumes at "c.txt".
        let mut calls = 0usize;
        let hr1 = step_dir_enum_buffer(&entries, &mut idx, false, |_| {
            calls += 1;
            if calls <= 2 {
                PRJ_S_OK
            } else {
                ERROR_INSUFFICIENT_BUFFER_HR
            }
        });
        assert_eq!(hr1, PRJ_S_OK);
        assert_eq!(idx, 2);

        // Remaining entry fits, then subsequent EOF call returns PRJ_S_OK cleanly.
        let hr2 = step_dir_enum_buffer(&entries, &mut idx, false, |_| PRJ_S_OK);
        assert_eq!(hr2, PRJ_S_OK);
        assert_eq!(idx, 3);
        let hr_eof = step_dir_enum_buffer(&entries, &mut idx, false, |_| panic!("unexpected fill"));
        assert_eq!(hr_eof, PRJ_S_OK);
    }
}
