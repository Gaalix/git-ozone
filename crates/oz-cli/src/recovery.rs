// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Transparent daemon VFS (`SIGKILL`) recovery at CLI command entry.

use oz_core::{MountHealth, MountRecord, OzError, OzoneRepo, Result, probe_mount_health_os};
use std::path::{Path, PathBuf};

/// Discovers the repository and transparently recovers any dead `oz-vfsd` FUSE3 daemon
/// or `ENOTCONN` mount before executing a workspace command.
///
/// If the caller's process CWD was inside a dead FUSE3 mountpoint (causing `current_dir()` to fail
/// with `ENOTCONN`), re-anchors CWD onto the freshly remounted workspace path and re-discovers
/// [`OzoneRepo`] so `mount_health` reflects the healthy mount.
pub fn discover_and_recover_repo(
    directory: Option<&Path>,
    recover_all_ws: bool,
) -> Result<OzoneRepo> {
    let cwd_before = std::env::current_dir();
    let pwd_before = std::env::var_os("PWD").map(PathBuf::from);
    let mut repo = OzoneRepo::discover(directory)?;

    let self_needs_recovery = repo_mount_needs_recovery(&repo);
    let ws_needs_recovery = recover_all_ws && any_daemon_workspace_needs_recovery(&repo);

    if self_needs_recovery || ws_needs_recovery {
        oz_vfsd::ensure_daemon_and_mounts(&repo.layout.git_common_dir)?;

        if directory.is_none()
            && (cwd_before.is_err() || repo.mount_health == MountHealth::DisconnectedEnotconn)
        {
            let target = match pwd_before {
                Some(ref pwd) if pwd.starts_with(&repo.workdir_path) && pwd.is_dir() => {
                    pwd.as_path()
                }
                _ => repo.workdir_path.as_path(),
            };
            if target.is_dir() {
                std::env::set_current_dir(target)?;
            }
        }

        repo = OzoneRepo::discover(directory)?;
        if repo.mount_health == MountHealth::DisconnectedEnotconn {
            return Err(OzError::MountUnavailable {
                workspace: repo
                    .workspace
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                path: repo.workdir_path.clone(),
                errno: Some(107),
                message: "FUSE mount remained ENOTCONN after oz-vfsd auto-remount".to_string(),
            });
        }
    }

    Ok(repo)
}

fn repo_mount_needs_recovery(repo: &OzoneRepo) -> bool {
    match repo.mount_health {
        MountHealth::DisconnectedEnotconn => true,
        MountHealth::NotMounted => repo
            .mount_record
            .as_ref()
            .is_some_and(|mr| mr.mounted && mr.backend.requires_daemon()),
        MountHealth::Healthy => {
            repo.mount_record
                .as_ref()
                .is_some_and(|mr| mr.mounted && mr.backend.requires_daemon())
                && oz_vfsd::ping_daemon(&repo.layout.vfsd_socket_path()).is_err()
        }
        MountHealth::NotApplicable => false,
    }
}

fn any_daemon_workspace_needs_recovery(repo: &OzoneRepo) -> bool {
    let ws_dir = repo.layout.ws_dir();
    let Ok(entries) = std::fs::read_dir(&ws_dir) else {
        return false;
    };

    let mut has_expected_daemon_mount = false;
    for entry in entries.flatten() {
        let mount_json = entry.path().join("mount.json");
        let Ok(raw) = std::fs::read_to_string(&mount_json) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<MountRecord>(&raw) else {
            continue;
        };
        if record.mounted && record.backend.requires_daemon() {
            has_expected_daemon_mount = true;
            if probe_mount_health_os(&record.mount_point) != MountHealth::Healthy {
                return true;
            }
        }
    }

    has_expected_daemon_mount && oz_vfsd::ping_daemon(&repo.layout.vfsd_socket_path()).is_err()
}
