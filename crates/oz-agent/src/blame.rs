// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-Rust line-level `oz blame <file>` engine walking commit history via `gix` (`gix::diff::blob`)
//! and joining originating commits and 1-indexed origin line numbers against `refs/notes/ai`.

use crate::AiProvenanceNote;
use crate::notes::{load_provenance_notes_gix, open_gix_with_fallback, resolve_commit_oid};
use chrono::{DateTime, TimeZone, Utc};
use gix::bstr::ByteSlice;
use gix::diff::blob::{Algorithm, Diff, Hunk, InternedInput, sources};
use gix::hash::ObjectId;
use gix::objs::tree::EntryKind;
use oz_core::{OzError, OzoneRepo, Result, WorkspaceState, is_binary_bytes};
use serde::{Deserialize, Serialize};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

/// Options controlling [`blame_file`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlameOptions {
    /// Target commit/branch/revision to blame from (defaults to `"HEAD"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

impl BlameOptions {
    /// Creates blame options targeting a specific revision.
    #[must_use]
    pub fn for_revision(revision: impl Into<String>) -> Self {
        Self {
            revision: Some(revision.into()),
        }
    }
}

/// Attribution details for a single 1-indexed line in a [`BlameReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameLine {
    /// 1-indexed line number in the blamed revision of `<path>`.
    pub line_number: u32,
    /// 1-indexed line number in `<path>` at the commit that introduced/last modified this line.
    pub origin_line_number: u32,
    /// Line text without trailing `\r` / `\n`.
    pub content: String,
    /// Full 40-hex OID of the originating commit.
    pub commit_oid: String,
    /// Abbreviated (8-char) commit OID for compact display.
    pub short_oid: String,
    pub author_name: String,
    pub author_email: String,
    pub timestamp: DateTime<Utc>,
    pub summary: String,
    /// `"agent"` when matched in `refs/notes/ai`, otherwise `"human"`.
    pub author_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Complete structured blame report returned by [`blame_file`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlameReport {
    pub path: String,
    pub revision: String,
    pub resolved_commit_oid: String,
    pub total_lines: usize,
    pub human_lines: usize,
    pub agent_lines: usize,
    pub agent_percentage: f64,
    pub lines: Vec<BlameLine>,
}

impl BlameReport {
    /// Formats this blame report for human-readable terminal display (`[AI:<client>]` vs `[human]`).
    #[must_use]
    pub fn render_human(&self) -> String {
        format_blame_report(self)
    }
}

#[derive(Debug, Clone, Copy)]
struct Suspect {
    final_idx: usize,
    line_idx: u32,
}

#[derive(Debug, Clone, Copy)]
struct LineOrigin {
    commit_id: ObjectId,
    origin_line_1: u32,
}

#[derive(Debug, Clone)]
struct CommitMeta {
    commit_hex: String,
    short_oid: String,
    author_name: String,
    author_email: String,
    timestamp: DateTime<Utc>,
    summary: String,
}

/// Computes line-level commit and `refs/notes/ai` attribution for `file_path` at `opts.revision`
/// (or `"HEAD"` when `None`).
pub fn blame_file(
    repo: &OzoneRepo,
    file_path: impl AsRef<Path>,
    opts: &BlameOptions,
) -> Result<BlameReport> {
    let rel_path = normalize_repo_rel_path(repo, file_path.as_ref())?;
    let gix_repo = open_gix_with_fallback(repo)?;

    let rev_label = opts
        .revision
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("HEAD")
        .to_string();

    let target_commit_id = resolve_blame_start_commit(repo, &gix_repo, &rev_label)?;
    let target_commit_hex = target_commit_id.to_hex().to_string();

    let target_commit = gix_repo
        .find_commit(target_commit_id)
        .map_err(OzError::git_source)?;
    let target_tree_id = target_commit
        .tree_id()
        .map_err(OzError::git_source)?
        .detach();

    let Some(target_blob_id) = lookup_blob_in_tree(&gix_repo, target_tree_id, &rel_path)? else {
        return Err(OzError::InvalidUsage {
            message: format!("file '{rel_path}' does not exist in revision '{rev_label}'"),
            recovery: vec!["oz status".to_string()],
        });
    };

    let target_blob = gix_repo
        .find_blob(target_blob_id)
        .map_err(OzError::git_source)?;
    if is_binary_bytes(&target_blob.data) {
        return Err(OzError::InvalidUsage {
            message: format!("cannot blame binary file '{rel_path}'"),
            recovery: Vec::new(),
        });
    }

    let final_lines: Vec<String> = sources::byte_lines(&target_blob.data)
        .map(|raw| {
            let stripped = raw
                .strip_suffix(b"\r\n")
                .or_else(|| raw.strip_suffix(b"\n"))
                .unwrap_or(raw);
            String::from_utf8_lossy(stripped).into_owned()
        })
        .collect();

    let total_lines = final_lines.len();
    if total_lines == 0 {
        return Ok(BlameReport {
            path: rel_path,
            revision: rev_label,
            resolved_commit_oid: target_commit_hex,
            total_lines: 0,
            human_lines: 0,
            agent_lines: 0,
            agent_percentage: 0.0,
            lines: Vec::new(),
        });
    }

    let origins = trace_line_origins(&gix_repo, target_commit_id, &rel_path, total_lines)?;
    let notes_map = load_provenance_notes_gix(&gix_repo)?;

    let mut meta_cache: HashMap<ObjectId, CommitMeta> = HashMap::new();
    let mut blame_lines = Vec::with_capacity(total_lines);
    let mut agent_lines = 0usize;
    let mut human_lines = 0usize;

    for (idx, content) in final_lines.into_iter().enumerate() {
        let line_num = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        let origin = origins[idx].unwrap_or(LineOrigin {
            commit_id: target_commit_id,
            origin_line_1: line_num,
        });

        let meta = if let Some(m) = meta_cache.get(&origin.commit_id) {
            m.clone()
        } else {
            let loaded = load_commit_meta(&gix_repo, origin.commit_id)?;
            meta_cache.insert(origin.commit_id, loaded.clone());
            loaded
        };

        let (author_kind, client, model, session_id) = match notes_map.get(&meta.commit_hex) {
            Some(note) => resolve_note_attribution(note, &rel_path, origin.origin_line_1),
            None => ("human".to_string(), None, None, None),
        };

        if author_kind == "agent" {
            agent_lines += 1;
        } else {
            human_lines += 1;
        }

        blame_lines.push(BlameLine {
            line_number: line_num,
            origin_line_number: origin.origin_line_1,
            content,
            commit_oid: meta.commit_hex,
            short_oid: meta.short_oid,
            author_name: meta.author_name,
            author_email: meta.author_email,
            timestamp: meta.timestamp,
            summary: meta.summary,
            author_kind,
            client,
            model,
            session_id,
        });
    }

    let agent_u128 = u128::try_from(agent_lines).unwrap_or(0);
    let total_u128 = u128::try_from(total_lines).unwrap_or(1).max(1);
    let basis_points = (agent_u128 * 20_000 + total_u128) / (2 * total_u128);
    let bp_u32 = u32::try_from(basis_points).unwrap_or(10_000);
    let agent_percentage = f64::from(bp_u32) / 100.0;

    Ok(BlameReport {
        path: rel_path,
        revision: rev_label,
        resolved_commit_oid: target_commit_hex,
        total_lines,
        human_lines,
        agent_lines,
        agent_percentage,
        lines: blame_lines,
    })
}

/// Formats a [`BlameReport`] into clean human-readable output distinguishing `[AI:<client>]` from `[human]`.
#[must_use]
pub fn format_blame_report(report: &BlameReport) -> String {
    let mut out = String::new();
    let short_rev = &report.resolved_commit_oid[..report.resolved_commit_oid.len().min(8)];
    let _ = writeln!(
        out,
        "{} @ {} ({} lines: {} human, {} agent / {:.2}% AI)",
        report.path,
        short_rev,
        report.total_lines,
        report.human_lines,
        report.agent_lines,
        report.agent_percentage
    );

    let line_width = report.total_lines.to_string().len().max(1);
    for line in &report.lines {
        let badge = if line.author_kind == "agent" {
            match line.client.as_deref() {
                Some(client) if !client.is_empty() => format!("[AI:{client}]"),
                _ => "[AI:agent]".to_string(),
            }
        } else {
            "[human]".to_string()
        };

        let actor_label = if line.author_kind == "agent" {
            line.session_id
                .as_deref()
                .unwrap_or(line.author_name.as_str())
        } else {
            line.author_name.as_str()
        };

        let _ = writeln!(
            out,
            "{} {:<16} ({:<14} {}) {:>width$} | {}",
            line.short_oid,
            badge,
            truncate_str(actor_label, 14),
            line.timestamp.format("%Y-%m-%d"),
            line.line_number,
            line.content,
            width = line_width
        );
    }

    out
}

fn trace_line_origins(
    gix_repo: &gix::Repository,
    start_commit_id: ObjectId,
    rel_path: &str,
    total_lines: usize,
) -> Result<Vec<Option<LineOrigin>>> {
    let mut resolved: Vec<Option<LineOrigin>> = vec![None; total_lines];
    let mut pending: HashMap<ObjectId, Vec<Suspect>> = HashMap::new();
    let mut heap: BinaryHeap<(i64, ObjectId)> = BinaryHeap::new();
    let mut in_heap: HashSet<ObjectId> = HashSet::new();
    let mut visited_guard: HashSet<(ObjectId, usize)> = HashSet::new();

    let initial_suspects: Vec<Suspect> = (0..total_lines)
        .map(|i| Suspect {
            final_idx: i,
            line_idx: u32::try_from(i).unwrap_or(u32::MAX),
        })
        .collect();

    enqueue_suspects(
        gix_repo,
        start_commit_id,
        initial_suspects,
        &mut pending,
        &mut heap,
        &mut in_heap,
        &mut visited_guard,
    )?;

    while let Some((_, commit_id)) = heap.pop() {
        in_heap.remove(&commit_id);
        let Some(suspects) = pending.remove(&commit_id) else {
            continue;
        };
        if suspects.is_empty() {
            continue;
        }

        let commit = gix_repo
            .find_commit(commit_id)
            .map_err(OzError::git_source)?;
        let tree_id = commit.tree_id().map_err(OzError::git_source)?.detach();

        let Some(curr_blob_id) = lookup_blob_in_tree(gix_repo, tree_id, rel_path)? else {
            for s in suspects {
                resolved[s.final_idx] = Some(LineOrigin {
                    commit_id,
                    origin_line_1: s.line_idx + 1,
                });
            }
            continue;
        };

        let parent_ids: Vec<ObjectId> = commit.parent_ids().map(gix::Id::detach).collect();
        if parent_ids.is_empty() {
            for s in suspects {
                resolved[s.final_idx] = Some(LineOrigin {
                    commit_id,
                    origin_line_1: s.line_idx + 1,
                });
            }
            continue;
        }

        let mut parent_blobs: Vec<(ObjectId, ObjectId)> = Vec::with_capacity(parent_ids.len());
        for pid in parent_ids {
            let p_commit = gix_repo.find_commit(pid).map_err(OzError::git_source)?;
            let p_tree_id = p_commit.tree_id().map_err(OzError::git_source)?.detach();
            if let Some(p_blob_id) = lookup_blob_in_tree(gix_repo, p_tree_id, rel_path)? {
                parent_blobs.push((pid, p_blob_id));
            }
        }

        if parent_blobs.is_empty() {
            // File was newly added in `commit_id`.
            for s in suspects {
                resolved[s.final_idx] = Some(LineOrigin {
                    commit_id,
                    origin_line_1: s.line_idx + 1,
                });
            }
            continue;
        }

        // Fast O(1) skip when any parent has the exact same blob OID for `rel_path`!
        if let Some(&(matching_parent_id, _)) =
            parent_blobs.iter().find(|&&(_, bid)| bid == curr_blob_id)
        {
            let mut forwarded = Vec::new();
            for s in suspects {
                if visited_guard.contains(&(matching_parent_id, s.final_idx)) {
                    resolved[s.final_idx] = Some(LineOrigin {
                        commit_id,
                        origin_line_1: s.line_idx + 1,
                    });
                } else {
                    forwarded.push(s);
                }
            }
            if !forwarded.is_empty() {
                enqueue_suspects(
                    gix_repo,
                    matching_parent_id,
                    forwarded,
                    &mut pending,
                    &mut heap,
                    &mut in_heap,
                    &mut visited_guard,
                )?;
            }
            continue;
        }

        let curr_blob = gix_repo
            .find_blob(curr_blob_id)
            .map_err(OzError::git_source)?;
        let curr_bytes = &curr_blob.data;

        let mut remaining = suspects;
        for (parent_id, parent_blob_id) in parent_blobs {
            if remaining.is_empty() {
                break;
            }
            let parent_blob = gix_repo
                .find_blob(parent_blob_id)
                .map_err(OzError::git_source)?;
            if is_binary_bytes(&parent_blob.data) {
                continue;
            }

            let input = InternedInput::new(parent_blob.data.as_slice(), curr_bytes.as_slice());
            let mut diff = Diff::compute(Algorithm::Histogram, &input);
            diff.postprocess_lines(&input);
            let hunks: Vec<Hunk> = diff.hunks().collect();

            let mut to_parent = Vec::new();
            let mut still_modified = Vec::new();
            for s in remaining {
                if let Some(parent_line_idx) = map_after_line_to_before(s.line_idx, &hunks)
                    && !visited_guard.contains(&(parent_id, s.final_idx))
                {
                    to_parent.push(Suspect {
                        final_idx: s.final_idx,
                        line_idx: parent_line_idx,
                    });
                } else {
                    still_modified.push(s);
                }
            }

            if !to_parent.is_empty() {
                enqueue_suspects(
                    gix_repo,
                    parent_id,
                    to_parent,
                    &mut pending,
                    &mut heap,
                    &mut in_heap,
                    &mut visited_guard,
                )?;
            }
            remaining = still_modified;
        }

        for s in remaining {
            resolved[s.final_idx] = Some(LineOrigin {
                commit_id,
                origin_line_1: s.line_idx + 1,
            });
        }
    }

    Ok(resolved)
}

fn enqueue_suspects(
    gix_repo: &gix::Repository,
    commit_id: ObjectId,
    suspects: Vec<Suspect>,
    pending: &mut HashMap<ObjectId, Vec<Suspect>>,
    heap: &mut BinaryHeap<(i64, ObjectId)>,
    in_heap: &mut HashSet<ObjectId>,
    visited_guard: &mut HashSet<(ObjectId, usize)>,
) -> Result<()> {
    let mut accepted = Vec::with_capacity(suspects.len());
    for s in suspects {
        if visited_guard.insert((commit_id, s.final_idx)) {
            accepted.push(s);
        }
    }
    if accepted.is_empty() {
        return Ok(());
    }

    pending.entry(commit_id).or_default().extend(accepted);
    if in_heap.insert(commit_id) {
        let ts = commit_committer_seconds(gix_repo, commit_id)?;
        heap.push((ts, commit_id));
    }
    Ok(())
}

fn map_after_line_to_before(after_line: u32, hunks: &[Hunk]) -> Option<u32> {
    let mut delta: i64 = 0;
    for h in hunks {
        if after_line < h.after.start {
            break;
        }
        if after_line < h.after.end {
            return None;
        }
        let before_len = i64::from(h.before.end - h.before.start);
        let after_len = i64::from(h.after.end - h.after.start);
        delta += after_len - before_len;
    }
    let mapped = i64::from(after_line) - delta;
    u32::try_from(mapped).ok()
}

fn resolve_note_attribution(
    note: &AiProvenanceNote,
    rel_path: &str,
    origin_line_1: u32,
) -> (String, Option<String>, Option<String>, Option<String>) {
    if note.line_ranges.is_empty() {
        return (
            "agent".to_string(),
            Some(note.client.clone()),
            note.model.clone(),
            Some(note.session_id.clone()),
        );
    }

    for range in &note.line_ranges {
        let range_path = range.path.replace('\\', "/");
        let range_path = range_path.strip_prefix("./").unwrap_or(&range_path);
        if range_path == rel_path
            && range.start_line <= origin_line_1
            && origin_line_1 <= range.end_line
        {
            let kind = if range.author_kind.trim().is_empty() {
                "agent".to_string()
            } else {
                range.author_kind.clone()
            };
            if kind == "human" {
                return ("human".to_string(), None, None, None);
            }
            return (
                kind,
                Some(note.client.clone()),
                note.model.clone(),
                Some(note.session_id.clone()),
            );
        }
    }

    ("human".to_string(), None, None, None)
}

fn resolve_blame_start_commit(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    rev_label: &str,
) -> Result<ObjectId> {
    if rev_label == "HEAD" && repo.workspace.is_some() && !repo.is_linked_worktree {
        let mut branch = repo.branch.clone();
        if let Some(ref ws_name) = repo.workspace {
            let state_path = repo.layout.workspace_state_path(ws_name);
            if let Ok(raw) = fs::read_to_string(state_path)
                && let Ok(state) = serde_json::from_str::<WorkspaceState>(&raw)
            {
                branch = Some(state.branch);
            }
        }
        if let Some(br) = branch {
            let full_ref = format!("refs/heads/{br}");
            return resolve_commit_oid(gix_repo, &full_ref);
        }
    }
    resolve_commit_oid(gix_repo, rev_label)
}

fn lookup_blob_in_tree(
    gix_repo: &gix::Repository,
    root_tree_id: ObjectId,
    rel_path: &str,
) -> Result<Option<ObjectId>> {
    let parts: Vec<&str> = rel_path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Ok(None);
    }

    let mut current_tree_id = root_tree_id;
    for (idx, comp) in parts.iter().enumerate() {
        let tree = gix_repo
            .find_tree(current_tree_id)
            .map_err(OzError::git_source)?;
        let mut found = None;
        for entry_res in tree.iter() {
            let entry = entry_res.map_err(OzError::git_source)?;
            if entry.filename() == comp.as_bytes() {
                found = Some((entry.mode(), entry.object_id()));
                break;
            }
        }
        let Some((mode, oid)) = found else {
            return Ok(None);
        };

        let is_last = idx + 1 == parts.len();
        if is_last {
            if matches!(mode.kind(), EntryKind::Blob | EntryKind::BlobExecutable) {
                return Ok(Some(oid));
            }
            return Ok(None);
        }
        if !mode.is_tree() {
            return Ok(None);
        }
        current_tree_id = oid;
    }

    Ok(None)
}

