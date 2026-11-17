// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pre-commit guardrails for secrets / `.env` files, unresolved conflict markers,
//! oversized blobs, and workspace `--scope` / `--deny` path rules.

use crate::diff::is_binary_bytes;
use crate::error::{OzError, Result};
use crate::repo::OzoneRepo;
use crate::status::{ScopePolicy, compute_status_context, read_disk_bytes};
use crate::types::ChangeKind;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Allowed non-secret `.env.*` template filenames.
const ALLOWED_ENV_TEMPLATES: &[&str] = &[
    ".env.example",
    ".env.sample",
    ".env.template",
    ".env.dist",
];

/// Category of pre-commit guardrail violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailKind {
    Secret,
    ConflictMarker,
    OversizedBlob,
    OutOfScope,
}

impl GuardrailKind {
    /// Stable lowercase identifier reported in [`OzError::GuardrailViolation`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Secret => "secret",
            Self::ConflictMarker => "conflict_marker",
            Self::OversizedBlob => "oversized_blob",
            Self::OutOfScope => "out_of_scope",
        }
    }
}

/// Override flags passed to [`check_guardrails`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GuardrailOptions {
    /// Override secret, conflict-marker, and oversized-blob checks (`--force`).
    pub force: bool,
    /// Override workspace `--scope` and `--deny` path checks (`--allow-out-of-scope`).
    pub allow_out_of_scope: bool,
}

/// Candidate file payload inspected by [`check_guardrails`].
#[derive(Debug, Clone)]
pub struct GuardrailTarget<'a> {
    pub path: &'a str,
    pub kind: ChangeKind,
    pub content: &'a [u8],
    pub reported_size_bytes: Option<u64>,
    pub out_of_scope: bool,
    pub scope_reason: Option<String>,
}

impl<'a> GuardrailTarget<'a> {
    /// Constructs a target from path, change kind, and candidate blob bytes.
    #[must_use]
    pub fn new(path: &'a str, kind: ChangeKind, content: &'a [u8]) -> Self {
        Self {
            path,
            kind,
            content,
            reported_size_bytes: None,
            out_of_scope: false,
            scope_reason: None,
        }
    }
}

/// Individual guardrail finding discovered while inspecting candidate changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardrailFinding {
    pub kind: GuardrailKind,
    pub path: String,
    pub rule: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub message: String,
}

/// Inspects candidate commit targets against all configured guardrails and returns findings.
pub fn inspect_guardrails(
    repo: &OzoneRepo,
    targets: &[GuardrailTarget<'_>],
    opts: &GuardrailOptions,
) -> Result<Vec<GuardrailFinding>> {
    let scope_policy = ScopePolicy::from_repo(repo)?;
    let max_blob_bytes = repo.config.max_blob_mb.saturating_mul(1024 * 1024);
    let mut findings = Vec::new();

    for target in targets {
        if !opts.allow_out_of_scope {
            let reason = target
                .scope_reason
                .clone()
                .or_else(|| scope_policy.violation_reason(target.path));
            if let Some(msg) = reason {
                findings.push(GuardrailFinding {
                    kind: GuardrailKind::OutOfScope,
                    path: target.path.to_string(),
                    rule: "scope_deny".to_string(),
                    line: None,
                    message: msg,
                });
            } else if target.out_of_scope {
                findings.push(GuardrailFinding {
                    kind: GuardrailKind::OutOfScope,
                    path: target.path.to_string(),
                    rule: "scope_deny".to_string(),
                    line: None,
                    message: format!("'{}' violates workspace scope/deny policy", target.path),
                });
            }
        }

        // Deletions never introduce secrets, conflict markers, or oversized blobs.
        if target.kind == ChangeKind::Deleted || opts.force {
            continue;
        }

        let size_bytes = target
            .reported_size_bytes
            .unwrap_or(target.content.len() as u64);
        if size_bytes > max_blob_bytes {
            findings.push(GuardrailFinding {
                kind: GuardrailKind::OversizedBlob,
                path: target.path.to_string(),
                rule: "max_blob_mb".to_string(),
                line: None,
                message: format!(
                    "'{}' is {} bytes, exceeding max_blob_mb limit of {} MiB ({} bytes)",
                    target.path, size_bytes, repo.config.max_blob_mb, max_blob_bytes
                ),
            });
        }

        if repo.config.guardrails.check_secrets && is_unignored_env_filename(target.path) {
            findings.push(GuardrailFinding {
                kind: GuardrailKind::Secret,
                path: target.path.to_string(),
                rule: "dotenv_file".to_string(),
                line: None,
                message: format!(
                    "'{}' looks like an unignored environment secret file",
                    target.path
                ),
            });
        }

        if target.content.is_empty() || is_binary_bytes(target.content) {
            continue;
        }

        let text = String::from_utf8_lossy(target.content);
        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = (idx + 1) as u32;
            let line = raw_line.trim_end_matches('\r');

            if repo.config.guardrails.check_secrets
                && let Some(rule) = detect_secret_in_line(line)
            {
                findings.push(GuardrailFinding {
                    kind: GuardrailKind::Secret,
                    path: target.path.to_string(),
                    rule: rule.to_string(),
                    line: Some(line_no),
                    message: format!(
                        "suspected secret ({rule}) detected in '{}' at line {line_no}",
                        target.path
                    ),
                });
            }

            if repo.config.guardrails.check_conflict_markers && is_conflict_marker_line(line) {
                findings.push(GuardrailFinding {
                    kind: GuardrailKind::ConflictMarker,
                    path: target.path.to_string(),
                    rule: "conflict_marker".to_string(),
                    line: Some(line_no),
                    message: format!(
                        "unresolved conflict marker detected in '{}' at line {line_no}",
                        target.path
                    ),
                });
            }
        }
    }

    Ok(findings)
}

