// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! JSON IPC protocol types exchanged between `oz` and `oz-vfsd`.

use oz_core::ChangeKind;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

/// Maximum size in bytes of a single newline-delimited JSON IPC request frame (1 MiB).
pub(crate) const MAX_IPC_LINE_BYTES: usize = 1024 * 1024;

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

/// Wire-level IPC request envelope carrying an optional per-instance authentication token
/// alongside the flattened [`VfsRequest`] payload.
///
/// Unix domain socket connections authenticate peers via kernel `SO_PEERCRED` and may omit
/// `auth_token` (remaining wire-compatible with bare [`VfsRequest`] JSON objects).
/// Loopback TCP connections require `auth_token` on every request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthenticatedRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    #[serde(flatten)]
    pub request: VfsRequest,
}

impl AuthenticatedRequest {
    /// Wraps `request` without an auth token (for peer-credentialed Unix sockets).
    #[must_use]
    pub const fn unauthenticated(request: VfsRequest) -> Self {
        Self {
            auth_token: None,
            request,
        }
    }

    /// Wraps `request` with a per-instance loopback TCP authentication token.
    #[must_use]
    pub const fn with_token(request: VfsRequest, auth_token: String) -> Self {
        Self {
            auth_token: Some(auth_token),
            request,
        }
    }
}

/// Parsed loopback TCP IPC endpoint metadata stored in a daemon endpoint file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpEndpointInfo {
    pub addr: SocketAddr,
    pub auth_token: String,
}

/// Formats loopback TCP endpoint metadata (`tcp://127.0.0.1:<port>\ntoken=<hex>\n`).
#[must_use]
pub fn format_tcp_endpoint(port: u16, auth_token: &str) -> String {
    format!("tcp://127.0.0.1:{port}\ntoken={auth_token}\n")
}

/// Parses loopback TCP endpoint metadata written by `run_tcp_daemon`.
///
/// Enforces that line 1 is a loopback `tcp://<ip>:<port>` address and that a non-empty
/// `token=<value>` line is present (fail-closed if absent).
pub fn parse_tcp_endpoint(raw: &str) -> std::result::Result<TcpEndpointInfo, String> {
    let mut lines = raw.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().ok_or_else(|| {
        "empty oz-vfsd TCP endpoint file (expected 'tcp://127.0.0.1:<port>')".to_string()
    })?;
    let addr_raw = first.strip_prefix("tcp://").ok_or_else(|| {
        "invalid oz-vfsd TCP endpoint (expected 'tcp://127.0.0.1:<port>')".to_string()
    })?;
    let addr: SocketAddr = addr_raw
        .parse()
        .map_err(|err| format!("invalid oz-vfsd TCP socket address '{addr_raw}': {err}"))?;
    if !addr.ip().is_loopback() {
        return Err(format!("refusing non-loopback oz-vfsd IPC address {addr}"));
    }

    let mut auth_token: Option<String> = None;
    for line in lines {
        if let Some(tok) = line.strip_prefix("token=") {
            let tok = tok.trim();
            if !tok.is_empty() {
                auth_token = Some(tok.to_string());
            }
        }
    }
    let auth_token = auth_token
        .ok_or_else(|| "missing authentication token in oz-vfsd TCP endpoint file".to_string())?;

    Ok(TcpEndpointInfo { addr, auth_token })
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

        // Bare VfsRequest JSON also decodes cleanly as an unauthenticated AuthenticatedRequest.
        let bare_env: AuthenticatedRequest =
            serde_json::from_str(encoded.trim()).expect("decode bare req into envelope");
        assert_eq!(bare_env.auth_token, None);
        assert_eq!(bare_env.request, req);

        // AuthenticatedRequest with token round-trips across all request variants.
        let token = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90".to_string();
        for variant in [
            VfsRequest::Ping,
            VfsRequest::Status,
            req.clone(),
            VfsRequest::Unmount {
                workspace: "agent-auth".to_string(),
                force: true,
                git_dir: Some(PathBuf::from("/repo/.git")),
            },
            VfsRequest::DirtyPaths {
                workspace: "agent-auth".to_string(),
                git_dir: None,
            },
            VfsRequest::ListMounts,
            VfsRequest::Shutdown,
        ] {
            let env = AuthenticatedRequest::with_token(variant.clone(), token.clone());
            let line = encode_line(&env).expect("encode auth envelope");
            assert!(line.contains("\"auth_token\""));
            let decoded_env: AuthenticatedRequest =
                serde_json::from_str(line.trim()).expect("decode auth envelope");
            assert_eq!(decoded_env.auth_token.as_deref(), Some(token.as_str()));
            assert_eq!(decoded_env.request, variant);

            let unauth = AuthenticatedRequest::unauthenticated(variant.clone());
            let unauth_line = encode_line(&unauth).expect("encode unauth envelope");
            assert!(!unauth_line.contains("auth_token"));
            let decoded_unauth: AuthenticatedRequest =
                serde_json::from_str(unauth_line.trim()).expect("decode unauth envelope");
            assert_eq!(decoded_unauth.auth_token, None);
            assert_eq!(decoded_unauth.request, variant);
        }

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

    #[test]
    fn tcp_endpoint_metadata_formats_and_validates_fail_closed() {
        let tok = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let formatted = format_tcp_endpoint(45123, tok);
        assert!(formatted.starts_with("tcp://127.0.0.1:45123\n"));
        assert!(formatted.ends_with('\n'));
        let parsed = parse_tcp_endpoint(&formatted).expect("parse valid endpoint");
        assert_eq!(parsed.addr.port(), 45123);
        assert!(parsed.addr.ip().is_loopback());
        assert_eq!(parsed.auth_token, tok);

        // Missing token must fail closed.
        assert!(parse_tcp_endpoint("tcp://127.0.0.1:45123\n").is_err());
        assert!(parse_tcp_endpoint("tcp://127.0.0.1:45123\ntoken=\n").is_err());
        // Non-loopback address must be rejected.
        assert!(parse_tcp_endpoint(&format!("tcp://0.0.0.0:45123\ntoken={tok}\n")).is_err());
        assert!(parse_tcp_endpoint(&format!("tcp://192.168.1.5:45123\ntoken={tok}\n")).is_err());
        // Invalid scheme / empty content must be rejected.
        assert!(parse_tcp_endpoint("").is_err());
        assert!(parse_tcp_endpoint("http://127.0.0.1:45123\ntoken=abc\n").is_err());
    }
}