fn commit_committer_seconds(gix_repo: &gix::Repository, commit_id: ObjectId) -> Result<i64> {
    let commit = gix_repo
        .find_commit(commit_id)
        .map_err(OzError::git_source)?;
    let committer = commit.committer().map_err(OzError::git_source)?;
    let time = committer.time().unwrap_or_default();
    Ok(time.seconds)
}

fn load_commit_meta(gix_repo: &gix::Repository, commit_id: ObjectId) -> Result<CommitMeta> {
    let commit = gix_repo
        .find_commit(commit_id)
        .map_err(OzError::git_source)?;
    let author = commit
        .author()
        .map_err(OzError::git_source)?
        .to_owned()
        .map_err(OzError::git_source)?;

    let author_name = String::from_utf8_lossy(author.name.as_bytes()).into_owned();
    let author_email = String::from_utf8_lossy(author.email.as_bytes()).into_owned();
    let timestamp = Utc
        .timestamp_opt(author.time.seconds, 0)
        .single()
        .unwrap_or_else(Utc::now);

    let raw_msg = commit.message_raw_sloppy().to_str_lossy();
    let summary = raw_msg
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();

    let commit_hex = commit_id.to_hex().to_string();
    let short_oid = commit_hex[..commit_hex.len().min(8)].to_string();

    Ok(CommitMeta {
        commit_hex,
        short_oid,
        author_name,
        author_email,
        timestamp,
        summary,
    })
}

