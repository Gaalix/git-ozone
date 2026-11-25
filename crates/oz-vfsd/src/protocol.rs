// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! JSON IPC protocol types exchanged between `oz` and `oz-vfsd`.

use oz_core::ChangeKind;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Mount configuration sent to `oz-vfsd` to materialize a VFS workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSpec {
    pub workspace: String,
    pub branch: String,
    pub repo_root: PathBuf,
    pub git_dir: PathBuf,
    pub mount_point: PathBuf,
    pub upper_dir: PathBuf,
    pub meta_dir: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_tree: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_dir: Option<PathBuf>,
    /// Defaults to `false` so unprivileged Linux hosts without `user_allow_other` work out of the box.
    #[serde(default)]
    pub allow_other: bool,
    /// Enables kernel writeback caching on Linux FUSE3 where supported.
    #[serde(default = "default_true")]
    pub writeback_cache: bool,
}

const fn default_true() -> bool {
    true
}

impl MountSpec {
    /// Resolves the optional middle `seed/` layer directory (`<meta_dir>/../seed`).
    #[must_use]
    pub fn resolved_seed_dir(&self) -> Option<PathBuf> {
        self.meta_dir
            .parent()
            .map(|p| p.join("seed"))
            .filter(|d| d.is_dir())
    }
}

/// Request message sent over `.git/ozone/vfsd.sock` (or Windows named pipe).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum VfsRequest {
    Ping,
    Status,
    Mount {
        spec: Box<MountSpec>,
    },
    Unmount {
        workspace: String,
        #[serde(default)]
        force: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git_dir: Option<PathBuf>,
    },
    DirtyPaths {
        workspace: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git_dir: Option<PathBuf>,
    },
    ListMounts,
    Shutdown,
}

/// Active mount summary returned by `oz-vfsd`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountInfo {
    pub workspace: String,
    pub branch: String,
    pub mount_point: PathBuf,
    pub upper_dir: PathBuf,
    pub mounted: bool,
    pub dirty_count: usize,
}

/// Dirty file path tracked in memory / upperdir by `oz-vfsd`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyPathEntry {
    pub path: String,
    pub kind: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

/// Response message returned by `oz-vfsd`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum VfsResponse {
    Pong {
        version: String,
        pid: u32,
    },
    DaemonStatus {
        version: String,
        pid: u32,
        uptime_secs: u64,
        active_mounts: Vec<MountInfo>,
    },
    Mounted {
        mount: MountInfo,
    },
    Unmounted {
        workspace: String,
    },
    DirtySet {
        workspace: String,
        paths: Vec<DirtyPathEntry>,
    },
    MountList {
        mounts: Vec<MountInfo>,
    },
    ShuttingDown,
    Error {
        code: String,
        message: String,
    },
}

/// Encodes any IPC request/response as a single newline-terminated JSON line.
pub fn encode_line<T: Serialize>(msg: &T) -> serde_json::Result<String> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_ipc_requests_and_responses() {
        let req = VfsRequest::Mount {
            spec: Box::new(MountSpec {
                workspace: "agent-auth".to_string(),
                branch: "feat/pkce".to_string(),
                repo_root: PathBuf::from("/repo"),
                git_dir: PathBuf::from("/repo/.git"),
                mount_point: PathBuf::from("/repo.oz/agent-auth"),
                upper_dir: PathBuf::from("/repo/.git/ozone/ws/agent-auth/upper"),
                meta_dir: PathBuf::from("/repo/.git/ozone/ws/agent-auth/meta"),
                base_tree: Some("0123456789abcdef".to_string()),
                base_commit: None,
                baseline_dir: None,
                allow_other: false,
                writeback_cache: true,
            }),
        };
        let encoded = encode_line(&req).expect("encode req");
        assert!(encoded.ends_with('\n'));
        let decoded: VfsRequest = serde_json::from_str(encoded.trim()).expect("decode req");
        assert_eq!(decoded, req);

        let resp = VfsResponse::DirtySet {
            workspace: "agent-auth".to_string(),
            paths: vec![DirtyPathEntry {
                path: "crates/auth/src/lib.rs".to_string(),
                kind: ChangeKind::Modified,
                size_bytes: Some(1024),
            }],
        };
        let encoded_resp = encode_line(&resp).expect("encode resp");
        let decoded_resp: VfsResponse =
            serde_json::from_str(encoded_resp.trim()).expect("decode resp");
        assert_eq!(decoded_resp, resp);
    }
}
