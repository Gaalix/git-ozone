// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Background VFS daemon (`oz-vfsd`) IPC contract and executable lookup.
//!
//! On Linux (`ext4`), `oz-vfsd` drives an unprivileged pure-Rust `fuser` FUSE3 overlay
//! backed by `.git/ozone/ws/<ws>/upper/`. On Windows (`NTFS`), Milestone 2 adds `windows-projfs`.

pub mod lookup;
pub mod protocol;

pub use lookup::{OZ_VFSD_BIN_ENV, find_oz_vfsd_bin, resolve_oz_vfsd_bin_with, vfsd_binary_name};
pub use protocol::{DirtyPathEntry, MountInfo, MountSpec, VfsRequest, VfsResponse, encode_line};

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
        assert!(!cfg.mount_options.contains(&fuser::MountOption::AutoUnmount));
    }
}
