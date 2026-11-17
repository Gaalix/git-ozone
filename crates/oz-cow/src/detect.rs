// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! OS and filesystem capability detection (`statfs` on Linux/macOS, Win32 volume query on Windows).

use oz_core::{BackendKind, FilesystemKind, OzError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Linux `/dev/fuse` and `fusermount3` readiness diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinuxFuseReadiness {
    pub dev_fuse_exists: bool,
    pub dev_fuse_rw: bool,
    pub fusermount3_path: Option<PathBuf>,
    pub fusermount3_version: Option<String>,
    pub user_allow_other_enabled: bool,
    pub ready: bool,
}

/// Detected OS, filesystem, and recommended Ozone workspace backend for a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemProbe {
    pub os: String,
    pub arch: String,
    pub probed_path: PathBuf,
    pub fs_kind: FilesystemKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs_magic: Option<String>,
    pub reflink_supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fuse: Option<LinuxFuseReadiness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projfs_ready: Option<bool>,
    pub recommended_backend: BackendKind,
}

/// Selects the optimal [`BackendKind`] given probe results and an optional config override.
#[must_use]
pub fn select_backend(probe: &FilesystemProbe, configured: Option<BackendKind>) -> BackendKind {
    if let Some(override_backend) = configured {
        return override_backend;
    }
    probe.recommended_backend
}

/// Probes OS, host filesystem type, reflink support, and VFS daemon readiness for `path`.
pub fn detect_filesystem(path: &Path) -> Result<FilesystemProbe> {
    let existing = nearest_existing_ancestor(path);
    let existing_ozone = find_existing_ozone_dir(&existing);
    detect_filesystem_with_ozone(path, existing_ozone.as_deref())
}

/// Probes OS, host filesystem type, empirical `.git/ozone/.probe` reflink support, and VFS daemon
/// readiness for `path`.
pub fn detect_filesystem_with_ozone(
    path: &Path,
    ozone_dir: Option<&Path>,
) -> Result<FilesystemProbe> {
    let existing = nearest_existing_ancestor(path);
    let (fs_kind, fs_magic) = probe_fs_kind(&existing)?;
    let reflink_supported = if let Some(oz) = ozone_dir {
        probe_ozone_reflink(oz)
    } else if fs_kind.supports_reflinks() {
        probe_reflink_ephemeral(&existing).unwrap_or_else(|| fs_kind.supports_reflinks())
    } else {
        false
    };

    let fuse = detect_linux_fuse();
    let projfs_ready = detect_windows_projfs();
    let recommended_backend = recommend_backend(&fs_kind, reflink_supported, fuse.as_ref(), projfs_ready);

    Ok(FilesystemProbe {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        probed_path: existing,
        fs_kind,
        fs_magic,
        reflink_supported,
        fuse,
        projfs_ready,
        recommended_backend,
    })
}

/// Empirically probes native kernel reflink support inside `<ozone_dir>/.probe`.
#[must_use]
pub fn probe_ozone_reflink(ozone_dir: &Path) -> bool {
    probe_reflink_in_dir(&ozone_dir.join(".probe"))
}

/// Empirically tests `reflink_copy::reflink` using temporary probe files inside `probe_dir`.
#[must_use]
pub fn probe_reflink_in_dir(probe_dir: &Path) -> bool {
    if std::fs::create_dir_all(probe_dir).is_err() {
        return false;
    }
    probe_reflink_ephemeral(probe_dir).unwrap_or(false)
}

fn probe_reflink_ephemeral(dir: &Path) -> Option<bool> {
    if !dir.is_dir() {
        return None;
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let src = dir.join(format!(".oz_probe_src_{pid}_{seq}"));
    let dst = dir.join(format!(".oz_probe_dst_{pid}_{seq}"));

    if std::fs::write(&src, b"ozone-reflink-probe").is_err() {
        let _ = std::fs::remove_file(&src);
        return None;
    }
    let _ = std::fs::remove_file(&dst);
    let res = reflink_copy::reflink(&src, &dst).is_ok();
    let _ = std::fs::remove_file(&dst);
    let _ = std::fs::remove_file(&src);
    Some(res)
}

fn find_existing_ozone_dir(path: &Path) -> Option<PathBuf> {
    for anc in path.ancestors() {
        let cand = anc.join(".git/ozone");
        if cand.is_dir() {
            return Some(cand);
        }
        if anc.file_name().is_some_and(|n| n == "ozone")
            && anc.parent().and_then(|p| p.file_name()).is_some_and(|n| n == ".git")
            && anc.is_dir()
        {
            return Some(anc.to_path_buf());
        }
    }
    None
}

fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    for anc in path.ancestors() {
        if anc.exists() {
            return anc.to_path_buf();
        }
    }
    PathBuf::from(".")
}

