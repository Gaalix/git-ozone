// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-Rust `refs/notes/ai` Git notes engine and automatic line-level AI commit provenance
//! recorder built on `gix` + `imara-diff` (100% interoperable with `git notes --ref=ai`).

use crate::{AiProvenanceNote, LineRangeAttribution};
use chrono::Utc;
use gix::bstr::ByteSlice;
use gix::hash::ObjectId;
use gix::objs::tree::EntryKind;
use gix::refs::FullName;
use gix::refs::Target;
use gix::refs::transaction::{PreviousValue, RefEdit};
use imara_diff::{Algorithm, Diff, InternedInput, sources};
use oz_core::{OzError, OzoneRepo, Result, is_binary_bytes};
use std::collections::BTreeMap;

/// Canonical Git notes reference storing structured AI commit provenance.
pub const AI_NOTES_REF: &str = "refs/notes/ai";

/// Current schema version for [`AiProvenanceNote`].
pub const AI_PROVENANCE_SCHEMA_VERSION: u32 = 1;

const MAX_CAS_RETRIES: usize = 8;

/// Active coding agent session metadata detected from options or environment variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedAgentSession {
    pub session_id: String,
    pub client: String,
    pub model: Option<String>,
    pub prompt_summary: Option<String>,
}

/// Options controlling [`record_commit_provenance`].
#[derive(Debug, Clone)]
pub struct ProvenanceOptions {
    pub session_id: Option<String>,
    pub client: Option<String>,
    pub model: Option<String>,
    pub prompt_summary: Option<String>,
    /// Record provenance as an agent commit even when no agent environment variables are set.
    pub force_agent: bool,
    /// Inspect `std::env::vars()` when `env_overrides` is `None` (default: `true`).
    pub inspect_env: bool,
    /// Optional explicit environment map for deterministic testing.
    pub env_overrides: Option<BTreeMap<String, String>>,
}

impl Default for ProvenanceOptions {
    fn default() -> Self {
        Self {
            session_id: None,
            client: None,
            model: None,
            prompt_summary: None,
            force_agent: false,
            inspect_env: true,
            env_overrides: None,
        }
    }
}

impl ProvenanceOptions {
    /// Creates explicit agent provenance options that do not depend on ambient process env.
    #[must_use]
    pub fn explicit(client: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            session_id: Some(session_id.into()),
            client: Some(client.into()),
            model: None,
            prompt_summary: None,
            force_agent: true,
            inspect_env: false,
            env_overrides: None,
        }
    }
}

/// Detects an active coding agent session from the current process environment.
#[must_use]
pub fn detect_agent_session() -> Option<DetectedAgentSession> {
    let env_map: BTreeMap<String, String> = std::env::vars().collect();
    detect_agent_session_from_map(&env_map)
}

/// Detects an active coding agent session from an explicit environment map.
///
/// Inspects `OZ_AGENT_SESSION`, `OZ_AGENT_CLIENT`, `OZ_AGENT_MODEL`, `OZ_AGENT_PROMPT`,
/// `CLAUDE_CODE_*` / `CLAUDECODE`, `CURSOR_TRACE_ID`, `CODEX_*`, `WINDSURF_*`,
/// `ANTIGRAVITY_*`, and `OZ_AGENT=1`.
#[must_use]
pub fn detect_agent_session_from_map(
    env: &BTreeMap<String, String>,
) -> Option<DetectedAgentSession> {
    let explicit_session = nonempty_env(env, "OZ_AGENT_SESSION");
    let explicit_client = nonempty_env(env, "OZ_AGENT_CLIENT");
    let explicit_model = nonempty_env(env, "OZ_AGENT_MODEL");
    let explicit_prompt = nonempty_env(env, "OZ_AGENT_PROMPT");

    let mut detected_client: Option<&'static str> = None;
    let mut detected_session: Option<String> = None;
    let mut detected_model: Option<String> = None;

    if is_truthy_or_set(env, "CLAUDECODE") || has_any_prefix(env, "CLAUDE_CODE_") {
        detected_client = Some("claude-code");
        detected_session = nonempty_env(env, "CLAUDE_CODE_SESSION_ID")
            .or_else(|| nonempty_env(env, "CLAUDE_CODE_SESSION"))
            .or_else(|| first_prefixed_id(env, "CLAUDE_CODE_"));
        detected_model =
            nonempty_env(env, "ANTHROPIC_MODEL").or_else(|| nonempty_env(env, "CLAUDE_MODEL"));
    } else if nonempty_env(env, "CURSOR_TRACE_ID").is_some()
        || nonempty_env(env, "CURSOR_SESSION_ID").is_some()
        || is_truthy_or_set(env, "CURSOR_AGENT")
    {
        detected_client = Some("cursor");
        detected_session =
            nonempty_env(env, "CURSOR_TRACE_ID").or_else(|| nonempty_env(env, "CURSOR_SESSION_ID"));
        detected_model = nonempty_env(env, "CURSOR_MODEL");
    } else if has_any_prefix(env, "CODEX_") {
        detected_client = Some("codex-cli");
        detected_session =
            nonempty_env(env, "CODEX_THREAD_ID").or_else(|| nonempty_env(env, "CODEX_SESSION_ID"));
        detected_model =
            nonempty_env(env, "CODEX_MODEL").or_else(|| nonempty_env(env, "OPENAI_MODEL"));
    } else if has_any_prefix(env, "WINDSURF_") {
        detected_client = Some("windsurf");
        detected_session = nonempty_env(env, "WINDSURF_SESSION_ID");
        detected_model = nonempty_env(env, "WINDSURF_MODEL");
    } else if has_any_prefix(env, "ANTIGRAVITY_") || nonempty_env(env, "AGY_SESSION_ID").is_some() {
        detected_client = Some("antigravity");
        detected_session = nonempty_env(env, "ANTIGRAVITY_SESSION_ID")
            .or_else(|| nonempty_env(env, "ANTIGRAVITY_CONVERSATION_ID"))
            .or_else(|| nonempty_env(env, "ANTIGRAVITY_TRAJECTORY_ID"))
            .or_else(|| nonempty_env(env, "AGY_SESSION_ID"));
        detected_model = nonempty_env(env, "ANTIGRAVITY_MODEL");
    } else if is_truthy_flag(env, "OZ_AGENT") {
        detected_client = Some("oz-agent");
    }

    let is_agent = explicit_session.is_some()
        || explicit_client.is_some()
        || explicit_model.is_some()
        || explicit_prompt.is_some()
        || detected_client.is_some();

    if !is_agent {
        return None;
    }

    let client = explicit_client
        .or_else(|| detected_client.map(str::to_string))
        .unwrap_or_else(|| "oz-agent".to_string());
    let session_id = explicit_session
        .or(detected_session)
        .unwrap_or_else(|| format!("oz-pid-{}", std::process::id()));
    let model = explicit_model.or(detected_model);

    Some(DetectedAgentSession {
        session_id,
        client,
        model,
        prompt_summary: explicit_prompt,
    })
}