fn normalize_repo_rel_path(repo: &OzoneRepo, input: &Path) -> Result<String> {
    let stripped = if input.is_absolute() {
        input
            .strip_prefix(&repo.workdir_path)
            .or_else(|_| input.strip_prefix(&repo.repo_root))
            .map_err(|_| OzError::InvalidUsage {
                message: format!(
                    "path '{}' is outside repository '{}'",
                    input.display(),
                    repo.workdir_path.display()
                ),
                recovery: Vec::new(),
            })?
    } else {
        input
    };

    let raw = stripped.to_string_lossy().replace('\\', "/");
    let trimmed = raw.trim_start_matches("./").trim_matches('/').to_string();

    if trimmed.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "file path for `oz blame` cannot be empty".to_string(),
            recovery: Vec::new(),
        });
    }
    Ok(trimmed)
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else {
        s.chars().take(max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::{ProvenanceOptions, record_commit_provenance, write_provenance_note};
    use chrono::Utc;
    use oz_core::{CommitOptions, commit_changes};
    use std::process::Command;
    use tempfile::TempDir;

    fn init_repo() -> (TempDir, OzoneRepo) {
        let dir = TempDir::new().expect("tempdir");
        let s = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        assert!(s.success());
        for (k, v) in [
            ("user.name", "Dev Human"),
            ("user.email", "dev@example.com"),
        ] {
            let c = Command::new("git")
                .args(["config", k, v])
                .current_dir(dir.path())
                .status()
                .expect("git config");
            assert!(c.success());
        }
        let repo = OzoneRepo::discover(Some(dir.path())).expect("discover");
        (dir, repo)
    }

    #[test]
    fn blames_mixed_human_and_agent_commits_accurately_across_line_shifts() {
        let (dir, repo) = init_repo();

        // Commit 1 (Human): base auth module with unique function terminators (no refs/notes/ai entry).
        fs::create_dir_all(dir.path().join("src")).expect("mkdir src");
        fs::write(
            dir.path().join("src/auth.rs"),
            "pub struct Token {\n    pub raw: String,\n}\n\npub fn verify() -> bool {\n    true\n}\n// end auth\n",
        )
        .expect("write c1");
        let c1 =
            commit_changes(&repo, &CommitOptions::new("feat: human base auth")).expect("commit c1");

        // Commit 2 (Agent - claude-code): modifies verify() body and inserts revoke() before `// end auth`!
        let repo = OzoneRepo::discover(Some(dir.path())).expect("rediscover");
        fs::write(
            dir.path().join("src/auth.rs"),
            "pub struct Token {\n    pub raw: String,\n}\n\npub fn verify() -> bool {\n    !false\n}\n\npub fn revoke() {\n    // ai revoke\n}\n// end auth\n",
        )
        .expect("write c2");
        let c2 = commit_changes(
            &repo,
            &CommitOptions::new("feat(auth): agent verify & revoke"),
        )
        .expect("commit c2");
        let mut agent_opts = ProvenanceOptions::explicit("claude-code", "cc-sess-42");
        agent_opts.model = Some("claude-opus-4-6".to_string());
        record_commit_provenance(&repo, &c2.commit_oid, &agent_opts)
            .expect("record c2 provenance")
            .expect("note recorded");

        // Commit 3 (Human): inserts a 2-line header comment at the VERY TOP of `src/auth.rs`,
        // shifting all C1 and C2 line numbers down by +2!
        let repo = OzoneRepo::discover(Some(dir.path())).expect("rediscover");
        fs::write(
            dir.path().join("src/auth.rs"),
            "// Auth module\n// Maintained by team\npub struct Token {\n    pub raw: String,\n}\n\npub fn verify() -> bool {\n    !false\n}\n\npub fn revoke() {\n    // ai revoke\n}\n// end auth\n",
        )
        .expect("write c3");
        let c3 =
            commit_changes(&repo, &CommitOptions::new("docs: add file header")).expect("commit c3");

        let repo = OzoneRepo::discover(Some(dir.path())).expect("rediscover");
        let report = blame_file(&repo, "src/auth.rs", &BlameOptions::default()).expect("blame");

        assert_eq!(report.path, "src/auth.rs");
        assert_eq!(report.total_lines, 14);
        assert_eq!(report.agent_lines, 5);
        assert_eq!(report.human_lines, 9);
        assert_eq!(report.agent_percentage, 35.71);

        assert_eq!(report.lines[0].commit_oid, c3.commit_oid);
        assert_eq!(report.lines[0].author_kind, "human");
        assert_eq!(report.lines[0].origin_line_number, 1);

        assert_eq!(report.lines[2].commit_oid, c1.commit_oid);
        assert_eq!(report.lines[2].author_kind, "human");
        assert_eq!(report.lines[2].origin_line_number, 1);

        assert_eq!(report.lines[7].commit_oid, c2.commit_oid);
        assert_eq!(report.lines[7].author_kind, "agent");
        assert_eq!(report.lines[7].client.as_deref(), Some("claude-code"));
        assert_eq!(report.lines[7].model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(report.lines[7].session_id.as_deref(), Some("cc-sess-42"));
        assert_eq!(report.lines[7].origin_line_number, 6);
        assert_eq!(report.lines[7].content, "    !false");

        // Last line `// end auth` originated in C1 line 8!
        assert_eq!(report.lines[13].commit_oid, c1.commit_oid);
        assert_eq!(report.lines[13].author_kind, "human");
        assert_eq!(report.lines[13].origin_line_number, 8);
        assert_eq!(report.lines[13].content, "// end auth");

        let human_text = report.render_human();
        assert!(human_text.contains("[AI:claude-code]"));
        assert!(human_text.contains("[human]"));
    }

    #[test]
    fn supports_whole_commit_provenance_notes_when_line_ranges_empty() {
        let (dir, repo) = init_repo();
        fs::write(dir.path().join("hello.txt"), "one\ntwo\n").expect("write");
        let c1 =
            commit_changes(&repo, &CommitOptions::new("feat: whole commit note")).expect("commit");

        let whole_note = AiProvenanceNote {
            schema_version: 1,
            session_id: "wind-1".to_string(),
            client: "windsurf".to_string(),
            model: None,
            prompt_summary: None,
            timestamp: Utc::now(),
            line_ranges: Vec::new(),
        };
        write_provenance_note(&repo, &c1.commit_oid, &whole_note).expect("write note");

        let report = blame_file(&repo, "hello.txt", &BlameOptions::default()).expect("blame");
        assert_eq!(report.total_lines, 2);
        assert_eq!(report.agent_lines, 2);
        assert_eq!(report.human_lines, 0);
        assert_eq!(report.agent_percentage, 100.0);
        assert_eq!(report.lines[0].client.as_deref(), Some("windsurf"));
    }

    #[test]
    fn blames_across_merge_commits_preserving_feature_branch_ai_attribution() {
        let (dir, repo) = init_repo();
        fs::write(
            dir.path().join("app.rs"),
            "fn main() {\n    println!(\"v1\");\n}\n",
        )
        .expect("write v1");
        let base =
            commit_changes(&repo, &CommitOptions::new("feat: base app")).expect("commit base");

        // Create feature branch where an agent adds a helper function.
        let s = Command::new("git")
            .args(["checkout", "-b", "feat/agent-helper"])
            .current_dir(dir.path())
            .status()
            .expect("checkout feat");
        assert!(s.success());

        let feat_repo = OzoneRepo::discover(Some(dir.path())).expect("discover feat");
        fs::write(
            dir.path().join("app.rs"),
            "fn main() {\n    println!(\"v1\");\n}\n\nfn ai_helper() -> u32 {\n    7\n}\n",
        )
        .expect("write feat");
        let feat_commit = commit_changes(&feat_repo, &CommitOptions::new("feat: add ai_helper"))
            .expect("commit feat");
        record_commit_provenance(
            &feat_repo,
            &feat_commit.commit_oid,
            &ProvenanceOptions::explicit("cursor", "cur-merge-1"),
        )
        .expect("record feat provenance");

        // Advance `main` with a human header line, then create a real 2-parent merge commit!
        let s = Command::new("git")
            .args(["checkout", "main"])
            .current_dir(dir.path())
            .status()
            .expect("checkout main");
        assert!(s.success());

        let main_repo = OzoneRepo::discover(Some(dir.path())).expect("discover main");
        fs::write(
            dir.path().join("app.rs"),
            "// Main header\nfn main() {\n    println!(\"v1\");\n}\n",
        )
        .expect("write main header");
        let main_commit = commit_changes(&main_repo, &CommitOptions::new("docs: main header"))
            .expect("commit main header");

        let merge_status = Command::new("git")
            .args([
                "merge",
                "--no-ff",
                "-m",
                "Merge feat/agent-helper",
                "feat/agent-helper",
            ])
            .current_dir(dir.path())
            .status()
            .expect("git merge");
        assert!(merge_status.success());

        let merged_repo = OzoneRepo::discover(Some(dir.path())).expect("discover merged");
        let report = blame_file(&merged_repo, "app.rs", &BlameOptions::default()).expect("blame");

        assert_eq!(report.total_lines, 8);
        assert_eq!(report.lines[0].commit_oid, main_commit.commit_oid);
        assert_eq!(report.lines[0].author_kind, "human");
        assert_eq!(report.lines[1].commit_oid, base.commit_oid);
        assert_eq!(report.lines[1].author_kind, "human");
        // Agent helper lines trace through parent #2 (`feat/agent-helper`) rather than the merge commit!
        assert_eq!(report.lines[5].commit_oid, feat_commit.commit_oid);
        assert_eq!(report.lines[5].author_kind, "agent");
        assert_eq!(report.lines[5].client.as_deref(), Some("cursor"));
        assert_eq!(report.lines[5].session_id.as_deref(), Some("cur-merge-1"));
    }
}