fn recommend_backend(
    fs_kind: &FilesystemKind,
    reflink_supported: bool,
    fuse: Option<&LinuxFuseReadiness>,
    projfs_ready: Option<bool>,
) -> BackendKind {
    match std::env::consts::OS {
        "macos" => {
            if reflink_supported && matches!(fs_kind, FilesystemKind::Apfs) {
                BackendKind::ApfsClonefile
            } else {
                BackendKind::CasHardlink
            }
        }
        "linux" => {
            if reflink_supported
                && matches!(
                    fs_kind,
                    FilesystemKind::Btrfs | FilesystemKind::Xfs | FilesystemKind::Bcachefs
                )
            {
                BackendKind::LinuxFiclone
            } else if fuse.is_some_and(|f| f.ready) {
                BackendKind::LinuxFuse3
            } else {
                BackendKind::CasHardlink
            }
        }
        "windows" => {
            if reflink_supported && matches!(fs_kind, FilesystemKind::Refs) {
                BackendKind::WindowsRefsClone
            } else if matches!(fs_kind, FilesystemKind::Ntfs) && projfs_ready == Some(true) {
                BackendKind::WindowsProjFs
            } else {
                BackendKind::CasHardlink
            }
        }
        _ => BackendKind::CasHardlink,
    }
}

#[cfg(target_os = "linux")]
fn probe_fs_kind(path: &Path) -> Result<(FilesystemKind, Option<String>)> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;

    const EXT4_SUPER_MAGIC: u64 = 0xEF53;
    const BTRFS_SUPER_MAGIC: u64 = 0x9123_683E;
    const XFS_SUPER_MAGIC: u64 = 0x5846_5342;
    const BCACHEFS_STATFS_MAGIC: u64 = 0xCA45_1A4E;
    const TMPFS_MAGIC: u64 = 0x0102_1994;
    const OVERLAYFS_SUPER_MAGIC: u64 = 0x794C_7630;
    const FUSE_SUPER_MAGIC: u64 = 0x6573_5546;
    const NFS_SUPER_MAGIC: u64 = 0x6969;
    const ZFS_SUPER_MAGIC: u64 = 0x2FC1_2FC1;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| OzError::Other(format!("invalid path for statfs: {e}")))?;

    let mut buf = MaybeUninit::<libc::statfs>::uninit();
    let rc = unsafe { libc::statfs(c_path.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return Err(OzError::Io(std::io::Error::last_os_error()));
    }
    let stat = unsafe { buf.assume_init() };
    let magic = stat.f_type as u64;
    let kind = match magic {
        EXT4_SUPER_MAGIC => FilesystemKind::Ext4,
        BTRFS_SUPER_MAGIC => FilesystemKind::Btrfs,
        XFS_SUPER_MAGIC => FilesystemKind::Xfs,
        BCACHEFS_STATFS_MAGIC => FilesystemKind::Bcachefs,
        TMPFS_MAGIC => FilesystemKind::Tmpfs,
        OVERLAYFS_SUPER_MAGIC => FilesystemKind::Overlayfs,
        FUSE_SUPER_MAGIC => FilesystemKind::Fuse,
        NFS_SUPER_MAGIC => FilesystemKind::Nfs,
        ZFS_SUPER_MAGIC => FilesystemKind::Zfs,
        other => FilesystemKind::Unknown(format!("0x{other:x}")),
    };
    Ok((kind, Some(format!("0x{magic:x}"))))
}

#[cfg(target_os = "macos")]
fn probe_fs_kind(path: &Path) -> Result<(FilesystemKind, Option<String>)> {
    use std::ffi::{CStr, CString};
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| OzError::Other(format!("invalid path for statfs: {e}")))?;

    let mut buf = MaybeUninit::<libc::statfs>::uninit();
    let rc = unsafe { libc::statfs(c_path.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return Err(OzError::Io(std::io::Error::last_os_error()));
    }
    let stat = unsafe { buf.assume_init() };
    let fs_name = unsafe { CStr::from_ptr(stat.f_fstypename.as_ptr()) }
        .to_string_lossy()
        .to_ascii_lowercase();

    let kind = match fs_name.as_str() {
        "apfs" => FilesystemKind::Apfs,
        "hfs" => FilesystemKind::HfsPlus,
        "tmpfs" => FilesystemKind::Tmpfs,
        "nfs" => FilesystemKind::Nfs,
        other => FilesystemKind::Unknown(other.to_string()),
    };
    Ok((kind, None))
}