/// Writes an [`AiProvenanceNote`] JSON blob for `commit_or_rev` onto `refs/notes/ai` using
/// an atomic `gix` CAS ref transaction, preserving all existing notes (flat or fanout) and
/// advancing `refs/notes/ai` from its prior commit tip.
///
/// Returns the hex OID of the commit now at `refs/notes/ai`.
pub fn write_provenance_note(
    repo: &OzoneRepo,
    commit_or_rev: &str,
    note: &AiProvenanceNote,
) -> Result<String> {
    let gix_repo = open_gix_with_fallback(repo)?;
    let target_oid = resolve_commit_oid(&gix_repo, commit_or_rev)?;
    let target_hex = target_oid.to_hex().to_string();

    let mut payload = serde_json::to_string_pretty(note)?;
    payload.push('\n');
    let note_blob_id = gix_repo
        .write_blob(payload.as_bytes())
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();

    let (author_sig, committer_sig) = resolve_note_signatures(&gix_repo)?;
    let notes_ref_name = FullName::try_from(AI_NOTES_REF)
        .map_err(|err| OzError::Git(format!("invalid notes ref '{AI_NOTES_REF}': {err}")))?;

    for attempt in 0..MAX_CAS_RETRIES {
        let current_tip = read_notes_tip(&gix_repo)?;
        let (prev_commit_oid, base_tree_id) = match current_tip {
            Some((cid, tid)) => (Some(cid), tid),
            None => (None, ObjectId::empty_tree(gix_repo.object_hash())),
        };

        let existing_index = index_notes_tree(&gix_repo, base_tree_id)?;
        let note_rel_path = if let Some(entry) = existing_index.by_oid.get(&target_hex) {
            if entry.blob_id == note_blob_id
                && let Some(prev_cid) = prev_commit_oid
            {
                return Ok(prev_cid.to_hex().to_string());
            }
            entry.tree_path.clone()
        } else if existing_index.uses_fanout && target_hex.len() > 2 {
            format!("{}/{}", &target_hex[..2], &target_hex[2..])
        } else {
            target_hex.clone()
        };

        let mut editor = gix_repo
            .edit_tree(base_tree_id)
            .map_err(|err| OzError::Git(err.to_string()))?;
        editor
            .upsert(note_rel_path.as_str(), EntryKind::Blob, note_blob_id)
            .map_err(|err| OzError::Git(err.to_string()))?;
        let new_notes_tree_id = editor
            .write()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach();

        let mut author_buf = gix::date::parse::TimeBuf::default();
        let mut committer_buf = gix::date::parse::TimeBuf::default();
        let author_ref = author_sig.to_ref(&mut author_buf);
        let committer_ref = committer_sig.to_ref(&mut committer_buf);

        let parents: Vec<ObjectId> = prev_commit_oid.into_iter().collect();
        let notes_commit = gix_repo
            .new_commit_as(
                committer_ref,
                author_ref,
                "Notes added by 'oz'\n",
                new_notes_tree_id,
                parents,
            )
            .map_err(|err| OzError::Git(err.to_string()))?;
        let new_notes_commit_id = notes_commit.id;

        let expected = match prev_commit_oid {
            Some(prev) => PreviousValue::MustExistAndMatch(Target::Object(prev)),
            None => PreviousValue::MustNotExist,
        };
        let edit = RefEdit::update(
            notes_ref_name.clone(),
            new_notes_commit_id,
            expected,
            "oz notes: record ai provenance",
        );

        match gix_repo.edit_references([edit]) {
            Ok(_) => return Ok(new_notes_commit_id.to_hex().to_string()),
            Err(err) => {
                if attempt + 1 == MAX_CAS_RETRIES {
                    return Err(OzError::Git(format!(
                        "failed to update '{AI_NOTES_REF}' after {MAX_CAS_RETRIES} CAS attempts: {err}"
                    )));
                }
            }
        }
    }

    Err(OzError::Git(format!(
        "exhausted CAS retries updating '{AI_NOTES_REF}'"
    )))
}

