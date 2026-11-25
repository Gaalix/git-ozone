// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Core library for Ozone (`oz` / `git-ozone`).
//!
//! Provides colocated `.git` + `.git/ozone/` repository discovery, `.ozone.toml`
//! configuration parsing, canonical `oz.v1` JSON envelope formatting, non-interactive
//! detection, exit codes, and shared domain types.

mod attributes;
mod checkout;
mod commit;
mod config;
mod daily;
mod diff;
mod envelope;
mod error;
mod git_config;
mod guardrails;
mod hooks;
mod repo;
mod ssh_sign;
mod status;
mod types;

pub use attributes::{
    AttrState, AttributeResolver, ResolvedAttrs, clean_blob_bytes, clean_eol_bytes, is_lfs_pointer,
    smudge_blob_bytes, smudge_eol_bytes,
};
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
    GuardrailsConfig, OzoneConfig, SigningConfig, WarmConfig, default_aliases, normalize_lexical,
    validate_workspace_name,
};
pub use daily::{
    BranchCreateOutcome, BranchDeleteOutcome, BranchInfo, BranchListOptions, BranchListReport,
    BranchRenameOutcome, CherryPickOptions, CherryPickOutcome, GrepMatch, GrepOptions, GrepReport,
    LogCommitEntry, LogOptions, LogReport, OZ_STASH_REF, PreparedStashPop, RestoreOptions,
    RestoreOutcome, RevParseEntry, RevParseOptions, RevParseReport, RevertOptions, RevertOutcome,
    RevisionDiffOptions, ShowBlobOptions, ShowBlobReport, ShowFileStat, ShowOptions, ShowReport,
    ShowStatSummary, StashDropOutcome, StashEntryRecord, StashItem, StashListReport,
    StashPopOutcome, StashPushOutcome, TagCreateOptions, TagDeleteOutcome, TagInfo, TagListReport,
    TagVerifyOutcome, apply_stash_pop, branch_create, branch_delete, branch_list,
    branch_list_with_options, branch_rename, cherry_pick_commit, compute_revision_diff,
    extract_change_id, format_numbered_blob_lines, grep_repo, log_commits, parse_line_range,
    parse_rev_path_spec, parse_since_timestamp, parse_stash_selector, prepare_stash_pop,
    restore_paths, rev_parse, revert_commit, show_blob, show_commit, slice_text_lines, stash_drop,
    stash_list, stash_push, tag_create, tag_delete, tag_list, tag_verify,
};
pub use diff::{
    BINARY_SNIFF_BYTES, DEFAULT_CONTEXT_LINES, DiffReport, compute_diff, compute_diff_filtered,
    compute_diff_with_overrides, file_id, filter_changes_by_targets_and_excludes, hunk_id,
    hunk_suffix, is_binary_bytes, render_file_unified_diff, render_file_unified_diff_numbered,
    render_hunk_unified_diff, render_hunk_unified_diff_numbered, render_unified_diff,
    render_unified_diff_numbered,
};
pub use envelope::{
    ConflictEntry, Envelope, EnvelopeError, ExitCode, InteractionInput, InteractionMode,
    OZ_SCHEMA_V1, WarningEntry,
};
pub use error::{OzError, Result};
pub use git_config::{
    AutoCrlf, EolSetting, FilterDriverConfig, GitConfig, GitConfigContext, RemoteConfig,
    ResolvedRemoteUrls,
};
pub use guardrails::{
    GuardrailFinding, GuardrailKind, GuardrailOptions, GuardrailTarget, check_guardrails,
    check_workspace_guardrails, detect_secret_in_line, inspect_guardrails, is_conflict_marker_line,
    is_unignored_env_filename,
};
pub use hooks::{
    CommitHookEnv, HookRunOutcome, PrePushRefLine, find_executable_hook, resolve_hooks_dir,
    run_commit_msg, run_post_commit, run_pre_commit, run_pre_push, run_prepare_commit_msg,
};
pub use repo::{
    CliInvocation, MountHealth, OzoneLayout, OzoneRepo, SiblingOzPath, find_git_ozone_on_path,
    is_enotconn, parse_oz_workspace_path, probe_mount_health_os,
};
pub use ssh_sign::{
    CommitSignatureVerification, Ed25519SigningKey, Ed25519VerifyingKey, GIT_SSHSIG_NAMESPACE,
    OpenSshPrivateKey, OpenSshPublicKey, ParsedSshSignature, SSHSIG_ARMOR_FOOTER,
    SSHSIG_ARMOR_HEADER, SshSigningReport, expand_tilde_path, inspect_ssh_signing_config,
    match_allowed_signers_file, resolve_signing_private_key, sign_sshsig, verify_sshsig,
};
pub use status::{
    ScopePolicy, StatusReport, compute_status, compute_status_with_overrides,
    lookup_tree_entries_meta,
};
pub use types::{
    BackendKind, ChangeKind, DiffHunk, DiffLine, DiffLineKind, DirtyEntry, FileChange,
    FilesystemKind, MountRecord, WorkspaceState,
};
