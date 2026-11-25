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

    /// Structured Git / `gix` error that preserves the underlying causal error chain.
    #[error("{}", format_git_source(context.as_deref(), source.as_ref()))]
    GitSource {
        context: Option<String>,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Filesystem I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization failure.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// Generic backend or internal error.
    #[error("{0}")]
    Other(String),

    /// Structured backend or internal error that preserves the underlying causal error chain.
    #[error("{}", format_other_source(context.as_deref(), source.as_ref()))]
    OtherSource {
        context: Option<String>,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

fn format_git_source(
    context: Option<&str>,
    source: &(dyn std::error::Error + Send + Sync),
) -> String {
    match context {
        Some(ctx) if !ctx.is_empty() => format!("git error: {ctx}: {source}"),
        _ => format!("git error: {source}"),
    }
}

fn format_other_source(
    context: Option<&str>,
    source: &(dyn std::error::Error + Send + Sync),
) -> String {
    match context {
        Some(ctx) if !ctx.is_empty() => format!("{ctx}: {source}"),
        _ => source.to_string(),
    }
}

impl OzError {
    /// Wraps a typed Git / `gix` error while preserving its [`std::error::Error::source`] chain.
    #[must_use]
    pub fn git_source<E>(source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::GitSource {
            context: None,
            source: Box::new(source),
        }
    }

    /// Wraps a typed Git / `gix` error with context while preserving its [`std::error::Error::source`] chain.
    #[must_use]
    pub fn git_context<E>(context: impl Into<String>, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::GitSource {
            context: Some(context.into()),
            source: Box::new(source),
        }
    }

    /// Wraps a backend or internal error while preserving its [`std::error::Error::source`] chain.
    #[must_use]
    pub fn other_source<E>(source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::OtherSource {
            context: None,
            source: Box::new(source),
        }
    }

    /// Wraps a backend or internal error with context while preserving its [`std::error::Error::source`] chain.
    #[must_use]
    pub fn other_context<E>(context: impl Into<String>, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::OtherSource {
            context: Some(context.into()),
            source: Box::new(source),
        }
    }

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
            | Self::GitSource { .. }
            | Self::Io(_)
            | Self::Json(_)
            | Self::Other(_)
            | Self::OtherSource { .. } => ExitCode::UsageOrGuardrail,
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
            Self::CheckFailed { .. } | Self::HookFailed { .. } => "check_failed",
            Self::MountUnavailable { .. } => "mount_unavailable",
            Self::WorkspaceNotFound { .. } => "workspace_not_found",
            Self::BinaryNotFound { .. } => "binary_not_found",
            Self::Git(_) | Self::GitSource { .. } => "git_error",
            Self::Io(_) => "io_error",
            Self::Json(_) => "json_error",
            Self::Other(_) | Self::OtherSource { .. } => "internal_error",
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
                    format!("oz ws spawn {workspace} --backend cas_hardlink"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Envelope;
    use std::error::Error as _;

    #[test]
    fn structured_git_and_other_sources_preserve_causal_chains_and_codes() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked ref");
        let git_err = OzError::git_source(io_err);
        assert_eq!(git_err.error_code(), "git_error");
        assert_eq!(git_err.exit_code(), ExitCode::UsageOrGuardrail);
        assert_eq!(git_err.to_string(), "git error: locked ref");
        let src = git_err
            .source()
            .expect("git_source preserves Error::source");
        let downcast = src
            .downcast_ref::<std::io::Error>()
            .expect("source downcasts to io::Error");
        assert_eq!(downcast.kind(), std::io::ErrorKind::PermissionDenied);

        let ctx_git = OzError::git_context(
            "opening repo",
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing HEAD"),
        );
        assert_eq!(ctx_git.error_code(), "git_error");
        assert_eq!(ctx_git.to_string(), "git error: opening repo: missing HEAD");
        assert!(ctx_git.source().is_some());

        let env = Envelope::from_error(&ctx_git);
        let env_err = env.error.expect("envelope error");
        assert_eq!(env_err.code, "git_error");
        assert_eq!(env_err.message, "git error: opening repo: missing HEAD");

        let other_err = OzError::other_context(
            "directory walk failed",
            std::io::Error::new(std::io::ErrorKind::Interrupted, "walk aborted"),
        );
        assert_eq!(other_err.error_code(), "internal_error");
        assert_eq!(other_err.exit_code(), ExitCode::UsageOrGuardrail);
        assert_eq!(other_err.to_string(), "directory walk failed: walk aborted");
        assert!(other_err.source().is_some());
    }
}