/// Reads and parses the [`AiProvenanceNote`] attached to `commit_or_rev` under `refs/notes/ai`,
/// returning `Ok(None)` if `refs/notes/ai` does not exist or has no note for that commit.
pub fn read_provenance_note(
    repo: &OzoneRepo,
    commit_or_rev: &str,
) -> Result<Option<AiProvenanceNote>> {
    let gix_repo = open_gix_with_fallback(repo)?;
    let target_oid = resolve_commit_oid(&gix_repo, commit_or_rev)?;
    let target_hex = target_oid.to_hex().to_string();

    let Some((_, notes_tree_id)) = read_notes_tip(&gix_repo)? else {
        return Ok(None);
    };

    let Some(blob_id) = find_note_blob_in_tree(&gix_repo, notes_tree_id, &target_hex)? else {
        return Ok(None);
    };

    let blob = gix_repo
        .find_blob(blob_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let note: AiProvenanceNote = serde_json::from_slice(&blob.data)?;
    Ok(Some(note))
}

/// Loads all valid [`AiProvenanceNote`] entries stored in `refs/notes/ai` keyed by 40-hex commit OID.
///
/// Non-JSON notes (if a user manually attached plain text to `refs/notes/ai`) are gracefully skipped
/// so `oz blame` never crashes on custom notes.
pub fn list_provenance_notes(repo: &OzoneRepo) -> Result<BTreeMap<String, AiProvenanceNote>> {
    let gix_repo = open_gix_with_fallback(repo)?;
    load_provenance_notes_gix(&gix_repo)
}

pub(crate) fn load_provenance_notes_gix(
    gix_repo: &gix::Repository,
) -> Result<BTreeMap<String, AiProvenanceNote>> {
    let Some((_, notes_tree_id)) = read_notes_tip(gix_repo)? else {
        return Ok(BTreeMap::new());
    };
    let index = index_notes_tree(gix_repo, notes_tree_id)?;
    let mut out = BTreeMap::new();
    for (commit_hex, entry) in index.by_oid {
        let blob = gix_repo
            .find_blob(entry.blob_id)
            .map_err(|err| OzError::Git(err.to_string()))?;
        if let Ok(note) = serde_json::from_slice::<AiProvenanceNote>(&blob.data) {
            out.insert(commit_hex, note);
        }
    }
    Ok(out)
}

/// Computes exact 1-indexed added/modified new-file [`LineRangeAttribution`]s introduced by
/// `commit_or_rev` relative to its first parent (or the empty tree for root commits) via
/// `gix` + `imara-diff` (`Algorithm::Histogram`).
pub fn compute_commit_line_attributions(
    repo: &OzoneRepo,
    commit_or_rev: &str,
) -> Result<Vec<LineRangeAttribution>> {
    let gix_repo = open_gix_with_fallback(repo)?;
    let commit_id = resolve_commit_oid(&gix_repo, commit_or_rev)?;
    compute_commit_line_attributions_gix(&gix_repo, commit_id, "agent")
}

pub(crate) fn compute_commit_line_attributions_gix(
    gix_repo: &gix::Repository,
    commit_id: ObjectId,
    author_kind: &str,
) -> Result<Vec<LineRangeAttribution>> {
    let commit = gix_repo
        .find_commit(commit_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let new_tree_id = commit
        .tree_id()
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();
    let parent_tree_id = match commit.parent_ids().next() {
        Some(parent_id) => {
            let parent_commit = gix_repo
                .find_commit(parent_id.detach())
                .map_err(|err| OzError::Git(err.to_string()))?;
            parent_commit
                .tree_id()
                .map_err(|err| OzError::Git(err.to_string()))?
                .detach()
        }
        None => ObjectId::empty_tree(gix_repo.object_hash()),
    };

    let mut changed_blobs = Vec::new();
    diff_tree_changed_blobs(
        gix_repo,
        parent_tree_id,
        new_tree_id,
        "",
        &mut changed_blobs,
    )?;
    changed_blobs.sort_by(|a, b| a.path.cmp(&b.path));

    let mut attributions = Vec::new();
    for item in changed_blobs {
        let new_blob = gix_repo
            .find_blob(item.new_blob_id)
            .map_err(|err| OzError::Git(err.to_string()))?;
        let new_bytes = &new_blob.data;
        if new_bytes.is_empty() || is_binary_bytes(new_bytes) {
            continue;
        }

        match item.old_blob_id {
            None => {
                let line_count = sources::byte_lines(new_bytes).count() as u32;
                if line_count > 0 {
                    attributions.push(LineRangeAttribution {
                        path: item.path,
                        start_line: 1,
                        end_line: line_count,
                        author_kind: author_kind.to_string(),
                    });
                }
            }
            Some(old_id) => {
                let old_blob = gix_repo
                    .find_blob(old_id)
                    .map_err(|err| OzError::Git(err.to_string()))?;
                let old_bytes = &old_blob.data;
                if is_binary_bytes(old_bytes) {
                    continue;
                }

                let input = InternedInput::new(old_bytes.as_slice(), new_bytes.as_slice());
                let mut diff = Diff::compute(Algorithm::Histogram, &input);
                diff.postprocess_lines(&input);

                let mut file_ranges: Vec<(u32, u32)> = Vec::new();
                for hunk in diff.hunks() {
                    if hunk.after.start < hunk.after.end {
                        let start_1 = hunk.after.start + 1;
                        let end_1 = hunk.after.end;
                        if let Some(last) = file_ranges.last_mut()
                            && last.1 + 1 >= start_1
                        {
                            last.1 = last.1.max(end_1);
                        } else {
                            file_ranges.push((start_1, end_1));
                        }
                    }
                }

                for (start_line, end_line) in file_ranges {
                    attributions.push(LineRangeAttribution {
                        path: item.path.clone(),
                        start_line,
                        end_line,
                        author_kind: author_kind.to_string(),
                    });
                }
            }
        }
    }

    Ok(attributions)
}

/// Automatically detects an active coding agent session (via `opts` and/or environment markers),
/// computes exact added/modified line ranges for `commit_or_rev`, writes an [`AiProvenanceNote`]
/// to `refs/notes/ai` without altering the human commit message, and returns `Ok(Some(note))`.
///
/// Returns `Ok(None)` when no agent session is active (e.g., a normal human commit).
pub fn record_commit_provenance(
    repo: &OzoneRepo,
    commit_or_rev: &str,
    opts: &ProvenanceOptions,
) -> Result<Option<AiProvenanceNote>> {
    let env_session = if let Some(ref map) = opts.env_overrides {
        detect_agent_session_from_map(map)
    } else if opts.inspect_env {
        detect_agent_session()
    } else {
        None
    };

    let has_explicit_agent = opts.force_agent
        || opts.session_id.is_some()
        || opts.client.is_some()
        || opts.model.is_some()
        || opts.prompt_summary.is_some();

    if !has_explicit_agent && env_session.is_none() {
        return Ok(None);
    }

    let gix_repo = open_gix_with_fallback(repo)?;
    let commit_id = resolve_commit_oid(&gix_repo, commit_or_rev)?;
    let commit_hex = commit_id.to_hex().to_string();

    let session_id = opts
        .session_id
        .clone()
        .or_else(|| env_session.as_ref().map(|s| s.session_id.clone()))
        .unwrap_or_else(|| format!("oz-{}", &commit_hex[..commit_hex.len().min(12)]));

    let client = opts
        .client
        .clone()
        .or_else(|| env_session.as_ref().map(|s| s.client.clone()))
        .unwrap_or_else(|| "oz-agent".to_string());

    let model = opts
        .model
        .clone()
        .or_else(|| env_session.as_ref().and_then(|s| s.model.clone()));

    let prompt_summary = opts
        .prompt_summary
        .clone()
        .or_else(|| env_session.as_ref().and_then(|s| s.prompt_summary.clone()));

    let line_ranges = compute_commit_line_attributions_gix(&gix_repo, commit_id, "agent")?;

    let note = AiProvenanceNote {
        schema_version: AI_PROVENANCE_SCHEMA_VERSION,
        session_id,
        client,
        model,
        prompt_summary,
        timestamp: Utc::now(),
        line_ranges,
    };

    write_provenance_note(repo, &commit_hex, &note)?;
    Ok(Some(note))
}

#[derive(Debug, Clone)]
struct NotesEntry {
    tree_path: String,
    blob_id: ObjectId,
}

#[derive(Debug, Default)]
struct NotesTreeIndex {
    uses_fanout: bool,
    by_oid: BTreeMap<String, NotesEntry>,
}

#[derive(Debug, Clone)]
struct ChangedBlob {
    path: String,
    old_blob_id: Option<ObjectId>,
    new_blob_id: ObjectId,
}

pub(crate) fn open_gix_with_fallback(repo: &OzoneRepo) -> Result<gix::Repository> {
    let target = if repo.is_linked_worktree && repo.git_dir.join("HEAD").is_file() {
        &repo.git_dir
    } else {
        &repo.repo_root
    };
    let mut gix_repo = gix::open(target).map_err(|err| OzError::Git(err.to_string()))?;
    gix_repo
        .committer_or_set_fallback("Ozone", "oz@ozone.dev")
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(gix_repo)
}

pub(crate) fn resolve_commit_oid(
    gix_repo: &gix::Repository,
    commit_or_rev: &str,
) -> Result<ObjectId> {
    let trimmed = commit_or_rev.trim();
    if trimmed.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "commit OID or revision cannot be empty".to_string(),
            recovery: Vec::new(),
        });
    }

    if let Ok(oid) = ObjectId::from_hex(trimmed.as_bytes())
        && gix_repo.find_commit(oid).is_ok()
    {
        return Ok(oid);
    }

    let spec = gix_repo
        .rev_parse_single(trimmed)
        .map_err(|err| OzError::Git(format!("unable to resolve revision '{trimmed}': {err}")))?;
    let object = spec.object().map_err(|err| OzError::Git(err.to_string()))?;
    let commit = object
        .peel_to_commit()
        .map_err(|err| OzError::Git(format!("revision '{trimmed}' is not a commit: {err}")))?;
    Ok(commit.id)
}