/// Validates candidate commit targets against all enabled guardrails, returning
/// [`OzError::GuardrailViolation`] (exit code `2`) with actionable recovery commands on failure.
pub fn check_guardrails(
    repo: &OzoneRepo,
    targets: &[GuardrailTarget<'_>],
    opts: &GuardrailOptions,
) -> Result<()> {
    let findings = inspect_guardrails(repo, targets, opts)?;
    if findings.is_empty() {
        return Ok(());
    }

    let primary_kind = findings[0].kind.as_str().to_string();
    let has_content_violation = findings
        .iter()
        .any(|f| f.kind != GuardrailKind::OutOfScope);
    let has_scope_violation = findings
        .iter()
        .any(|f| f.kind == GuardrailKind::OutOfScope);

    let summary = findings
        .iter()
        .take(5)
        .map(|f| f.message.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    let extra = if findings.len() > 5 {
        format!(" (+{} more)", findings.len() - 5)
    } else {
        String::new()
    };

    let mut recovery = Vec::new();
    if has_content_violation {
        recovery.push(
            "Remove the secret/conflict marker/large file or re-run with --force to override."
                .to_string(),
        );
        recovery.push("oz commit --force -m \"<message>\"".to_string());
    }
    if has_scope_violation {
        recovery.push(
            "Adjust workspace changes or re-run with --allow-out-of-scope to override.".to_string(),
        );
        recovery.push("oz commit --allow-out-of-scope -m \"<message>\"".to_string());
    }

    Err(OzError::GuardrailViolation {
        kind: primary_kind,
        message: format!("{summary}{extra}"),
        recovery,
    })
}

/// Runs [`check_guardrails`] across all current dirty files in `repo`.
pub fn check_workspace_guardrails(repo: &OzoneRepo, opts: &GuardrailOptions) -> Result<()> {
    let ctx = compute_status_context(repo)?;
    let max_blob_bytes = repo.config.max_blob_mb.saturating_mul(1024 * 1024);

    struct OwnedTarget {
        path: String,
        kind: ChangeKind,
        content: Vec<u8>,
        reported_size_bytes: Option<u64>,
        out_of_scope: bool,
        scope_reason: Option<String>,
    }

    let mut owned = Vec::with_capacity(ctx.dirty_files.len());
    for raw in &ctx.dirty_files {
        let out_of_scope = ctx.scope_policy.is_out_of_scope(&raw.path);
        let scope_reason = ctx.scope_policy.violation_reason(&raw.path);

        if raw.kind == ChangeKind::Deleted {
            owned.push(OwnedTarget {
                path: raw.path.clone(),
                kind: raw.kind,
                content: Vec::new(),
                reported_size_bytes: None,
                out_of_scope,
                scope_reason,
            });
            continue;
        }

        let disk_path = ctx.source.disk_path(&raw.path);
        let meta = fs::symlink_metadata(&disk_path)?;
        let len = meta.len();
        let content = if len > max_blob_bytes && raw.disk_kind != Some(gix::objs::tree::EntryKind::Link)
        {
            // Avoid loading a multi-gigabyte file into RAM just to reject it.
            Vec::new()
        } else {
            read_disk_bytes(&disk_path, raw.disk_kind)?
        };

        owned.push(OwnedTarget {
            path: raw.path.clone(),
            kind: raw.kind,
            content,
            reported_size_bytes: Some(len),
            out_of_scope,
            scope_reason,
        });
    }

    let borrowed: Vec<GuardrailTarget<'_>> = owned
        .iter()
        .map(|o| GuardrailTarget {
            path: &o.path,
            kind: o.kind,
            content: &o.content,
            reported_size_bytes: o.reported_size_bytes,
            out_of_scope: o.out_of_scope,
            scope_reason: o.scope_reason.clone(),
        })
        .collect();

    check_guardrails(repo, &borrowed, opts)
}

/// Returns `true` if `rel_path` has filename `.env` or `.env.*` other than allowed templates.
#[must_use]
pub fn is_unignored_env_filename(rel_path: &str) -> bool {
    let Some(name) = Path::new(rel_path).file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    if lower == ".env" {
        return true;
    }
    if lower.starts_with(".env.") {
        return !ALLOWED_ENV_TEMPLATES.contains(&lower.as_str());
    }
    false
}

/// Detects suspected secret patterns in a single text line without exposing the secret value.
#[must_use]
pub fn detect_secret_in_line(line: &str) -> Option<&'static str> {
    if let Some(pos) = line.find("-----BEGIN ")
        && line[pos..].contains("PRIVATE KEY-----")
    {
        return Some("private_key");
    }

    let bytes = line.as_bytes();
    if has_aws_access_key(bytes) {
        return Some("aws_access_key");
    }
    if has_github_classic_pat(bytes) {
        return Some("github_classic_pat");
    }
    if has_github_fine_grained_pat(bytes) {
        return Some("github_fine_grained_pat");
    }

    None
}

/// Returns `true` if `line` (without trailing `\r`/`\n`) is an unresolved merge conflict marker.
#[must_use]
pub fn is_conflict_marker_line(line: &str) -> bool {
    line.starts_with("<<<<<<< ") || line == "=======" || line.starts_with(">>>>>>> ")
}

fn is_key_boundary_before(bytes: &[u8], idx: usize) -> bool {
    idx == 0 || !(bytes[idx - 1].is_ascii_alphanumeric() || bytes[idx - 1] == b'_')
}

fn has_aws_access_key(bytes: &[u8]) -> bool {
    const PREFIX: &[u8] = b"AKIA";
    let total_len = PREFIX.len() + 16;
    if bytes.len() < total_len {
        return false;
    }
    for i in 0..=bytes.len() - total_len {
        if !bytes[i..].starts_with(PREFIX) || !is_key_boundary_before(bytes, i) {
            continue;
        }
        let body = &bytes[i + PREFIX.len()..i + total_len];
        let body_ok = body
            .iter()
            .all(|b| matches!(b, b'0'..=b'9' | b'A'..=b'Z'));
        let after_ok = i + total_len == bytes.len() || !bytes[i + total_len].is_ascii_alphanumeric();
        if body_ok && after_ok {
            return true;
        }
    }
    false
}

fn has_github_classic_pat(bytes: &[u8]) -> bool {
    const PREFIX: &[u8] = b"ghp_";
    let total_len = PREFIX.len() + 36;
    if bytes.len() < total_len {
        return false;
    }
    for i in 0..=bytes.len() - total_len {
        if !bytes[i..].starts_with(PREFIX) || !is_key_boundary_before(bytes, i) {
            continue;
        }
        let body = &bytes[i + PREFIX.len()..i + total_len];
        let body_ok = body.iter().all(|b| b.is_ascii_alphanumeric());
        let after_ok = i + total_len == bytes.len() || !bytes[i + total_len].is_ascii_alphanumeric();
        if body_ok && after_ok {
            return true;
        }
    }
    false
}

fn has_github_fine_grained_pat(bytes: &[u8]) -> bool {
    const PREFIX: &[u8] = b"github_pat_";
    if bytes.len() <= PREFIX.len() {
        return false;
    }
    for i in 0..bytes.len() - PREFIX.len() {
        if !bytes[i..].starts_with(PREFIX) || !is_key_boundary_before(bytes, i) {
            continue;
        }
        let next = bytes[i + PREFIX.len()];
        if next.is_ascii_alphanumeric() || next == b'_' {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_env_files_and_exempts_templates() {
        assert!(is_unignored_env_filename(".env"));
        assert!(is_unignored_env_filename("apps/api/.env.local"));
        assert!(is_unignored_env_filename(".env.production"));
        assert!(!is_unignored_env_filename(".env.example"));
        assert!(!is_unignored_env_filename(".env.sample"));
        assert!(!is_unignored_env_filename(".env.template"));
        assert!(!is_unignored_env_filename(".env.dist"));
        assert!(!is_unignored_env_filename("src/env.rs"));
    }

    #[test]
    fn detects_secret_and_conflict_marker_patterns() {
        let priv_key = concat!("-----BEGIN ", "RSA PRIVATE KEY-----");
        assert_eq!(detect_secret_in_line(priv_key), Some("private_key"));

        let aws = concat!("AWS_KEY=", "AKIA", "1234567890ABCDEF");
        assert_eq!(detect_secret_in_line(aws), Some("aws_access_key"));

        let ghp = concat!("token = \"", "ghp_", "abcdefghijklmnopqrstuvwxyz0123456789\"");
        assert_eq!(detect_secret_in_line(ghp), Some("github_classic_pat"));

        let pat = concat!("PAT=", "github_pat_", "11AAAAAA_testtoken");
        assert_eq!(detect_secret_in_line(pat), Some("github_fine_grained_pat"));

        assert!(is_conflict_marker_line("<<<<<<< HEAD"));
        assert!(is_conflict_marker_line("======="));
        assert!(is_conflict_marker_line(">>>>>>> feat/other"));
        assert!(!is_conflict_marker_line("======= divider"));
    }
}
