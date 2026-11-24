// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Histogram diff computation, deterministic hunk IDs (`h1a`, `h1b`, ...),
//! partial hunk reconstruction, and unified diff rendering.

use crate::error::{OzError, Result};
use crate::repo::OzoneRepo;
use crate::status::{compute_status, compute_status_with_overrides};
use crate::types::{ChangeKind, DiffHunk, DiffLine, DiffLineKind, FileChange};
use globset::{GlobBuilder, GlobMatcher};
use imara_diff::{Algorithm, Diff, InternedInput, sources};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// Number of leading bytes checked for NUL (`0x00`) to detect binary blobs.
pub const BINARY_SNIFF_BYTES: usize = 8192;

/// Number of unchanged context lines included before and after each diff hunk.
pub const DEFAULT_CONTEXT_LINES: u32 = 3;

/// Structured diff report produced by [`compute_diff`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffReport {
    pub workspace: Option<String>,
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_oid: Option<String>,
    pub files: Vec<FileChange>,
    pub total_additions: u32,
    pub total_deletions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawHunkSpan {
    pub raw_before_start: usize,
    pub raw_before_end: usize,
    pub raw_after_start: usize,
    pub raw_after_end: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ComputedFileDiff {
    pub is_binary: bool,
    pub additions: u32,
    pub deletions: u32,
    pub hunks: Vec<DiffHunk>,
    pub spans: Vec<RawHunkSpan>,
}

/// Returns `true` if `bytes` contains a NUL byte (`0x00`) in its first 8 KiB.
#[must_use]
pub fn is_binary_bytes(bytes: &[u8]) -> bool {
    let prefix = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    prefix.contains(&0)
}

/// Computes the deterministic alphabetic suffix for a 0-based hunk index:
/// `0 -> "a"`, `25 -> "z"`, `26 -> "aa"`, `27 -> "ab"`, etc.
#[must_use]
pub fn hunk_suffix(mut index: usize) -> String {
    let mut chars = Vec::new();
    loop {
        let rem = index % 26;
        chars.push((b'a' + rem as u8) as char);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    chars.reverse();
    chars.into_iter().collect()
}

/// Formats a deterministic hunk ID from a 1-based file number and 0-based hunk index
/// (e.g., `(1, 0) -> "h1a"`, `(1, 26) -> "h1aa"`, `(2, 0) -> "h2a"`).
#[must_use]
pub fn hunk_id(file_num_one_based: usize, hunk_index_zero_based: usize) -> String {
    format!("h{file_num_one_based}{}", hunk_suffix(hunk_index_zero_based))
}

/// Formats a deterministic file ID from a 1-based file index (`1 -> "f1"`).
#[must_use]
pub fn file_id(file_num_one_based: usize) -> String {
    format!("f{file_num_one_based}")
}

/// Normalizes a user-supplied path target (`./src\lib.rs` -> `src/lib.rs`).
#[must_use]
pub(crate) fn normalize_target_path(target: &str) -> String {
    let replaced = target.replace('\\', "/");
    let stripped = replaced.strip_prefix("./").unwrap_or(&replaced);
    stripped.trim_end_matches('/').to_string()
}

/// Returns `true` if `target` matches a file change path (exact file or directory prefix).
pub(crate) fn target_matches_file(target: &str, file: &FileChange) -> bool {
    if target == file.id {
        return true;
    }
    let norm = normalize_target_path(target);
    if norm.is_empty() {
        return false;
    }
    if file.path == norm {
        return true;
    }
    file.path
        .strip_prefix(&norm)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Computes Histogram diff hunks and deterministic IDs between `before` and `after` blobs.
#[must_use]
pub(crate) fn compute_file_diff(
    file_id_str: &str,
    file_num: usize,
    path: &str,
    before: &[u8],
    after: &[u8],
) -> ComputedFileDiff {
    if is_binary_bytes(before) || is_binary_bytes(after) {
        return ComputedFileDiff {
            is_binary: true,
            additions: 0,
            deletions: 0,
            hunks: Vec::new(),
            spans: Vec::new(),
        };
    }

    if before == after {
        return ComputedFileDiff {
            is_binary: false,
            additions: 0,
            deletions: 0,
            hunks: Vec::new(),
            spans: Vec::new(),
        };
    }

    let before_lines: Vec<&[u8]> = sources::byte_lines(before).collect();
    let after_lines: Vec<&[u8]> = sources::byte_lines(after).collect();

    let input = InternedInput::new(before, after);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let additions = diff.count_additions();
    let deletions = diff.count_removals();
    let raw_hunks: Vec<imara_diff::Hunk> = diff.hunks().collect();
    if raw_hunks.is_empty() {
        return ComputedFileDiff {
            is_binary: false,
            additions,
            deletions,
            hunks: Vec::new(),
            spans: Vec::new(),
        };
    }

    let mut groups: Vec<Vec<imara_diff::Hunk>> = Vec::new();
    let mut current_group: Vec<imara_diff::Hunk> = Vec::new();
    for rh in raw_hunks {
        if let Some(prev) = current_group.last()
            && rh.before.start.saturating_sub(prev.before.end) > 2 * DEFAULT_CONTEXT_LINES
        {
            groups.push(std::mem::take(&mut current_group));
        }
        current_group.push(rh);
    }
    if !current_group.is_empty() {
        groups.push(current_group);
    }

    let mut hunks = Vec::with_capacity(groups.len());
    let mut spans = Vec::with_capacity(groups.len());

    for (hunk_idx, group) in groups.into_iter().enumerate() {
        let first = &group[0];
        let last = &group[group.len() - 1];

        let ctx_before_start = first.before.start.saturating_sub(DEFAULT_CONTEXT_LINES);
        let leading_ctx = first.before.start - ctx_before_start;
        let ctx_after_start = first.after.start.saturating_sub(leading_ctx);

        let ctx_before_end =
            (last.before.end + DEFAULT_CONTEXT_LINES).min(before_lines.len() as u32);
        let trailing_ctx = ctx_before_end - last.before.end;
        let ctx_after_end = (last.after.end + trailing_ctx).min(after_lines.len() as u32);

        let old_lines = ctx_before_end - ctx_before_start;
        let new_lines = ctx_after_end - ctx_after_start;
        let old_start = if old_lines == 0 {
            0
        } else {
            ctx_before_start + 1
        };
        let new_start = if new_lines == 0 {
            0
        } else {
            ctx_after_start + 1
        };
        let header = format!("@@ -{old_start},{old_lines} +{new_start},{new_lines} @@");

        let mut lines = Vec::with_capacity((old_lines + new_lines) as usize);
        let mut cursor = ctx_before_start as usize;
        let mut old_cursor = ctx_before_start + 1;
        let mut new_cursor = ctx_after_start + 1;
        for rh in &group {
            for line_bytes in &before_lines[cursor..rh.before.start as usize] {
                lines.push(DiffLine {
                    kind: DiffLineKind::Context,
                    old_line: Some(old_cursor),
                    new_line: Some(new_cursor),
                    content: String::from_utf8_lossy(line_bytes).into_owned(),
                });
                old_cursor += 1;
                new_cursor += 1;
            }
            for line_bytes in &before_lines[rh.before.start as usize..rh.before.end as usize] {
                lines.push(DiffLine {
                    kind: DiffLineKind::Delete,
                    old_line: Some(old_cursor),
                    new_line: None,
                    content: String::from_utf8_lossy(line_bytes).into_owned(),
                });
                old_cursor += 1;
            }
            for line_bytes in &after_lines[rh.after.start as usize..rh.after.end as usize] {
                lines.push(DiffLine {
                    kind: DiffLineKind::Add,
                    old_line: None,
                    new_line: Some(new_cursor),
                    content: String::from_utf8_lossy(line_bytes).into_owned(),
                });
                new_cursor += 1;
            }
            cursor = rh.before.end as usize;
        }
        for line_bytes in &before_lines[cursor..ctx_before_end as usize] {
            lines.push(DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(old_cursor),
                new_line: Some(new_cursor),
                content: String::from_utf8_lossy(line_bytes).into_owned(),
            });
            old_cursor += 1;
            new_cursor += 1;
        }

        hunks.push(DiffHunk {
            id: hunk_id(file_num, hunk_idx),
            file_id: file_id_str.to_string(),
            path: path.to_string(),
            old_start,
            old_lines,
            new_start,
            new_lines,
            header,
            lines,
        });

        spans.push(RawHunkSpan {
            raw_before_start: first.before.start as usize,
            raw_before_end: last.before.end as usize,
            raw_after_start: first.after.start as usize,
            raw_after_end: last.after.end as usize,
        });
    }

    ComputedFileDiff {
        is_binary: false,
        additions,
        deletions,
        hunks,
        spans,
    }
}

/// Applies only the selected 0-based unified hunk indices from `(before, after)` onto
/// `before`, returning the resulting blob bytes without touching the working directory.
pub(crate) fn apply_selected_hunks(
    path: &str,
    before: &[u8],
    after: &[u8],
    selected_indices: &BTreeSet<usize>,
) -> Result<Vec<u8>> {
    let computed = compute_file_diff("f0", 0, path, before, after);
    if computed.is_binary {
        return Err(OzError::InvalidUsage {
            message: format!("cannot apply partial hunk selection to binary file '{path}'"),
            recovery: vec!["Select the whole file by path or file ID instead.".to_string()],
        });
    }

    let before_lines: Vec<&[u8]> = sources::byte_lines(before).collect();
    let after_lines: Vec<&[u8]> = sources::byte_lines(after).collect();

    for &idx in selected_indices {
        if idx >= computed.spans.len() {
            return Err(OzError::InvalidUsage {
                message: format!(
                    "hunk index {idx} out of range for '{path}' ({} hunks)",
                    computed.spans.len()
                ),
                recovery: vec!["Run `oz diff` to inspect current hunk IDs.".to_string()],
            });
        }
    }

    let mut out = Vec::with_capacity(before.len().max(after.len()));
    let mut cursor = 0usize;
    for (idx, span) in computed.spans.iter().enumerate() {
        if selected_indices.contains(&idx) {
            for line in &before_lines[cursor..span.raw_before_start] {
                out.extend_from_slice(line);
            }
            for line in &after_lines[span.raw_after_start..span.raw_after_end] {
                out.extend_from_slice(line);
            }
            cursor = span.raw_before_end;
        }
    }
    for line in &before_lines[cursor..] {
        out.extend_from_slice(line);
    }

    Ok(out)
}

/// Computes working-tree diffs against `HEAD`, optionally filtered by file path,
/// directory prefix, file ID (`f1`), hunk ID (`h1a`), or Git negative pathspecs (`:!pattern`).
///
/// Deterministic IDs (`f1`, `h1a`, ...) are always assigned across all dirty files
/// before applying `targets` filters.
pub fn compute_diff(repo: &OzoneRepo, targets: &[String]) -> Result<DiffReport> {
    compute_diff_filtered(repo, targets, &[])
}

/// Computes working-tree diffs against `HEAD` with explicit include `targets` and
/// `excludes` (file paths, directory prefixes, globs, file IDs `f1`, or hunk IDs `h1a`).
pub fn compute_diff_filtered(
    repo: &OzoneRepo,
    targets: &[String],
    excludes: &[String],
) -> Result<DiffReport> {
    let status = compute_status(repo)?;
    build_diff_report(status, targets, excludes)
}

/// Computes working-tree diffs with ephemeral `-c key=value` Git config overrides.
pub fn compute_diff_with_overrides(
    repo: &OzoneRepo,
    targets: &[String],
    overrides: &[(String, String)],
) -> Result<DiffReport> {
    let status = compute_status_with_overrides(repo, overrides)?;
    build_diff_report(status, targets, &[])
}

fn build_diff_report(
    status: crate::status::StatusReport,
    targets: &[String],
    excludes: &[String],
) -> Result<DiffReport> {
    let files = if targets.is_empty() && excludes.is_empty() {
        status.changes
    } else {
        filter_changes_by_targets_and_excludes(&status.changes, targets, excludes)?
    };

    let total_additions = files.iter().map(|f| f.additions).sum();
    let total_deletions = files.iter().map(|f| f.deletions).sum();

    Ok(DiffReport {
        workspace: status.workspace,
        branch: status.branch,
        head_oid: status.head_oid,
        base_oid: None,
        target_oid: None,
        files,
        total_additions,
        total_deletions,
    })
}

#[derive(Debug, Clone)]
struct ParsedPathPattern {
    raw: String,
    normalized: String,
    literal_only: bool,
    glob: Option<GlobMatcher>,
}

impl ParsedPathPattern {
    fn compile(raw_pattern: &str, literal_only: bool) -> Result<Self> {
        let normalized = normalize_target_path(raw_pattern);
        let has_glob_meta = !literal_only
            && (normalized.contains('*')
                || normalized.contains('?')
                || normalized.contains('[')
                || normalized.contains('{'));
        let glob = if has_glob_meta {
            let compiled = GlobBuilder::new(&normalized)
                .literal_separator(false)
                .build()
                .map_err(|err| OzError::InvalidUsage {
                    message: format!("invalid diff glob pattern '{raw_pattern}': {err}"),
                    recovery: vec![
                        "Use a valid glob or path, e.g. `oz diff --exclude '*.lock'`.".to_string(),
                    ],
                })?
                .compile_matcher();
            Some(compiled)
        } else {
            None
        };
        Ok(Self {
            raw: raw_pattern.to_string(),
            normalized,
            literal_only,
            glob,
        })
    }

    fn matches_file(&self, file: &FileChange) -> bool {
        if self.raw == file.id {
            return true;
        }
        if self.normalized.is_empty() {
            return false;
        }
        if file.path == self.normalized {
            return true;
        }
        if file
            .path
            .strip_prefix(&self.normalized)
            .is_some_and(|rest| rest.starts_with('/'))
        {
            return true;
        }
        if !self.literal_only
            && let Some(ref glob) = self.glob
            && glob.is_match(&file.path)
        {
            return true;
        }
        false
    }

    fn matches_hunk(&self, hunk: &DiffHunk) -> bool {
        self.raw == hunk.id
    }
}

fn parse_single_pathspec(raw: &str, force_exclude: bool) -> Result<(bool, ParsedPathPattern)> {
    let mut is_exclude = force_exclude;
    let mut literal_only = false;
    let body: &str;

    if let Some(rest) = raw.strip_prefix(":(") {
        let Some((magic_str, after)) = rest.split_once(')') else {
            return Err(OzError::InvalidUsage {
                message: format!("malformed Git pathspec magic '{raw}': missing closing ')'"),
                recovery: vec![
                    "Use `--exclude <path|glob|f1>` or `:!<path>` to exclude files.".to_string(),
                ],
            });
        };
        for token in magic_str.split(',').filter(|t| !t.is_empty()) {
            match token {
                "exclude" | "!" | "^" => is_exclude = true,
                "top" | "/" | "glob" => {}
                "literal" => literal_only = true,
                other => {
                    return Err(OzError::InvalidUsage {
                        message: format!(
                            "unsupported Git pathspec magic ':{other}' in '{raw}'"
                        ),
                        recovery: vec![
                            "Supported pathspec forms: `:!<path>`, `:(exclude)<path>`, `:(top)<path>`, `:(literal)<path>`, or `--exclude <path|glob|f1>`."
                                .to_string(),
                        ],
                    });
                }
            }
        }
        body = after.strip_prefix('/').unwrap_or(after);
    } else if let Some(rest) = raw.strip_prefix(":!").or_else(|| raw.strip_prefix(":^")) {
        is_exclude = true;
        body = rest.strip_prefix('/').unwrap_or(rest);
    } else if let Some(rest) = raw.strip_prefix(":/") {
        body = rest;
    } else {
        body = raw;
    }

    if body.is_empty() {
        return Err(OzError::InvalidUsage {
            message: format!("empty pathspec pattern '{raw}'"),
            recovery: vec![
                "Pass a file path, directory prefix, glob, or file/hunk ID (e.g. `--exclude pnpm-lock.yaml`)."
                    .to_string(),
            ],
        });
    }

    Ok((is_exclude, ParsedPathPattern::compile(body, literal_only)?))
}

/// Filters `changes` by include `targets` (including Git negative pathspecs `:!`, `:^`,
/// `:(exclude)`) and explicit `excludes`. Fails loudly on unrecognized `:(...)` pathspec magic.
pub fn filter_changes_by_targets_and_excludes(
    changes: &[FileChange],
    targets: &[String],
    excludes: &[String],
) -> Result<Vec<FileChange>> {
    let mut include_pats = Vec::new();
    let mut exclude_pats = Vec::new();

    for raw in targets {
        let (is_exc, pat) = parse_single_pathspec(raw, false)?;
        if is_exc {
            exclude_pats.push(pat);
        } else {
            include_pats.push(pat);
        }
    }
    for raw in excludes {
        let (_, pat) = parse_single_pathspec(raw, true)?;
        exclude_pats.push(pat);
    }

    // 1. Apply positive include filters (if any). When only excludes are given, start from all files.
    let included: Vec<FileChange> = if include_pats.is_empty() {
        changes.to_vec()
    } else {
        let mut out = Vec::new();
        for file in changes {
            if include_pats.iter().any(|p| p.matches_file(file)) {
                out.push(file.clone());
                continue;
            }
            let matching_hunks: Vec<DiffHunk> = file
                .hunks
                .iter()
                .filter(|h| include_pats.iter().any(|p| p.matches_hunk(h)))
                .cloned()
                .collect();
            if !matching_hunks.is_empty() {
                let (additions, deletions) = count_hunk_line_deltas(&matching_hunks);
                let mut filtered = file.clone();
                filtered.hunks = matching_hunks;
                filtered.additions = additions;
                filtered.deletions = deletions;
                out.push(filtered);
            }
        }
        out
    };

    if exclude_pats.is_empty() {
        return Ok(included);
    }

    // 2. Subtract file-level and hunk-level exclusions.
    let mut result = Vec::with_capacity(included.len());
    for file in included {
        if exclude_pats.iter().any(|p| p.matches_file(&file)) {
            continue;
        }
        if !file.hunks.is_empty()
            && file
                .hunks
                .iter()
                .any(|h| exclude_pats.iter().any(|p| p.matches_hunk(h)))
        {
            let kept_hunks: Vec<DiffHunk> = file
                .hunks
                .iter()
                .filter(|h| !exclude_pats.iter().any(|p| p.matches_hunk(h)))
                .cloned()
                .collect();
            if kept_hunks.is_empty() {
                continue;
            }
            let (additions, deletions) = count_hunk_line_deltas(&kept_hunks);
            let mut filtered = file;
            filtered.hunks = kept_hunks;
            filtered.additions = additions;
            filtered.deletions = deletions;
            result.push(filtered);
        } else {
            result.push(file);
        }
    }
    Ok(result)
}

fn count_hunk_line_deltas(hunks: &[DiffHunk]) -> (u32, u32) {
    let mut additions = 0u32;
    let mut deletions = 0u32;
    for hunk in hunks {
        for line in &hunk.lines {
            match line.kind {
                DiffLineKind::Add => additions += 1,
                DiffLineKind::Delete => deletions += 1,
                DiffLineKind::Context => {}
            }
        }
    }
    (additions, deletions)
}

/// Renders an entire [`DiffReport`] as a unified diff string with deterministic IDs.
#[must_use]
pub fn render_unified_diff(report: &DiffReport) -> String {
    let mut out = String::new();
    for file in &report.files {
        out.push_str(&render_file_unified_diff(file));
    }
    out
}

/// Renders an entire [`DiffReport`] with explicit `old` and `new` line numbers on every line.
#[must_use]
pub fn render_unified_diff_numbered(report: &DiffReport) -> String {
    let mut out = String::new();
    for file in &report.files {
        out.push_str(&render_file_unified_diff_numbered(file));
    }
    out
}

/// Renders a single [`FileChange`] as a unified diff section.
#[must_use]
pub fn render_file_unified_diff(file: &FileChange) -> String {
    render_file_unified_diff_impl(file, false)
}

/// Renders a single [`FileChange`] as a line-numbered unified diff section.
#[must_use]
pub fn render_file_unified_diff_numbered(file: &FileChange) -> String {
    render_file_unified_diff_impl(file, true)
}

fn render_file_unified_diff_impl(file: &FileChange, numbered: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "diff --ozone [{}] a/{} b/{}", file.id, file.path, file.path);
    match file.kind {
        ChangeKind::Added | ChangeKind::Untracked => {
            let _ = writeln!(out, "--- /dev/null");
            let _ = writeln!(out, "+++ b/{}", file.path);
        }
        ChangeKind::Deleted => {
            let _ = writeln!(out, "--- a/{}", file.path);
            let _ = writeln!(out, "+++ /dev/null");
        }
        _ => {
            let _ = writeln!(out, "--- a/{}", file.path);
            let _ = writeln!(out, "+++ b/{}", file.path);
        }
    }

    if file.is_binary {
        let _ = writeln!(out, "Binary files a/{} and b/{} differ", file.path, file.path);
        return out;
    }

    for hunk in &file.hunks {
        if numbered {
            out.push_str(&render_hunk_unified_diff_numbered(hunk));
        } else {
            out.push_str(&render_hunk_unified_diff(hunk));
        }
    }
    out
}

/// Renders a single [`DiffHunk`] including its unified header and hunk ID annotation.
#[must_use]
pub fn render_hunk_unified_diff(hunk: &DiffHunk) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{} [{}]", hunk.header, hunk.id);
    for line in &hunk.lines {
        let prefix = match line.kind {
            DiffLineKind::Context => ' ',
            DiffLineKind::Add => '+',
            DiffLineKind::Delete => '-',
        };
        out.push(prefix);
        out.push_str(&line.content);
        if !line.content.ends_with('\n') {
            out.push_str("\n\\ No newline at end of file\n");
        }
    }
    out
}