fn read_notes_tip(gix_repo: &gix::Repository) -> Result<Option<(ObjectId, ObjectId)>> {
    let Some(mut reference) = gix_repo
        .try_find_reference(AI_NOTES_REF)
        .map_err(|err| OzError::Git(err.to_string()))?
    else {
        return Ok(None);
    };

    let commit = reference
        .peel_to_commit()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let tree_id = commit
        .tree_id()
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();
    Ok(Some((commit.id, tree_id)))
}

fn resolve_note_signatures(
    gix_repo: &gix::Repository,
) -> Result<(gix::actor::Signature, gix::actor::Signature)> {
    let committer = match gix_repo.committer() {
        Some(res) => res
            .map_err(|err| OzError::Git(err.to_string()))?
            .to_owned()
            .map_err(|err| OzError::Git(err.to_string()))?,
        None => gix::actor::Signature {
            name: "Ozone".into(),
            email: "oz@ozone.dev".into(),
            time: gix::date::Time::now_local_or_utc(),
        },
    };
    let author = match gix_repo.author() {
        Some(res) => res
            .map_err(|err| OzError::Git(err.to_string()))?
            .to_owned()
            .map_err(|err| OzError::Git(err.to_string()))?,
        None => committer.clone(),
    };
    Ok((author, committer))
}

