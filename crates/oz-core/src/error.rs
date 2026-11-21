// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Structured error types and exit code mapping for Ozone.

use crate::envelope::{ConflictEntry, ExitCode};
use std::path::PathBuf;
use thiserror::Error;

/// Core error type shared across Ozone crates.
#[derive(Debug, Error)]
pub enum OzError {
    /// Current path is not inside a Git or Ozone repository.
    #[error("not a git or ozone repository (searched from {path})")]
    NotARepository { path: PathBuf },

    /// Invalid `.ozone.toml` or `.git/ozone/config.toml` configuration.
    #[error("invalid ozone configuration in {path}: {message}")]
    InvalidConfig { path: PathBuf, message: String },

    /// Invalid CLI arguments or missing parameters in non-interactive mode.
    #[error("{message}")]
    InvalidUsage {
        message: String,
        recovery: Vec<String>,
    },

    /// Workspace path scope, secret, binary-size, or conflict-marker guardrail tripped.
    #[error("guardrail violation ({kind}): {message}")]
    GuardrailViolation {
        kind: String,
        message: String,
        recovery: Vec<String>,
    },

    /// Merge or integration conflict encountered.
    #[error("{message}")]
    Conflict {
        message: String,
        conflicts: Vec<ConflictEntry>,
        recovery: Vec<String>,
    },

    /// Verification gate or health check failed.
    #[error("{message}")]
    CheckFailed {
        message: String,
        recovery: Vec<String>,
    },

    /// A Git hook failed with a non-zero exit status.
    #[error("{message}")]
    HookFailed {
        hook: String,
        hook_exit_code: Option<i32>,
        message: String,
        recovery: Vec<String>,
    },

    /// VFS workspace mount is unavailable or disconnected (e.g., `ENOTCONN`).
    #[error("workspace '{workspace}' mount unavailable at {path}: {message}")]
    MountUnavailable {
        workspace: String,
        path: PathBuf,
        errno: Option<i32>,
        message: String,
    },

    /// Named Ozone workspace could not be found.
    #[error("workspace '{name}' not found")]
    WorkspaceNotFound { name: String },

    /// Required external or helper binary could not be located.
    #[error("executable '{name}' not found: {message}")]
    BinaryNotFound { name: String, message: String },

    /// Error reported by `gix` or Git metadata parsing.
    #[error("git error: {0}")]
    Git(String),

    /// Filesystem I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization failure.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// Generic backend or internal error.
    #[error("{0}")]
    Other(String),
}

impl OzError {
    /// Maps this error to the canonical Ozone CLI exit code taxonomy:
    /// - `1` for conflicts or check failures
    /// - `2` for usage, repository, config, guardrail, or runtime errors
    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Conflict { .. } | Self::CheckFailed { .. } | Self::HookFailed { .. } => {
                ExitCode::ConflictOrCheckFail
            }
            Self::NotARepository { .. }
            | Self::InvalidConfig { .. }
            | Self::InvalidUsage { .. }
            | Self::GuardrailViolation { .. }
            | Self::MountUnavailable { .. }
            | Self::WorkspaceNotFound { .. }
            | Self::BinaryNotFound { .. }
            | Self::Git(_)
            | Self::Io(_)
            | Self::Json(_)
            | Self::Other(_) => ExitCode::UsageOrGuardrail,
        }
    }

    /// Stable machine-readable error code for the `oz.v1` JSON envelope.
    #[must_use]
    pub const fn error_code(&self) -> &'static str {
        match self {
            Self::NotARepository { .. } => "not_a_repository",
            Self::InvalidConfig { .. } => "invalid_config",
            Self::InvalidUsage { .. } => "invalid_usage",
            Self::GuardrailViolation { .. } => "guardrail_violation",
            Self::Conflict { .. } => "conflict",
            Self::CheckFailed { .. } => "check_failed",
            Self::HookFailed { .. } => "hook_failed",
            Self::MountUnavailable { .. } => "mount_unavailable",
            Self::WorkspaceNotFound { .. } => "workspace_not_found",
            Self::BinaryNotFound { .. } => "binary_not_found",
            Self::Git(_) => "git_error",
            Self::Io(_) => "io_error",
            Self::Json(_) => "json_error",
            Self::Other(_) => "internal_error",
        }
    }

    /// Actionable recovery commands associated with this error, if any.
    #[must_use]
    pub fn recovery_commands(&self) -> Vec<String> {
        match self {
            Self::InvalidUsage { recovery, .. }
            | Self::GuardrailViolation { recovery, .. }
            | Self::Conflict { recovery, .. }
            | Self::CheckFailed { recovery, .. }
            | Self::HookFailed { recovery, .. } => recovery.clone(),
            Self::NotARepository { .. } => vec!["git init -b main".to_string()],
            Self::MountUnavailable { workspace, .. } => {
                vec![
                    "oz doctor --json".to_string(),
                    format!("oz ws list --json # workspace: {workspace}"),
                ]
            }
            Self::WorkspaceNotFound { .. } => vec!["oz ws list --json".to_string()],
            _ => Vec::new(),
        }
    }
}

/// Convenience result alias using [`OzError`].
pub type Result<T> = std::result::Result<T, OzError>;