/// Renders a single [`DiffHunk`] with explicit 1-based `old` and `new` line numbers on each line.
#[must_use]
pub fn render_hunk_unified_diff_numbered(hunk: &DiffHunk) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{} [{}]", hunk.header, hunk.id);
    let max_line = hunk
        .lines
        .iter()
        .flat_map(|l| [l.old_line, l.new_line])
        .flatten()
        .max()
        .unwrap_or(1);
    let width = max_line.to_string().len().max(4);
    for line in &hunk.lines {
        let prefix = match line.kind {
            DiffLineKind::Context => ' ',
            DiffLineKind::Add => '+',
            DiffLineKind::Delete => '-',
        };
        match (line.old_line, line.new_line) {
            (Some(old), Some(new)) => {
                let _ = write!(out, "{old:>width$} {new:>width$} | {prefix}");
            }
            (Some(old), None) => {
                let _ = write!(out, "{old:>width$} {:>width$} | {prefix}", "");
            }
            (None, Some(new)) => {
                let _ = write!(out, "{:>width$} {new:>width$} | {prefix}", "");
            }
            (None, None) => {
                let _ = write!(out, "{:>width$} {:>width$} | {prefix}", "", "");
            }
        }
        out.push_str(&line.content);
        if !line.content.ends_with('\n') {
            out.push_str("\n\\ No newline at end of file\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_hunk_ids_past_26() {
        assert_eq!(hunk_id(1, 0), "h1a");
        assert_eq!(hunk_id(1, 25), "h1z");
        assert_eq!(hunk_id(1, 26), "h1aa");
        assert_eq!(hunk_id(1, 27), "h1ab");
        assert_eq!(hunk_id(1, 51), "h1az");
        assert_eq!(hunk_id(1, 52), "h1ba");
        assert_eq!(hunk_id(2, 0), "h2a");
    }

    #[test]
    fn detects_binary_within_first_8kib() {
        let mut text = vec![b'a'; 9000];
        assert!(!is_binary_bytes(&text));
        text[8500] = 0;
        assert!(!is_binary_bytes(&text));
        text[100] = 0;
        assert!(is_binary_bytes(&text));
    }

    #[test]
    fn applies_selected_hunks_preserving_unselected_hunk() {
        let before: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let mut after_lines: Vec<String> = (1..=30).map(|i| format!("line {i}\n")).collect();
        after_lines[1] = "line 2 edited A\n".to_string();
        after_lines[25] = "line 26 edited B\n".to_string();
        let after: String = after_lines.concat();

        let computed = compute_file_diff("f1", 1, "src/lib.rs", before.as_bytes(), after.as_bytes());
        assert_eq!(computed.hunks.len(), 2);
        assert_eq!(computed.hunks[0].id, "h1a");
        assert_eq!(computed.hunks[1].id, "h1b");

        let mut picked = BTreeSet::new();
        picked.insert(0);
        let partial =
            apply_selected_hunks("src/lib.rs", before.as_bytes(), after.as_bytes(), &picked)
                .expect("apply h1a");
        let partial_str = String::from_utf8(partial).expect("utf8");
        assert!(partial_str.contains("line 2 edited A\n"));
        assert!(!partial_str.contains("line 26 edited B\n"));
        assert!(partial_str.contains("line 26\n"));
    }

    #[test]
    fn diff_lines_carry_exact_old_and_new_line_numbers() {
        let before = "a\nb\nc\nd\ne\n";
        let after = "a\nb_mod\nc\nd\ne\nf_added\n";
        let computed = compute_file_diff("f1", 1, "demo.txt", before.as_bytes(), after.as_bytes());
        assert_eq!(computed.hunks.len(), 1);
        let h = &computed.hunks[0];

        // Context line 1 ("a\n")
        assert_eq!(h.lines[0].kind, DiffLineKind::Context);
        assert_eq!(h.lines[0].old_line, Some(1));
        assert_eq!(h.lines[0].new_line, Some(1));
        // Delete line 2 ("b\n")
        assert_eq!(h.lines[1].kind, DiffLineKind::Delete);
        assert_eq!(h.lines[1].old_line, Some(2));
        assert_eq!(h.lines[1].new_line, None);
        // Add line 2 ("b_mod\n")
        assert_eq!(h.lines[2].kind, DiffLineKind::Add);
        assert_eq!(h.lines[2].old_line, None);
        assert_eq!(h.lines[2].new_line, Some(2));
        // Added line 6 ("f_added\n")
        let last = h.lines.last().unwrap();
        assert_eq!(last.kind, DiffLineKind::Add);
        assert_eq!(last.old_line, None);
        assert_eq!(last.new_line, Some(6));

        let numbered = render_hunk_unified_diff_numbered(h);
        assert!(numbered.contains("   1    1 |  a\n"));
        assert!(numbered.contains("   2      | -b\n"));
        assert!(numbered.contains("        2 | +b_mod\n"));
        assert!(numbered.contains("        6 | +f_added\n"));
    }

    #[test]
    fn diff_excludes_and_negative_pathspecs_work_and_reject_unknown_magic() {
        let mk_file = |id: &str, path: &str| FileChange {
            id: id.to_string(),
            path: path.to_string(),
            old_path: None,
            kind: ChangeKind::Modified,
            additions: 1,
            deletions: 0,
            is_binary: false,
            pinned: false,
            out_of_scope: false,
            hunks: Vec::new(),
        };
        let changes = vec![
            mk_file("f1", "pnpm-lock.yaml"),
            mk_file("f2", "src/app.ts"),
            mk_file("f3", "src/util.ts"),
        ];

        // Exclusion-only negative Git pathspec starts from full set (no silent 0!)
        let filtered = filter_changes_by_targets_and_excludes(
            &changes,
            &[":!pnpm-lock.yaml".to_string()],
            &[],
        )
        .expect("negative pathspec");
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|f| f.path != "pnpm-lock.yaml"));

        // Explicit --exclude with glob
        let filtered_glob = filter_changes_by_targets_and_excludes(
            &changes,
            &[],
            &["*.yaml".to_string()],
        )
        .expect("glob exclude");
        assert_eq!(filtered_glob.len(), 2);

        // :(exclude) syntax
        let filtered_magic = filter_changes_by_targets_and_excludes(
            &changes,
            &["src".to_string(), ":(exclude)src/util.ts".to_string()],
            &[],
        )
        .expect("magic exclude");
        assert_eq!(filtered_magic.len(), 1);
        assert_eq!(filtered_magic[0].path, "src/app.ts");

        // Unknown :(magic) must fail loudly
        let err = filter_changes_by_targets_and_excludes(
            &changes,
            &[":(icase)src/app.ts".to_string()],
            &[],
        )
        .expect_err("unknown magic must fail");
        assert!(err.to_string().contains("unsupported Git pathspec magic"));
    }
}
