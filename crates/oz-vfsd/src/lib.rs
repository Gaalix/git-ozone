// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Background VFS daemon (`oz-vfsd`), Linux FUSE3 CoW overlay engine, Windows ProjFS engine,
//! IPC client/server, executable lookup, and OS setup helpers.
//!
//! On Linux (`ext4`), `oz-vfsd` drives an unprivileged pure-Rust `fuser` FUSE3 overlay
//! backed by `.git/ozone/ws/<ws>/upper/` with tombstones and opacity markers in
//! `.git/ozone/ws/<ws>/meta/`. On Windows (`NTFS`), `oz-vfsd` drives Windows ProjFS via
//! dynamic `ProjectedFSLib.dll` FFI backed by the exact same `upper/` + `meta/` layout.

pub mod client;
pub mod lookup;
pub mod overlay;
pub mod projfs;
pub mod protocol;
pub mod server;
pub mod systemd;
pub mod windows_setup;

pub use client::{
    DaemonPong, dirty_paths_via_daemon, ensure_daemon_and_mounts, mount_via_daemon,
    mount_via_daemon_with_auto_start, ping_daemon, send_request, send_tcp_request, shutdown_daemon,
    socket_path_for_git_dir, unmount_via_daemon,
};
pub use lookup::{OZ_VFSD_BIN_ENV, find_oz_vfsd_bin, resolve_oz_vfsd_bin_with, vfsd_binary_name};
pub use overlay::{
    LayerKind, MergedDirEntry, MountedOverlay, OPAQUE_SUFFIX, OverlayMountConfig, ResolvedEntry,
    WHITEOUT_SUFFIX, lazy_unmount_fuse, list_merged_dir_entries, mount_overlay, resolve_rel_path,
    resolve_seed_dir, scan_dirty_paths,
};
pub use projfs::{
    MountedProjFs, ProjFsDirEntry, ProjFsLayerEngine, ProjFsPlaceholderInfo, compute_instance_guid,
    mount_projfs, normalize_rel_path, prj_file_name_compare, prj_file_name_match,
    system_time_to_filetime, unix_to_filetime,
};
pub use protocol::{DirtyPathEntry, MountInfo, MountSpec, VfsRequest, VfsResponse, encode_line};
pub use server::{VfsDaemon, run_daemon, run_tcp_daemon};
pub use systemd::{
    generate_systemd_user_unit, install_systemd_user_unit, install_systemd_user_unit_in,
    systemd_user_unit_dir, systemd_user_unit_name,
};
pub use windows_setup::{
    PROJFS_ENABLE_POWERSHELL_CMD, enable_windows_projfs, enable_windows_projfs_with,
    generate_windows_startup_cmd, is_windows_projfs_enabled, is_windows_projfs_enabled_in,
    projected_fs_dll_path, projected_fs_dll_path_in,
};

/// Builds a pure-Rust `fuser::Config` for Linux FUSE3 mounts without requiring `user_allow_other`
/// in `/etc/fuse.conf` when `spec.allow_other` is `false`.
#[cfg(target_os = "linux")]
#[must_use]
pub fn default_linux_fuser_config(spec: &MountSpec) -> fuser::Config {
    let mut cfg = fuser::Config::default();
    cfg.mount_options = vec![
        fuser::MountOption::FSName(format!("ozone:{}", spec.workspace)),
        fuser::MountOption::Subtype("ozone".to_string()),
        fuser::MountOption::RW,
        fuser::MountOption::DefaultPermissions,
        fuser::MountOption::NoAtime,
    ];
    cfg.acl = if spec.allow_other {
        fuser::SessionACL::All
    } else {
        fuser::SessionACL::Owner
    };
    cfg
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn builds_pure_rust_fuser_config_without_allow_other_by_default() {
        let spec = MountSpec {
            workspace: "agent-auth".to_string(),
            branch: "feat/pkce".to_string(),
            repo_root: PathBuf::from("/repo"),
            git_dir: PathBuf::from("/repo/.git"),
            mount_point: PathBuf::from("/repo.oz/agent-auth"),
            upper_dir: PathBuf::from("/repo/.git/ozone/ws/agent-auth/upper"),
            meta_dir: PathBuf::from("/repo/.git/ozone/ws/agent-auth/meta"),
            base_tree: None,
            base_commit: None,
            baseline_dir: None,
            allow_other: false,
            writeback_cache: true,
        };
        let cfg = default_linux_fuser_config(&spec);
        assert_eq!(cfg.acl, fuser::SessionACL::Owner);
        assert!(cfg.mount_options.contains(&fuser::MountOption::NoAtime));
        assert!(!cfg.mount_options.contains(&fuser::MountOption::AutoUnmount));
    }
}