fn find_note_blob_in_tree(
    gix_repo: &gix::Repository,
    notes_tree_id: ObjectId,
    target_hex: &str,
) -> Result<Option<ObjectId>> {
    // Fast direct lookup for flat layout (`<40-hex>`) and 2-hex fanout (`<2-hex>/<38-hex>`).
    let root_tree = gix_repo
        .find_tree(notes_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;

    for entry_res in root_tree.iter() {
        let entry = entry_res.map_err(|err| OzError::Git(err.to_string()))?;
        if entry.mode().is_blob() && entry.filename() == target_hex.as_bytes() {
            return Ok(Some(entry.object_id()));
        }
        if target_hex.len() > 2
            && entry.mode().is_tree()
            && entry.filename() == &target_hex.as_bytes()[..2]
        {
            let sub_tree = gix_repo
                .find_tree(entry.object_id())
                .map_err(|err| OzError::Git(err.to_string()))?;
            let rest = &target_hex.as_bytes()[2..];
            for sub_res in sub_tree.iter() {
                let sub = sub_res.map_err(|err| OzError::Git(err.to_string()))?;
                if sub.mode().is_blob() && sub.filename() == rest {
                    return Ok(Some(sub.object_id()));
                }
            }
        }
    }

    // Fallback for deeper multi-level fanout layouts (e.g. `ab/cd/ef...`).
    let index = index_notes_tree(gix_repo, notes_tree_id)?;
    Ok(index.by_oid.get(target_hex).map(|e| e.blob_id))
}

fn index_notes_tree(gix_repo: &gix::Repository, notes_tree_id: ObjectId) -> Result<NotesTreeIndex> {
    let expected_hex_len = gix_repo.object_hash().len_in_hex();
    let mut index = NotesTreeIndex::default();
    walk_notes_tree_recursive(
        gix_repo,
        notes_tree_id,
        "",
        "",
        0,
        expected_hex_len,
        &mut index,
    )?;
    Ok(index)
}

fn walk_notes_tree_recursive(
    gix_repo: &gix::Repository,
    tree_id: ObjectId,
    rel_prefix: &str,
    hex_prefix: &str,
    depth: usize,
    expected_hex_len: usize,
    out: &mut NotesTreeIndex,
) -> Result<()> {
    let tree = gix_repo
        .find_tree(tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;

    for entry_res in tree.iter() {
        let entry = entry_res.map_err(|err| OzError::Git(err.to_string()))?;
        let Some(seg) = entry.filename().to_str().ok() else {
            continue;
        };
        if !seg.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }

        let tree_path = if rel_prefix.is_empty() {
            seg.to_string()
        } else {
            format!("{rel_prefix}/{seg}")
        };
        let mut combined_hex = String::with_capacity(hex_prefix.len() + seg.len());
        combined_hex.push_str(hex_prefix);
        combined_hex.push_str(&seg.to_ascii_lowercase());

        if entry.mode().is_tree() {
            if depth == 0 && seg.len() == 2 {
                out.uses_fanout = true;
            }
            if combined_hex.len() < expected_hex_len {
                walk_notes_tree_recursive(
                    gix_repo,
                    entry.object_id(),
                    &tree_path,
                    &combined_hex,
                    depth + 1,
                    expected_hex_len,
                    out,
                )?;
            }
        } else if entry.mode().is_blob() && combined_hex.len() == expected_hex_len {
            out.by_oid.insert(
                combined_hex,
                NotesEntry {
                    tree_path,
                    blob_id: entry.object_id(),
                },
            );
        }
    }

    Ok(())
}

fn diff_tree_changed_blobs(
    gix_repo: &gix::Repository,
    old_tree_id: ObjectId,
    new_tree_id: ObjectId,
    prefix: &str,
    out: &mut Vec<ChangedBlob>,
) -> Result<()> {
    if old_tree_id == new_tree_id {
        return Ok(());
    }

    let empty_id = ObjectId::empty_tree(gix_repo.object_hash());

    let mut old_entries: BTreeMap<String, (gix::objs::tree::EntryMode, ObjectId)> = BTreeMap::new();
    if old_tree_id != empty_id {
        let old_tree = gix_repo
            .find_tree(old_tree_id)
            .map_err(|err| OzError::Git(err.to_string()))?;
        for res in old_tree.iter() {
            let e = res.map_err(|err| OzError::Git(err.to_string()))?;
            let name = String::from_utf8_lossy(e.filename().as_bytes()).into_owned();
            old_entries.insert(name, (e.mode(), e.object_id()));
        }
    }

    if new_tree_id == empty_id {
        return Ok(());
    }

    let new_tree = gix_repo
        .find_tree(new_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;

    for res in new_tree.iter() {
        let new_e = res.map_err(|err| OzError::Git(err.to_string()))?;
        let name = String::from_utf8_lossy(new_e.filename().as_bytes()).into_owned();
        let full_path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let new_mode = new_e.mode();
        let new_oid = new_e.object_id();

        match old_entries.get(&name).copied() {
            Some((old_mode, old_oid)) if old_mode == new_mode && old_oid == new_oid => {
                // Identical blob or subtree — skip!
            }
            Some((old_mode, old_oid)) if old_mode.is_tree() && new_mode.is_tree() => {
                diff_tree_changed_blobs(gix_repo, old_oid, new_oid, &full_path, out)?;
            }
            Some((old_mode, old_oid)) => {
                if new_mode.is_tree() {
                    diff_tree_changed_blobs(gix_repo, empty_id, new_oid, &full_path, out)?;
                } else if is_regular_file_mode(new_mode) {
                    let prev_blob = is_regular_file_mode(old_mode).then_some(old_oid);
                    out.push(ChangedBlob {
                        path: full_path,
                        old_blob_id: prev_blob,
                        new_blob_id: new_oid,
                    });
                }
            }
            None => {
                if new_mode.is_tree() {
                    diff_tree_changed_blobs(gix_repo, empty_id, new_oid, &full_path, out)?;
                } else if is_regular_file_mode(new_mode) {
                    out.push(ChangedBlob {
                        path: full_path,
                        old_blob_id: None,
                        new_blob_id: new_oid,
                    });
                }
            }
        }
    }

    Ok(())
}

fn is_regular_file_mode(mode: gix::objs::tree::EntryMode) -> bool {
    matches!(mode.kind(), EntryKind::Blob | EntryKind::BlobExecutable)
}

fn nonempty_env(env: &BTreeMap<String, String>, key: &str) -> Option<String> {
    env.get(key)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn is_truthy_flag(env: &BTreeMap<String, String>, key: &str) -> bool {
    env.get(key).is_some_and(|v| {
        let lower = v.trim().to_ascii_lowercase();
        matches!(lower.as_str(), "1" | "true" | "yes" | "on")
    })
}

fn is_truthy_or_set(env: &BTreeMap<String, String>, key: &str) -> bool {
    env.get(key).is_some_and(|v| {
        let lower = v.trim().to_ascii_lowercase();
        !lower.is_empty() && lower != "0" && lower != "false" && lower != "no"
    })
}

fn has_any_prefix(env: &BTreeMap<String, String>, prefix: &str) -> bool {
    env.iter().any(|(k, v)| {
        k.starts_with(prefix) && {
            let lower = v.trim().to_ascii_lowercase();
            !lower.is_empty() && lower != "0" && lower != "false" && lower != "no"
        }
    })
}

fn first_prefixed_id(env: &BTreeMap<String, String>, prefix: &str) -> Option<String> {
    for (k, v) in env {
        if k.starts_with(prefix) && (k.ends_with("_ID") || k.ends_with("_SESSION")) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use oz_core::{CommitOptions, commit_changes};
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn init_test_repo() -> (TempDir, OzoneRepo) {
        let dir = TempDir::new().expect("tempdir");
        let status = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        assert!(status.success());

        for (k, v) in [
            ("user.name", "Alice Human"),
            ("user.email", "alice@example.com"),
        ] {
            let s = Command::new("git")
                .args(["config", k, v])
                .current_dir(dir.path())
                .status()
                .expect("git config");
            assert!(s.success());
        }

        let repo = OzoneRepo::discover(Some(dir.path())).expect("discover repo");
        (dir, repo)
    }

    #[test]
    fn notes_interoperate_with_system_git_notes_and_preserve_clean_messages() {
        let (dir, repo) = init_test_repo();

        fs::write(
            dir.path().join("lib.rs"),
            "pub fn a() -> i32 {\n    1\n}\n\npub fn keep() -> &'static str {\n    \"stable\"\n}\n",
        )
        .expect("write lib.rs");
        let c1 = commit_changes(&repo, &CommitOptions::new("feat: initial function"))
            .expect("commit c1");

        // Record provenance on c1 via explicit options.
        let mut opts = ProvenanceOptions::explicit("claude-code", "sess-001");
        opts.model = Some("claude-sonnet-4-5".to_string());
        opts.prompt_summary = Some("add initial function a()".to_string());
        let note1 = record_commit_provenance(&repo, &c1.commit_oid, &opts)
            .expect("record c1 provenance")
            .expect("some note");
        assert_eq!(note1.client, "claude-code");
        assert_eq!(note1.session_id, "sess-001");
        assert_eq!(
            note1.line_ranges,
            vec![LineRangeAttribution {
                path: "lib.rs".to_string(),
                start_line: 1,
                end_line: 7,
                author_kind: "agent".to_string(),
            }]
        );

        // Verify commit message itself remained 100% clean (only subject + Change-Id trailer).
        let log_out = Command::new("/usr/bin/git")
            .args(["log", "-1", "--format=%B", &c1.commit_oid])
            .current_dir(dir.path())
            .output()
            .expect("git log");
        assert!(log_out.status.success());
        let commit_body = String::from_utf8_lossy(&log_out.stdout);
        assert!(commit_body.starts_with("feat: initial function\n"));
        assert!(!commit_body.contains("sess-001"));
        assert!(!commit_body.contains("claude-code"));

        // Verify `/usr/bin/git notes --ref=ai show <oid>` reads the note written by oz-agent!
        let git_note_out = Command::new("/usr/bin/git")
            .args(["notes", "--ref=ai", "show", &c1.commit_oid])
            .current_dir(dir.path())
            .output()
            .expect("git notes show");
        assert!(
            git_note_out.status.success(),
            "git notes failed: {}",
            String::from_utf8_lossy(&git_note_out.stderr)
        );
        let parsed_from_git: AiProvenanceNote =
            serde_json::from_slice(&git_note_out.stdout).expect("parse git notes JSON");
        assert_eq!(parsed_from_git, note1);

        // Second commit modifying line 2 and adding a new function below `keep()` (two disjoint ranges).
        let repo = OzoneRepo::discover(Some(dir.path())).expect("rediscover");
        fs::write(
            dir.path().join("lib.rs"),
            "pub fn a() -> i32 {\n    42\n}\n\npub fn keep() -> &'static str {\n    \"stable\"\n}\n\npub fn b() -> i32 {\n    99\n}\n",
        )
        .expect("update lib.rs");
        let c2 = commit_changes(&repo, &CommitOptions::new("feat: update a and add b"))
            .expect("commit c2");

        let opts2 = ProvenanceOptions::explicit("cursor", "trace-xyz");
        let note2 = record_commit_provenance(&repo, &c2.commit_oid, &opts2)
            .expect("record c2 provenance")
            .expect("some note2");
        assert_eq!(note2.line_ranges.len(), 2);
        assert_eq!(
            note2.line_ranges[0],
            LineRangeAttribution {
                path: "lib.rs".to_string(),
                start_line: 2,
                end_line: 2,
                author_kind: "agent".to_string(),
            }
        );
        assert_eq!(note2.line_ranges[1].path, "lib.rs");
        assert_eq!(note2.line_ranges[1].author_kind, "agent");
        assert_eq!(
            note2.line_ranges[1].end_line - note2.line_ranges[1].start_line + 1,
            4
        );

        // Overwrite existing note on c1 and verify BOTH c1 and c2 remain readable via oz-agent AND C git!
        let mut updated_note1 = note1.clone();
        updated_note1.prompt_summary = Some("updated summary".to_string());
        write_provenance_note(&repo, &c1.commit_oid, &updated_note1).expect("overwrite c1 note");

        let read_c1 = read_provenance_note(&repo, &c1.commit_oid)
            .expect("read c1")
            .expect("c1 exists");
        let read_c2 = read_provenance_note(&repo, &c2.commit_oid)
            .expect("read c2")
            .expect("c2 exists");
        assert_eq!(read_c1.prompt_summary.as_deref(), Some("updated summary"));
        assert_eq!(read_c2.client, "cursor");

        let all_notes = list_provenance_notes(&repo).expect("list all notes");
        assert_eq!(all_notes.len(), 2);

        let git_show_c2 = Command::new("/usr/bin/git")
            .args(["notes", "--ref=ai", "show", &c2.commit_oid])
            .current_dir(dir.path())
            .output()
            .expect("git notes show c2");
        assert!(git_show_c2.status.success());
    }

    #[test]
    fn reads_fanout_notes_trees_and_handles_concurrent_cas_updates() {
        let (dir, repo) = init_test_repo();

        let mut commit_oids = Vec::new();
        for i in 0..6 {
            let current_repo = OzoneRepo::discover(Some(dir.path())).expect("discover");
            fs::write(dir.path().join(format!("f{i}.txt")), format!("line {i}\n"))
                .expect("write file");
            let outcome = commit_changes(
                &current_repo,
                &CommitOptions::new(format!("feat: add f{i}")),
            )
            .expect("commit");
            commit_oids.push(outcome.commit_oid);
        }

        // Seed a 2-hex fanout entry (`ab/cdef...`) directly in `refs/notes/ai` to verify fanout support.
        let first_oid = &commit_oids[0];
        let seed_note = AiProvenanceNote {
            schema_version: 1,
            session_id: "fanout-seed".to_string(),
            client: "codex-cli".to_string(),
            model: None,
            prompt_summary: None,
            timestamp: Utc::now(),
            line_ranges: Vec::new(),
        };
        let gix_repo = open_gix_with_fallback(&repo).expect("open gix");
        let blob_id = gix_repo
            .write_blob(serde_json::to_vec_pretty(&seed_note).unwrap())
            .unwrap()
            .detach();
        let mut editor = gix_repo
            .edit_tree(ObjectId::empty_tree(gix_repo.object_hash()))
            .unwrap();
        let fanout_path = format!("{}/{}", &first_oid[..2], &first_oid[2..]);
        editor
            .upsert(fanout_path.as_str(), EntryKind::Blob, blob_id)
            .unwrap();
        let fanout_tree_id = editor.write().unwrap().detach();
        let (author, committer) = resolve_note_signatures(&gix_repo).unwrap();
        let mut abuf = gix::date::parse::TimeBuf::default();
        let mut cbuf = gix::date::parse::TimeBuf::default();
        let notes_commit = gix_repo
            .new_commit_as(
                committer.to_ref(&mut cbuf),
                author.to_ref(&mut abuf),
                "Seed fanout notes\n",
                fanout_tree_id,
                std::iter::empty::<ObjectId>(),
            )
            .unwrap();
        gix_repo
            .edit_references([RefEdit::update(
                FullName::try_from(AI_NOTES_REF).unwrap(),
                notes_commit.id,
                PreviousValue::MustNotExist,
                "seed fanout",
            )])
            .unwrap();

        // Verify oz-agent reads the fanout note cleanly.
        let loaded_seed = read_provenance_note(&repo, first_oid)
            .expect("read fanout note")
            .expect("seed exists");
        assert_eq!(loaded_seed.session_id, "fanout-seed");

        // Concurrent writers updating remaining 5 commits simultaneously!
        std::thread::scope(|s| {
            for (idx, oid) in commit_oids.iter().enumerate().skip(1) {
                let repo_clone = repo.clone();
                let oid_clone = oid.clone();
                s.spawn(move || {
                    let note = AiProvenanceNote {
                        schema_version: 1,
                        session_id: format!("concurrent-{idx}"),
                        client: "antigravity".to_string(),
                        model: Some("gemini".to_string()),
                        prompt_summary: None,
                        timestamp: Utc::now(),
                        line_ranges: Vec::new(),
                    };
                    write_provenance_note(&repo_clone, &oid_clone, &note)
                        .expect("concurrent write_provenance_note");
                });
            }
        });

        let all = list_provenance_notes(&repo).expect("list all after concurrent CAS");
        assert_eq!(all.len(), 6);
        for oid in &commit_oids {
            let out = Command::new("/usr/bin/git")
                .args(["notes", "--ref=ai", "show", oid])
                .current_dir(dir.path())
                .output()
                .expect("git notes show fanout");
            assert!(
                out.status.success(),
                "git notes failed on {oid}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn detects_agent_sessions_across_env_conventions_and_skips_clean_human_env() {
        let empty = BTreeMap::new();
        assert_eq!(detect_agent_session_from_map(&empty), None);

        let mut cursor_env = BTreeMap::new();
        cursor_env.insert("CURSOR_TRACE_ID".to_string(), "cur-777".to_string());
        let det = detect_agent_session_from_map(&cursor_env).expect("cursor session");
        assert_eq!(det.client, "cursor");
        assert_eq!(det.session_id, "cur-777");

        let mut oz_env = BTreeMap::new();
        oz_env.insert("CLAUDECODE".to_string(), "1".to_string());
        oz_env.insert("OZ_AGENT_SESSION".to_string(), "override-sess".to_string());
        oz_env.insert("OZ_AGENT_MODEL".to_string(), "claude-opus-4-6".to_string());
        oz_env.insert("OZ_AGENT_PROMPT".to_string(), "fix auth race".to_string());
        let det2 = detect_agent_session_from_map(&oz_env).expect("oz override session");
        assert_eq!(det2.client, "claude-code");
        assert_eq!(det2.session_id, "override-sess");
        assert_eq!(det2.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(det2.prompt_summary.as_deref(), Some("fix auth race"));
    }
}