#[cfg(target_os = "windows")]
fn probe_fs_kind(path: &Path) -> Result<(FilesystemKind, Option<String>)> {
    use std::os::windows::ffi::OsStrExt;

    unsafe extern "system" {
        fn GetVolumePathNameW(
            lpsz_file_name: *const u16,
            lpsz_volume_path_name: *mut u16,
            cch_buffer_length: u32,
        ) -> i32;
        fn GetVolumeInformationW(
            lp_root_path_name: *const u16,
            lp_volume_name_buffer: *mut u16,
            n_volume_name_size: u32,
            lp_volume_serial_number: *mut u32,
            lp_maximum_component_length: *mut u32,
            lp_file_system_flags: *mut u32,
            lp_file_system_name_buffer: *mut u16,
            n_file_system_name_size: u32,
        ) -> i32;
    }

    let wide_path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut vol_buf = vec![0u16; 260];
    let ok = unsafe {
        GetVolumePathNameW(
            wide_path.as_ptr(),
            vol_buf.as_mut_ptr(),
            vol_buf.len() as u32,
        )
    };
    if ok == 0 {
        return Err(OzError::Io(std::io::Error::last_os_error()));
    }

    let mut fs_buf = vec![0u16; 64];
    let info_ok = unsafe {
        GetVolumeInformationW(
            vol_buf.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs_buf.as_mut_ptr(),
            fs_buf.len() as u32,
        )
    };
    if info_ok == 0 {
        return Err(OzError::Io(std::io::Error::last_os_error()));
    }
    let end = fs_buf.iter().position(|&c| c == 0).unwrap_or(fs_buf.len());
    let fs_name = String::from_utf16_lossy(&fs_buf[..end]).to_ascii_lowercase();
    let kind = match fs_name.as_str() {
        "ntfs" => FilesystemKind::Ntfs,
        "refs" => FilesystemKind::Refs,
        other => FilesystemKind::Unknown(other.to_string()),
    };
    Ok((kind, None))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn probe_fs_kind(_path: &Path) -> Result<(FilesystemKind, Option<String>)> {
    Ok((
        FilesystemKind::Unknown(std::env::consts::OS.to_string()),
        None,
    ))
}

#[cfg(target_os = "linux")]
fn detect_linux_fuse() -> Option<LinuxFuseReadiness> {
    let dev_fuse = Path::new("/dev/fuse");
    let dev_fuse_exists = dev_fuse.exists();
    let dev_fuse_rw = if dev_fuse_exists {
        unsafe { libc::access(c"/dev/fuse".as_ptr(), libc::R_OK | libc::W_OK) == 0 }
    } else {
        false
    };

    let fusermount3_path = find_fusermount3();
    let fusermount3_version = fusermount3_path.as_deref().and_then(|bin| {
        std::process::Command::new(bin)
            .arg("--version")
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    });

    let user_allow_other_enabled = std::fs::read_to_string("/etc/fuse.conf")
        .map(|content| {
            content
                .lines()
                .any(|line| line.trim() == "user_allow_other")
        })
        .unwrap_or(false);

    let ready = dev_fuse_rw && fusermount3_path.is_some();

    Some(LinuxFuseReadiness {
        dev_fuse_exists,
        dev_fuse_rw,
        fusermount3_path,
        fusermount3_version,
        user_allow_other_enabled,
        ready,
    })
}

#[cfg(not(target_os = "linux"))]
fn detect_linux_fuse() -> Option<LinuxFuseReadiness> {
    None
}

#[cfg(target_os = "linux")]
fn find_fusermount3() -> Option<PathBuf> {
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join("fusermount3");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    let standard = PathBuf::from("/usr/bin/fusermount3");
    standard.is_file().then_some(standard)
}

#[cfg(target_os = "windows")]
fn detect_windows_projfs() -> Option<bool> {
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let dll = PathBuf::from(system_root).join("System32\\ProjectedFSLib.dll");
    Some(dll.is_file())
}

#[cfg(not(target_os = "windows"))]
fn detect_windows_projfs() -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_current_filesystem_successfully() {
        let probe = detect_filesystem(Path::new(".")).expect("detect filesystem");
        assert_eq!(probe.os, std::env::consts::OS);
        assert!(!probe.fs_kind.as_str().is_empty());
        #[cfg(target_os = "linux")]
        {
            assert!(probe.fuse.is_some());
        }
    }

    #[test]
    fn selects_expected_backend_per_platform_and_override() {
        let linux_ext4 = FilesystemProbe {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            probed_path: PathBuf::from("/tmp"),
            fs_kind: FilesystemKind::Ext4,
            fs_magic: Some("0xef53".to_string()),
            reflink_supported: false,
            fuse: Some(LinuxFuseReadiness {
                dev_fuse_exists: true,
                dev_fuse_rw: true,
                fusermount3_path: Some(PathBuf::from("/usr/bin/fusermount3")),
                fusermount3_version: Some("fusermount3 version: 3.18.3".to_string()),
                user_allow_other_enabled: false,
                ready: true,
            }),
            projfs_ready: None,
            recommended_backend: BackendKind::LinuxFuse3,
        };
        assert_eq!(select_backend(&linux_ext4, None), BackendKind::LinuxFuse3);
        assert_eq!(
            select_backend(&linux_ext4, Some(BackendKind::CasHardlink)),
            BackendKind::CasHardlink
        );
    }
}
