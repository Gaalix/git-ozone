// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Core library for Ozone (`oz` / `git-ozone`).
//!
//! Provides colocated `.git` + `.git/ozone/` repository discovery, `.ozone.toml`
//! configuration parsing, canonical `oz.v1` JSON envelope formatting, non-interactive
//! detection, exit codes, and shared domain types.

pub mod commit;
pub mod config;
pub mod diff;
pub mod envelope;
pub mod error;
pub mod guardrails;
pub mod repo;
pub mod status;
pub mod types;

pub use commit::{CommitOptions, CommitOutcome, commit_changes, compute_change_id, ensure_change_id_trailer};
pub use config::{
    DEFAULT_MAX_BASELINE_GB, DEFAULT_MAX_BLOB_MB, DEFAULT_WORKSPACE_TEMPLATE, DaemonConfig,
    GuardrailsConfig, OzoneConfig, WarmConfig, normalize_lexical, validate_workspace_name,
};
pub use diff::{
    BINARY_SNIFF_BYTES, DEFAULT_CONTEXT_LINES, DiffReport, compute_diff, file_id, hunk_id,
    hunk_suffix, is_binary_bytes, render_file_unified_diff, render_hunk_unified_diff,
    render_unified_diff,
};
pub use envelope::{
    ConflictEntry, Envelope, EnvelopeError, ExitCode, InteractionInput, InteractionMode,
    OZ_SCHEMA_V1, WarningEntry,
};
pub use error::{OzError, Result};
pub use guardrails::{
    GuardrailFinding, GuardrailKind, GuardrailOptions, GuardrailTarget, check_guardrails,
    check_workspace_guardrails, detect_secret_in_line, inspect_guardrails,
    is_conflict_marker_line, is_unignored_env_filename,
};
pub use repo::{
    CliInvocation, MountHealth, OzoneLayout, OzoneRepo, SiblingOzPath, find_git_ozone_on_path,
    is_enotconn, parse_oz_workspace_path, probe_mount_health_os,
};
pub use status::{ScopePolicy, StatusReport, compute_status};
pub use types::{
    BackendKind, ChangeKind, DiffHunk, DiffLine, DiffLineKind, DirtyEntry, FileChange,
    FilesystemKind, MountRecord, WorkspaceState,
};
