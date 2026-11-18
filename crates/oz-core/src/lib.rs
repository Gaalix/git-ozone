// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Core library for Ozone (`oz` / `git-ozone`).
//!
//! Provides colocated `.git` + `.git/ozone/` repository discovery, `.ozone.toml`
//! configuration parsing, canonical `oz.v1` JSON envelope formatting, non-interactive
//! detection, exit codes, and shared domain types.

pub mod checkout;
pub mod commit;
pub mod config;
pub mod diff;
pub mod envelope;
pub mod error;
pub mod guardrails;
pub mod repo;
pub mod ssh_sign;
pub mod status;
pub mod types;

pub use checkout::{
    CheckoutPathsOptions, CheckoutPathsOutcome, PathspecMatcher, SwitchOptions, SwitchOutcome,
    checkout_paths, create_or_reset_local_branch, is_local_branch, list_git_worktree_entries,
    list_tracked_paths_in_head, resolves_as_revision, switch_branch,
};

pub use commit::{
    CommitOidArg, CommitOptions, CommitOutcome, add_pinned_paths, clear_pinned_paths,
    commit_changes, commit_changes_ignoring_pinned, compute_change_id, ensure_change_id_trailer,
    read_pinned_paths, remove_pinned_paths, verify_commit_signature,
    verify_commit_signature_with_allowed_signers, write_pinned_paths,
};
pub use config::{
    DEFAULT_MAX_BASELINE_GB, DEFAULT_MAX_BLOB_MB, DEFAULT_WORKSPACE_TEMPLATE, DaemonConfig,
    GuardrailsConfig, OzoneConfig, SigningConfig, WarmConfig, normalize_lexical,
    validate_workspace_name,
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
pub use ssh_sign::{
    CommitSignatureVerification, Ed25519SigningKey, Ed25519VerifyingKey, GIT_SSHSIG_NAMESPACE,
    OpenSshPrivateKey, OpenSshPublicKey, ParsedSshSignature, SSHSIG_ARMOR_FOOTER,
    SSHSIG_ARMOR_HEADER, Sha512, SshSigningReport, base64_decode, base64_encode,
    base64_encode_no_pad, expand_tilde_path, inspect_ssh_signing_config,
    match_allowed_signers_file, resolve_signing_private_key, sha256, sha512, sign_sshsig,
    verify_sshsig,
};
pub use status::{ScopePolicy, StatusReport, compute_status};
pub use types::{
    BackendKind, ChangeKind, DiffHunk, DiffLine, DiffLineKind, DirtyEntry, FileChange,
    FilesystemKind, MountRecord, WorkspaceState,
};
