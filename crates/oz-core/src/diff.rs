// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Histogram diff computation, deterministic hunk IDs (`h1a`, `h1b`, ...),
//! partial hunk reconstruction, and unified diff rendering.

use crate::error::{OzError, Result};
use crate::repo::OzoneRepo;
use crate::status::compute_status;
use crate::types::{ChangeKind, DiffHunk, DiffLine, DiffLineKind, FileChange};
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
        for rh in &group {
            for line_bytes in &before_lines[cursor..rh.before.start as usize] {
                lines.push(DiffLine {
                    kind: DiffLineKind::Context,
                    content: String::from_utf8_lossy(line_bytes).into_owned(),
                });
            }
            for line_bytes in &before_lines[rh.before.start as usize..rh.before.end as usize] {
                lines.push(DiffLine {
                    kind: DiffLineKind::Delete,
                    content: String::from_utf8_lossy(line_bytes).into_owned(),
                });
            }
            for line_bytes in &after_lines[rh.after.start as usize..rh.after.end as usize] {
                lines.push(DiffLine {
                    kind: DiffLineKind::Add,
                    content: String::from_utf8_lossy(line_bytes).into_owned(),
                });
            }
            cursor = rh.before.end as usize;
        }
        for line_bytes in &before_lines[cursor..ctx_before_end as usize] {
            lines.push(DiffLine {
                kind: DiffLineKind::Context,
                content: String::from_utf8_lossy(line_bytes).into_owned(),
            });
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
/// directory prefix, file ID (`f1`), or hunk ID (`h1a`).
///
/// Deterministic IDs (`f1`, `h1a`, ...) are always assigned across all dirty files
/// before applying `targets` filters.
pub fn compute_diff(repo: &OzoneRepo, targets: &[String]) -> Result<DiffReport> {
    let status = compute_status(repo)?;
    let files = if targets.is_empty() {
        status.changes
    } else {
        filter_changes_by_targets(&status.changes, targets)
    };

    let total_additions = files.iter().map(|f| f.additions).sum();
    let total_deletions = files.iter().map(|f| f.deletions).sum();

    Ok(DiffReport {
        workspace: status.workspace,
        branch: status.branch,
        head_oid: status.head_oid,
        files,
        total_additions,
        total_deletions,
    })
}

fn filter_changes_by_targets(changes: &[FileChange], targets: &[String]) -> Vec<FileChange> {
    let mut out = Vec::new();
    for file in changes {
        if targets.iter().any(|t| target_matches_file(t, file)) {
            out.push(file.clone());
            continue;
        }
        let matching_hunks: Vec<DiffHunk> = file
            .hunks
            .iter()
            .filter(|h| targets.iter().any(|t| t == &h.id))
            .cloned()
            .collect();
        if !matching_hunks.is_empty() {
            let mut additions = 0u32;
            let mut deletions = 0u32;
            for hunk in &matching_hunks {
                for line in &hunk.lines {
                    match line.kind {
                        DiffLineKind::Add => additions += 1,
                        DiffLineKind::Delete => deletions += 1,
                        DiffLineKind::Context => {}
                    }
                }
            }
            let mut filtered = file.clone();
            filtered.hunks = matching_hunks;
            filtered.additions = additions;
            filtered.deletions = deletions;
            out.push(filtered);
        }
    }
    out
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

/// Renders a single [`FileChange`] as a unified diff section.
#[must_use]
pub fn render_file_unified_diff(file: &FileChange) -> String {
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
        out.push_str(&render_hunk_unified_diff(hunk));
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
}
