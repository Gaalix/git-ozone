// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Daily human developer operations in pure Rust (`gix` + `imara-diff` + `SSHSIG`):
//! - `log_commits` (`oz log`: compact commit graph, `--oneline`, `-n`, `--author`, `--grep`, `-S`, `-G`, `--since`)
//! - `show_commit` (`oz show`: commit metadata, `Change-Id`, signature status, `--stat`, unified diff)
//! - `stash_*` (`oz stash`: `push`, `pop`, `list`, `drop` stored as reachable Git objects under `refs/ozone/stash`)
//! - `cherry_pick_commit` (`oz cherry-pick`) and `revert_commit` (`oz revert`)
//! - `tag_*` (`oz tag`: create, list, delete, and SSH-signed tags via `ssh_sign`)
//! - `branch_*` (`oz branch`: list, create, rename, delete)

use crate::commit::{ensure_change_id_trailer, verify_commit_signature};
use crate::diff::{
    DiffReport, compute_file_diff, file_id, filter_changes_by_targets_and_excludes,
    is_binary_bytes, render_file_unified_diff,
};
use crate::error::{OzError, Result};
use crate::repo::OzoneRepo;
use crate::ssh_sign::{
    GIT_SSHSIG_NAMESPACE, expand_tilde_path, match_allowed_signers_file,
    resolve_signing_private_key, verify_sshsig,
};
use crate::status::{
    compute_status_context, open_gix_for_workspace, read_blob_bytes, resolve_workspace_head,
};
use crate::types::{ChangeKind, FileChange, WorkspaceState};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use gix::bstr::ByteSlice;
use gix::diff::blob::{Algorithm, Diff, InternedInput, sources};
use gix::hash::ObjectId;
use gix::objs::WriteTo as _;
use gix::objs::tree::EntryKind;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::{FullName, Target};
use gix::revision::walk::Sorting;
use gix::traverse::commit::simple::CommitTimeOrder;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

/// Git reference storing the atomic Ozone stash stack commit chain.
pub const OZ_STASH_REF: &str = "refs/ozone/stash";

const STASH_SCHEMA_VERSION: u32 = 1;

// ============================================================================
// Shared Git / Commit / Signature / Regex / Date Helpers
// ============================================================================

fn parse_oid(hex: &str) -> Result<ObjectId> {
    ObjectId::from_hex(hex.as_bytes())
        .map_err(|err| OzError::Git(format!("invalid object id '{hex}': {err}")))
}

fn commit_tree_id(gix_repo: &gix::Repository, commit_id: ObjectId) -> Result<ObjectId> {
    let commit = gix_repo
        .find_commit(commit_id)
        .map_err(OzError::git_source)?;
    Ok(commit.tree_id().map_err(OzError::git_source)?.detach())
}

fn rev_parse_error(spec: &str, err: impl std::fmt::Display) -> OzError {
    let trimmed = spec.trim();
    let looks_like_hex_sha =
        (7..=40).contains(&trimmed.len()) && trimmed.bytes().all(|b| b.is_ascii_hexdigit());
    let mut recovery = Vec::new();
    if looks_like_hex_sha {
        recovery.push("oz fetch".to_string());
    }
    recovery.push("Run `oz log --oneline` or `oz branch -a` to inspect revisions.".to_string());
    OzError::InvalidUsage {
        message: format!("cannot resolve revision '{trimmed}': {err}"),
        recovery,
    }
}

fn resolve_commit_oid(gix_repo: &gix::Repository, spec: &str) -> Result<ObjectId> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "revision cannot be empty".to_string(),
            recovery: vec!["Pass a commit OID, branch name, or tag (e.g. `HEAD`).".to_string()],
        });
    }
    if let Ok(oid) = ObjectId::from_hex(trimmed.as_bytes())
        && let Ok(obj) = gix_repo.find_object(oid)
        && let Ok(commit) = obj.peel_to_commit()
    {
        return Ok(commit.id);
    }
    let id = gix_repo
        .rev_parse_single(trimmed)
        .map_err(|err| rev_parse_error(trimmed, err))?;
    let obj = id.object().map_err(OzError::git_source)?;
    let commit = obj.peel_to_commit().map_err(|err| OzError::InvalidUsage {
        message: format!("revision '{trimmed}' does not point to a commit: {err}"),
        recovery: Vec::new(),
    })?;
    Ok(commit.id)
}

fn gix_time_to_utc(time: gix::date::Time) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(time.seconds, 0).unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
}

fn split_subject_and_body(message: &str) -> (String, String) {
    let trimmed = message.trim_end_matches(['\r', '\n']);
    if let Some((first, rest)) = trimmed.split_once("\n\n") {
        (
            first.lines().map(str::trim).collect::<Vec<_>>().join(" "),
            rest.to_string(),
        )
    } else {
        let mut lines = trimmed.lines();
        let first = lines.next().unwrap_or("").trim().to_string();
        let rest: Vec<&str> = lines.collect();
        (first, rest.join("\n"))
    }
}

/// Extracts `Change-Id:` trailer from the trailing paragraph of a commit message.
#[must_use]
pub(crate) fn extract_change_id(message: &str) -> Option<String> {
    let base = message.trim_end_matches(['\r', '\n']);
    let last_para = base.rsplit("\n\n").next().unwrap_or(base);
    for line in last_para.lines() {
        let trimmed = line.trim();
        if let Some((key, val)) = trimmed.split_once(':')
            && key.eq_ignore_ascii_case("Change-Id")
        {
            let value = val.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn resolve_actor_signatures(
    gix_repo: &mut gix::Repository,
) -> Result<(gix::actor::Signature, gix::actor::Signature)> {
    let committer = match gix_repo.committer() {
        Some(res) => res
            .map_err(OzError::git_source)?
            .to_owned()
            .map_err(OzError::git_source)?,
        None => gix_repo
            .committer_or_set_fallback("Ozone", "oz@ozone.dev")
            .map_err(OzError::git_source)?
            .to_owned()
            .map_err(OzError::git_source)?,
    };
    let author = match gix_repo.author() {
        Some(res) => res
            .map_err(OzError::git_source)?
            .to_owned()
            .map_err(OzError::git_source)?,
        None => committer.clone(),
    };
    Ok((author, committer))
}

fn resolve_should_sign(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    explicit_sign: Option<bool>,
) -> bool {
    if let Some(sign) = explicit_sign {
        return sign;
    }
    if let Some(enabled) = repo.config.signing.enabled {
        return enabled;
    }
    let snap = gix_repo.config_snapshot();
    let gpgsign = snap.boolean("commit.gpgsign").unwrap_or(false);
    let gpg_format = snap.string("gpg.format");
    gpgsign
        && gpg_format
            .as_deref()
            .is_some_and(|f| f.to_str_lossy().trim().eq_ignore_ascii_case("ssh"))
}

fn resolve_signing_key_spec(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    explicit_key: Option<&str>,
) -> String {
    if let Some(k) = explicit_key.map(str::trim).filter(|k| !k.is_empty()) {
        return k.to_string();
    }
    if let Some(k) = repo
        .config
        .signing
        .key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        return k.to_string();
    }
    if let Some(k) = gix_repo
        .config_snapshot()
        .string("user.signingkey")
        .map(|v| v.to_str_lossy().trim().to_string())
        .filter(|k| !k.is_empty())
    {
        return k;
    }
    "~/.ssh/id_ed25519".to_string()
}

fn target_branch_or_head_ref(repo: &OzoneRepo, branch: Option<&str>) -> Result<String> {
    if repo.workspace.is_some() && !repo.is_linked_worktree {
        if let Some(br) = branch {
            Ok(format!("refs/heads/{br}"))
        } else {
            Err(OzError::InvalidUsage {
                message: "cannot advance detached HEAD in non-linked workspace without branch"
                    .to_string(),
                recovery: vec!["Check out or spawn a named workspace branch.".to_string()],
            })
        }
    } else {
        Ok("HEAD".to_string())
    }
}

fn find_worktree_using_branch(repo: &OzoneRepo, target_ref: &FullName) -> Result<Option<String>> {
    let target_str = target_ref.as_bstr().to_str_lossy();
    let self_git_dir = fs::canonicalize(&repo.git_dir).unwrap_or_else(|_| repo.git_dir.clone());

    let main_head = repo.git_common_dir.join("HEAD");
    let main_canon =
        fs::canonicalize(&repo.git_common_dir).unwrap_or_else(|_| repo.git_common_dir.clone());
    if main_canon != self_git_dir
        && let Ok(content) = fs::read_to_string(&main_head)
        && let Some(sym) = content.trim().strip_prefix("ref:")
        && sym.trim() == target_str.as_ref()
    {
        return Ok(Some("main".to_string()));
    }

    let wt_dir = repo.git_common_dir.join("worktrees");
    if wt_dir.is_dir() {
        for entry in fs::read_dir(&wt_dir)? {
            let entry = entry?;
            let path = entry.path();
            let canon = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if canon == self_git_dir {
                continue;
            }
            if let Ok(content) = fs::read_to_string(path.join("HEAD"))
                && let Some(sym) = content.trim().strip_prefix("ref:")
                && sym.trim() == target_str.as_ref()
            {
                let name = entry.file_name().to_string_lossy().into_owned();
                return Ok(Some(name));
            }
        }
    }
    Ok(None)
}

#[derive(Debug, Clone)]
enum TreeDeltaOp {
    Delete(String),
    Upsert {
        path: String,
        oid: ObjectId,
        kind: EntryKind,
    },
}

impl TreeDeltaOp {
    fn path(&self) -> &str {
        match self {
            Self::Delete(p) | Self::Upsert { path: p, .. } => p,
        }
    }

    fn into_path(self) -> String {
        match self {
            Self::Delete(p) | Self::Upsert { path: p, .. } => p,
        }
    }
}

fn compute_tree_delta(
    gix_repo: &gix::Repository,
    lhs_tree_id: ObjectId,
    rhs_tree_id: ObjectId,
) -> Result<Vec<TreeDeltaOp>> {
    if lhs_tree_id == rhs_tree_id {
        return Ok(Vec::new());
    }
    let lhs_tree = gix_repo
        .find_tree(lhs_tree_id)
        .map_err(OzError::git_source)?;
    let rhs_tree = gix_repo
        .find_tree(rhs_tree_id)
        .map_err(OzError::git_source)?;

    let mut state = gix::diff::tree::State::default();
    let mut recorder = gix::diff::tree::Recorder::default()
        .track_location(Some(gix::diff::tree::recorder::Location::Path));
    let lhs_iter = gix::objs::TreeRefIter::from_bytes(&lhs_tree.data, lhs_tree.id.kind());
    let rhs_iter = gix::objs::TreeRefIter::from_bytes(&rhs_tree.data, rhs_tree.id.kind());

    gix::diff::tree(
        lhs_iter,
        rhs_iter,
        &mut state,
        &gix_repo.objects,
        &mut recorder,
    )
    .map_err(OzError::git_source)?;

    let mut ops = Vec::new();
    for change in recorder.records {
        match change {
            gix::diff::tree::recorder::Change::Deletion {
                entry_mode, path, ..
            } => {
                if !entry_mode.is_tree() && entry_mode.kind() != EntryKind::Commit {
                    ops.push(TreeDeltaOp::Delete(path.to_str_lossy().into_owned()));
                }
            }
            gix::diff::tree::recorder::Change::Addition {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if !entry_mode.is_tree() && entry_mode.kind() != EntryKind::Commit {
                    ops.push(TreeDeltaOp::Upsert {
                        path: path.to_str_lossy().into_owned(),
                        oid,
                        kind: entry_mode.kind(),
                    });
                }
            }
            gix::diff::tree::recorder::Change::Modification {
                previous_entry_mode,
                entry_mode,
                oid,
                path,
                ..
            } => {
                let path_str = path.to_str_lossy().into_owned();
                if entry_mode.is_tree() {
                    if !previous_entry_mode.is_tree() {
                        ops.push(TreeDeltaOp::Delete(path_str));
                    }
                } else if entry_mode.kind() != EntryKind::Commit {
                    ops.push(TreeDeltaOp::Upsert {
                        path: path_str,
                        oid,
                        kind: entry_mode.kind(),
                    });
                }
            }
        }
    }
    Ok(ops)
}

fn paths_collide(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
}

fn prune_empty_parents(workdir: &Path, file_path: &Path) {
    let mut cur = file_path.parent();
    while let Some(dir) = cur {
        if dir == workdir || !dir.starts_with(workdir) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}

fn remove_conflicting_non_dir_ancestors(workdir: &Path, target_file: &Path) -> Result<()> {
    let Ok(rel) = target_file.strip_prefix(workdir) else {
        return Ok(());
    };
    let mut acc = workdir.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    if comps.len() <= 1 {
        return Ok(());
    }
    for comp in &comps[..comps.len() - 1] {
        acc.push(comp.as_os_str());
        if let Ok(meta) = fs::symlink_metadata(&acc)
            && !meta.is_dir()
        {
            fs::remove_file(&acc)?;
        }
    }
    Ok(())
}

fn write_tree_blob_to_disk(
    workdir: &Path,
    gix_repo: &gix::Repository,
    rel_path: &str,
    oid: ObjectId,
    kind: EntryKind,
) -> Result<()> {
    let disk_path = workdir.join(rel_path);
    remove_conflicting_non_dir_ancestors(workdir, &disk_path)?;
    if let Some(parent) = disk_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = fs::symlink_metadata(&disk_path) {
        if meta.is_dir() {
            fs::remove_dir_all(&disk_path)?;
        } else if meta.file_type().is_symlink() || kind == EntryKind::Link {
            fs::remove_file(&disk_path)?;
        }
    }

    let blob = gix_repo.find_blob(oid).map_err(OzError::git_source)?;

    if kind == EntryKind::Link {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let target = std::ffi::OsStr::from_bytes(&blob.data);
            std::os::unix::fs::symlink(target, &disk_path)?;
        }
        #[cfg(not(unix))]
        {
            let target = String::from_utf8_lossy(&blob.data);
            fs::write(&disk_path, target.as_bytes())?;
        }
    } else {
        fs::write(&disk_path, &blob.data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perm = if kind == EntryKind::BlobExecutable {
                0o755
            } else {
                0o644
            };
            fs::set_permissions(&disk_path, fs::Permissions::from_mode(perm))?;
        }
    }
    Ok(())
}

fn materialize_tree_delta(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    ops: &[TreeDeltaOp],
) -> Result<usize> {
    let mut updated = 0usize;

    for op in ops {
        if let TreeDeltaOp::Delete(rel_path) = op {
            let disk_path = repo.workdir_path.join(rel_path);
            match fs::symlink_metadata(&disk_path) {
                Ok(meta) => {
                    if meta.is_dir() {
                        fs::remove_dir_all(&disk_path)?;
                    } else {
                        fs::remove_file(&disk_path)?;
                    }
                    prune_empty_parents(&repo.workdir_path, &disk_path);
                    updated += 1;
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::NotFound
                        || err.kind() == std::io::ErrorKind::NotADirectory => {}
                Err(err) => return Err(OzError::Io(err)),
            }
        }
    }

    for op in ops {
        if let TreeDeltaOp::Upsert {
            path: rel_path,
            oid,
            kind,
        } = op
        {
            write_tree_blob_to_disk(&repo.workdir_path, gix_repo, rel_path, *oid, *kind)?;
            updated += 1;
        }
    }

    Ok(updated)
}

fn write_index_no_ext(index: &mut gix::index::File) -> Result<()> {
    index
        .write(gix::index::write::Options {
            extensions: gix::index::write::Extensions::None,
            skip_hash: true,
        })
        .map_err(OzError::git_source)
}

fn load_or_rebuild_index(
    index_path: &Path,
    gix_repo: &gix::Repository,
    target_tree_id: ObjectId,
) -> Result<(gix::index::File, bool)> {
    if !index_path.is_file() {
        let mut rebuilt = gix_repo
            .index_from_tree(&target_tree_id)
            .map_err(OzError::git_source)?;
        write_index_no_ext(&mut rebuilt)?;
        return Ok((rebuilt, true));
    }
    if let Ok(idx) = gix::index::File::at(
        index_path,
        gix_repo.object_hash(),
        true,
        gix::index::decode::Options::default(),
    ) {
        Ok((idx, false))
    } else {
        let mut rebuilt = gix_repo
            .index_from_tree(&target_tree_id)
            .map_err(OzError::git_source)?;
        write_index_no_ext(&mut rebuilt)?;
        Ok((rebuilt, true))
    }
}

fn patch_and_write_index<'a, I>(index: &mut gix::index::File, updates: I) -> Result<()>
where
    I: IntoIterator<
        Item = (
            &'a str,
            Option<(ObjectId, gix::index::entry::Mode, gix::index::entry::Stat)>,
        ),
    >,
{
    let items: Vec<_> = updates.into_iter().collect();
    if items.is_empty() {
        return Ok(());
    }
    let touched: BTreeSet<&str> = items.iter().map(|(path, _)| *path).collect();
    index.remove_tree();
    index.remove_entries(|_, path, _| touched.contains(path.to_str_lossy().as_ref()));

    for (path, entry) in items {
        if let Some((oid, mode, stat)) = entry {
            index.dangerously_push_entry(
                stat,
                oid,
                gix::index::entry::Flags::empty(),
                mode,
                path.as_bytes().as_bstr(),
            );
        }
    }
    index.sort_entries();
    write_index_no_ext(index)
}

fn sync_index_for_delta(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    target_tree_id: ObjectId,
    delta: &[TreeDeltaOp],
) -> Result<()> {
    if delta.is_empty() || (repo.workspace.is_some() && !repo.is_linked_worktree) {
        return Ok(());
    }
    let index_path = repo.git_dir.join("index");
    let (mut index, already_rebuilt) =
        load_or_rebuild_index(&index_path, gix_repo, target_tree_id)?;
    if already_rebuilt {
        return Ok(());
    }

    let updates = delta.iter().map(|op| match op {
        TreeDeltaOp::Upsert { path, oid, kind } => {
            let disk_path = repo.workdir_path.join(path);
            let stat = gix::index::fs::Metadata::from_path_no_follow(&disk_path)
                .ok()
                .and_then(|meta| gix::index::entry::Stat::from_fs(&meta).ok())
                .unwrap_or_default();
            (path.as_str(), Some((*oid, (*kind).into(), stat)))
        }
        TreeDeltaOp::Delete(path) => (path.as_str(), None),
    });
    patch_and_write_index(&mut index, updates)
}

/// Ensures `.git/index` entries for `paths` match `target_tree_id` (`HEAD`), without staging
/// working-tree modifications or untracked files created by `oz stash pop`.
fn sync_index_paths_to_tree(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    target_tree_id: ObjectId,
    paths: &[String],
) -> Result<()> {
    if paths.is_empty() || (repo.workspace.is_some() && !repo.is_linked_worktree) {
        return Ok(());
    }
    let index_path = repo.git_dir.join("index");
    if !index_path.is_file() {
        return Ok(());
    }
    let (mut index, already_rebuilt) =
        load_or_rebuild_index(&index_path, gix_repo, target_tree_id)?;
    if already_rebuilt {
        return Ok(());
    }

    let empty_tree_id = ObjectId::empty_tree(gix_repo.object_hash());
    let target_tree = if target_tree_id == empty_tree_id {
        None
    } else {
        Some(
            gix_repo
                .find_tree(target_tree_id)
                .map_err(OzError::git_source)?,
        )
    };
    let mut patches = Vec::new();
    for rel in paths {
        let lookup = match target_tree.as_ref() {
            Some(tree) => tree
                .lookup_entry_by_path(rel.as_str())
                .map_err(OzError::git_source)?,
            None => None,
        };
        let target = match lookup {
            Some(entry) if !entry.mode().is_tree() => {
                Some((entry.object_id(), entry.mode().kind().into()))
            }
            _ => None,
        };
        let bstr = rel.as_bytes().as_bstr();
        let unconflicted =
            index.entry_by_path_and_stage(bstr, gix::index::entry::Stage::Unconflicted);
        let matches = match (target, unconflicted) {
            (Some((oid, mode)), Some(entry)) => {
                entry.id == oid
                    && entry.mode == mode
                    && index
                        .entry_by_path_and_stage(bstr, gix::index::entry::Stage::Base)
                        .is_none()
                    && index
                        .entry_by_path_and_stage(bstr, gix::index::entry::Stage::Ours)
                        .is_none()
                    && index
                        .entry_by_path_and_stage(bstr, gix::index::entry::Stage::Theirs)
                        .is_none()
            }
            (None, None) => index.entry_by_path(bstr).is_none(),
            _ => false,
        };
        if !matches {
            // Use zeroed stat so dirty files popped onto disk are never marked clean against HEAD.
            patches.push((
                rel.as_str(),
                target.map(|(oid, mode)| (oid, mode, gix::index::entry::Stat::default())),
            ));
        }
    }

    patch_and_write_index(&mut index, patches)
}

/// Regular expression matcher backed by `regex_automata::meta::Regex` for `oz grep`,
/// `oz log --grep`, and `oz log -G`.
#[derive(Debug, Clone)]
struct SimpleRegex {
    inner: regex_automata::meta::Regex,
}

impl SimpleRegex {
    fn compile(pattern: &str, case_insensitive: bool) -> Result<Self> {
        let syntax_cfg = regex_automata::util::syntax::Config::new()
            .case_insensitive(case_insensitive)
            .multi_line(false);
        let inner = regex_automata::meta::Regex::builder()
            .syntax(syntax_cfg)
            .build(pattern)
            .map_err(|err| OzError::InvalidUsage {
                message: format!("invalid regex pattern '{pattern}': {err}"),
                recovery: Vec::new(),
            })?;
        Ok(Self { inner })
    }

    fn compile_literal(literal: &str, case_insensitive: bool) -> Result<Self> {
        let escaped = regex_syntax::escape(literal);
        Self::compile(&escaped, case_insensitive)
    }

    fn is_match(&self, text: &str) -> bool {
        self.inner.is_match(text)
    }
}

/// Parses `--since` timestamps (RFC3339, `YYYY-MM-DD`, Unix epoch, or relative `"2 days ago"` / `"12h"`).
pub(crate) fn parse_since_timestamp(raw: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "--since value cannot be empty".to_string(),
            recovery: vec![
                "Pass e.g. `--since \"2 days ago\"` or `--since 2026-11-20`.".to_string(),
            ],
        });
    }

    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt.with_timezone(&Utc));
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(trimmed, fmt) {
            return Ok(Utc.from_utc_datetime(&ndt));
        }
    }
    if let Ok(nd) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
        && let Some(ndt) = nd.and_hms_opt(0, 0, 0)
    {
        return Ok(Utc.from_utc_datetime(&ndt));
    }
    if let Ok(secs) = trimmed.parse::<i64>()
        && let Some(dt) = DateTime::<Utc>::from_timestamp(secs, 0)
    {
        return Ok(dt);
    }

    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "now" => return Ok(now),
        "today" => {
            if let Some(ndt) = now.date_naive().and_hms_opt(0, 0, 0) {
                return Ok(Utc.from_utc_datetime(&ndt));
            }
        }
        "yesterday" => return Ok(now - Duration::days(1)),
        _ => {}
    }

    let without_ago = lower.strip_suffix("ago").map_or(lower.as_str(), str::trim);
    let tokens: Vec<&str> = without_ago.split_whitespace().collect();
    if tokens.len() == 2
        && let Ok(n) = tokens[0].parse::<i64>()
        && n >= 0
    {
        let dur = match tokens[1] {
            "s" | "sec" | "secs" | "second" | "seconds" => Some(Duration::seconds(n)),
            "m" | "min" | "mins" | "minute" | "minutes" => Some(Duration::minutes(n)),
            "h" | "hr" | "hrs" | "hour" | "hours" => Some(Duration::hours(n)),
            "d" | "day" | "days" => Some(Duration::days(n)),
            "w" | "wk" | "wks" | "week" | "weeks" => Some(Duration::weeks(n)),
            "mo" | "mon" | "month" | "months" => Some(Duration::days(n.saturating_mul(30))),
            "y" | "yr" | "yrs" | "year" | "years" => Some(Duration::days(n.saturating_mul(365))),
            _ => None,
        };
        if let Some(d) = dur {
            return Ok(now - d);
        }
    }

    if tokens.len() == 1
        && let Some(idx) = without_ago.find(|c: char| !c.is_ascii_digit())
    {
        let (num_str, unit) = without_ago.split_at(idx);
        if let Ok(n) = num_str.parse::<i64>()
            && n >= 0
        {
            let dur = match unit {
                "s" => Some(Duration::seconds(n)),
                "m" => Some(Duration::minutes(n)),
                "h" => Some(Duration::hours(n)),
                "d" => Some(Duration::days(n)),
                "w" => Some(Duration::weeks(n)),
                _ => None,
            };
            if let Some(d) = dur {
                return Ok(now - d);
            }
        }
    }

    Err(OzError::InvalidUsage {
        message: format!("invalid --since date/duration '{raw}'"),
        recovery: vec![
            "Supported formats: ISO 8601 (`2026-11-21T12:00:00Z`), `YYYY-MM-DD`, relative (`2 days ago`, `12h`, `yesterday`).".to_string(),
        ],
    })
}

// ============================================================================
// 1. `oz log`
// ============================================================================

/// Options controlling [`log_commits`].
#[derive(Debug, Clone)]
pub struct LogOptions {
    pub rev: Option<String>,
    /// Optional multiple starting revisions, ranges (`A..B`, `A...B`), or `^exclude` refs.
    pub revs: Vec<String>,
    pub all: bool,
    pub limit: Option<usize>,
    pub author: Option<String>,
    pub grep: Option<String>,
    pub ignore_case: bool,
    pub left_right: bool,
    pub search_string: Option<String>,
    pub search_diff_regex: Option<String>,
    pub since: Option<String>,
    pub graph: bool,
    pub paths: Vec<String>,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            rev: None,
            revs: Vec::new(),
            all: false,
            limit: None,
            author: None,
            grep: None,
            ignore_case: false,
            left_right: false,
            search_string: None,
            search_diff_regex: None,
            since: None,
            graph: true,
            paths: Vec::new(),
        }
    }
}

/// Single commit entry emitted by [`log_commits`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogCommitEntry {
    pub oid: String,
    pub short_oid: String,
    pub tree_oid: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_time: DateTime<Utc>,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_time: DateTime<Utc>,
    pub subject: String,
    pub body: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    #[serde(default)]
    pub refs: Vec<String>,
    /// Symmetric-difference side (`"left"` or `"right"`) when logging an `A...B` range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    pub graph_prefix: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub graph_connectors: Vec<String>,
}

/// Structured commit log report returned by [`log_commits`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_rev: Option<String>,
    pub unborn: bool,
    pub total_returned: usize,
    pub commits: Vec<LogCommitEntry>,
}

/// Walks commits and applies `--author`, `--grep`, `-S`, `-G`, `--since`, and `-n` filters
/// along with compact ASCII commit graph prefixes.
pub fn log_commits(repo: &OzoneRepo, opts: &LogOptions) -> Result<LogReport> {
    let gix_repo = open_gix_for_workspace(repo)?;
    let grep_re = opts
        .grep
        .as_deref()
        .map(|pat| SimpleRegex::compile(pat, true))
        .transpose()?;
    let diff_re = opts
        .search_diff_regex
        .as_deref()
        .map(|pat| SimpleRegex::compile(pat, opts.ignore_case))
        .transpose()?;
    let since_cutoff = opts
        .since
        .as_deref()
        .map(|s| parse_since_timestamp(s, Utc::now()))
        .transpose()?;

    let (starts, hidden, target_label, symmetric_sides) =
        resolve_log_bounds(repo, &gix_repo, opts)?;
    if starts.is_empty() {
        return Ok(LogReport {
            target_rev: target_label,
            unborn: true,
            total_returned: 0,
            commits: Vec::new(),
        });
    }

    let left_reachable: HashSet<ObjectId> = if let Some((left_tip, _)) = symmetric_sides {
        collect_reachable_excluding(&gix_repo, left_tip, &hidden)?
    } else {
        HashSet::new()
    };
    let right_reachable: HashSet<ObjectId> = if let Some((_, right_tip)) = symmetric_sides {
        collect_reachable_excluding(&gix_repo, right_tip, &hidden)?
    } else {
        HashSet::new()
    };

    let normalized_paths: Vec<LogPathFilter> = opts
        .paths
        .iter()
        .map(|p| LogPathFilter::compile(p))
        .collect::<Result<Vec<_>>>()?;

    let ref_decorations = collect_ref_decorations(repo, &gix_repo)?;
    let has_content_filter = opts.author.is_some()
        || opts.grep.is_some()
        || opts.search_string.is_some()
        || opts.search_diff_regex.is_some()
        || since_cutoff.is_some()
        || !normalized_paths.is_empty();

    let mut walk_platform = gix_repo
        .rev_walk(starts)
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst));
    if !hidden.is_empty() {
        walk_platform = walk_platform.with_hidden(hidden);
    }
    let walk = walk_platform.all().map_err(OzError::git_source)?;

    let mut commits = Vec::new();
    let mut lanes: Vec<ObjectId> = Vec::new();

    for item in walk {
        if let Some(limit) = opts.limit
            && commits.len() >= limit
        {
            break;
        }
        let info = item.map_err(OzError::git_source)?;
        let commit_id = info.id;
        let commit = gix_repo
            .find_commit(commit_id)
            .map_err(OzError::git_source)?;
        let decoded = commit.decode().map_err(OzError::git_source)?;

        let author = decoded.author().map_err(OzError::git_source)?;
        let committer = decoded.committer().map_err(OzError::git_source)?;

        let author_name = author.name.to_str_lossy().into_owned();
        let author_email = author.email.to_str_lossy().into_owned();
        let author_time = gix_time_to_utc(author.time().map_err(OzError::git_source)?);
        let committer_name = committer.name.to_str_lossy().into_owned();
        let committer_email = committer.email.to_str_lossy().into_owned();
        let committer_time = gix_time_to_utc(committer.time().map_err(OzError::git_source)?);
        let message = decoded.message.to_str_lossy().into_owned();
        let (subject, body) = split_subject_and_body(&message);
        let change_id = extract_change_id(&message);

        if let Some(cutoff) = since_cutoff
            && author_time < cutoff
            && committer_time < cutoff
        {
            continue;
        }
        if let Some(ref author_pat) = opts.author {
            let needle = author_pat.to_ascii_lowercase();
            let combined = format!("{author_name} <{author_email}>").to_ascii_lowercase();
            if !combined.contains(&needle) {
                continue;
            }
        }
        if let Some(ref re) = grep_re
            && !re.is_match(&message)
        {
            continue;
        }

        let tree_id = decoded.tree();
        let parent_ids: Vec<ObjectId> = info.parent_ids.to_vec();

        if !normalized_paths.is_empty()
            && !commit_touches_paths(&gix_repo, tree_id, &parent_ids, &normalized_paths)?
        {
            continue;
        }

        if opts.search_string.is_some() || diff_re.is_some() {
            let parent_tree_id = match parent_ids.first() {
                Some(&pid) => commit_tree_id(&gix_repo, pid)?,
                None => ObjectId::empty_tree(gix_repo.object_hash()),
            };
            if !commit_diff_matches_filters(
                &gix_repo,
                parent_tree_id,
                tree_id,
                opts.search_string.as_deref(),
                diff_re.as_ref(),
                &normalized_paths,
            )? {
                continue;
            }
        }

        let (graph_prefix, graph_connectors) = if !opts.graph {
            (String::new(), Vec::new())
        } else if has_content_filter {
            ("* ".to_string(), Vec::new())
        } else {
            advance_graph_lanes(&mut lanes, commit_id, &parent_ids)
        };

        let oid_hex = commit_id.to_hex().to_string();
        let short_oid = oid_hex[..oid_hex.len().min(7)].to_string();
        let refs = ref_decorations.get(&commit_id).cloned().unwrap_or_default();
        let side = if symmetric_sides.is_some() {
            if left_reachable.contains(&commit_id) {
                Some("left".to_string())
            } else if right_reachable.contains(&commit_id) {
                Some("right".to_string())
            } else {
                None
            }
        } else {
            None
        };

        commits.push(LogCommitEntry {
            oid: oid_hex,
            short_oid,
            tree_oid: tree_id.to_hex().to_string(),
            parents: parent_ids.iter().map(|p| p.to_hex().to_string()).collect(),
            author_name,
            author_email,
            author_time,
            committer_name,
            committer_email,
            committer_time,
            subject,
            body,
            message,
            change_id,
            refs,
            side,
            graph_prefix,
            graph_connectors,
        });
    }

    let total_returned = commits.len();
    Ok(LogReport {
        target_rev: target_label,
        unborn: false,
        total_returned,
        commits,
    })
}

fn collect_reachable_excluding(
    gix_repo: &gix::Repository,
    start: ObjectId,
    hidden: &[ObjectId],
) -> Result<HashSet<ObjectId>> {
    let mut walk = gix_repo.rev_walk([start]);
    if !hidden.is_empty() {
        walk = walk.with_hidden(hidden.iter().copied());
    }
    let mut set = HashSet::new();
    for item in walk.all().map_err(OzError::git_source)? {
        let info = item.map_err(OzError::git_source)?;
        set.insert(info.id);
    }
    Ok(set)
}

#[allow(clippy::type_complexity)]
fn resolve_log_bounds(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    opts: &LogOptions,
) -> Result<(
    Vec<ObjectId>,
    Vec<ObjectId>,
    Option<String>,
    Option<(ObjectId, ObjectId)>,
)> {
    if opts.all {
        let mut starts = Vec::new();
        let platform = gix_repo.references().map_err(OzError::git_source)?;
        for item in platform.local_branches().map_err(OzError::git_source)? {
            let mut r = item.map_err(OzError::git_source)?;
            if let Ok(c) = r.peel_to_commit() {
                starts.push(c.id);
            }
        }
        for item in platform.tags().map_err(OzError::git_source)? {
            let mut r = item.map_err(OzError::git_source)?;
            if let Ok(c) = r.peel_to_commit() {
                starts.push(c.id);
            }
        }
        let head = resolve_workspace_head(repo, gix_repo)?;
        if let Some(cid) = head.commit_oid
            && !starts.contains(&cid)
        {
            starts.push(cid);
        }
        return Ok((starts, Vec::new(), Some("--all".to_string()), None));
    }

    let mut raw_specs: Vec<&str> = Vec::new();
    if let Some(ref single) = opts.rev {
        raw_specs.push(single.as_str());
    }
    for r in &opts.revs {
        raw_specs.push(r.as_str());
    }

    if raw_specs.is_empty() {
        let head = resolve_workspace_head(repo, gix_repo)?;
        return match head.commit_oid {
            Some(cid) => Ok((
                vec![cid],
                Vec::new(),
                head.branch.or(Some("HEAD".to_string())),
                None,
            )),
            None => Ok((Vec::new(), Vec::new(), head.branch, None)),
        };
    }

    let mut starts: Vec<ObjectId> = Vec::new();
    let mut hidden: Vec<ObjectId> = Vec::new();
    let mut symmetric_sides: Option<(ObjectId, ObjectId)> = None;

    for raw_rev in &raw_specs {
        if let Some((lhs, rhs)) = raw_rev.split_once("...") {
            let left_spec = if lhs.trim().is_empty() {
                "HEAD"
            } else {
                lhs.trim()
            };
            let right_spec = if rhs.trim().is_empty() {
                "HEAD"
            } else {
                rhs.trim()
            };
            let left_oid = resolve_commit_oid(gix_repo, left_spec)?;
            let right_oid = resolve_commit_oid(gix_repo, right_spec)?;
            if !starts.contains(&left_oid) {
                starts.push(left_oid);
            }
            if !starts.contains(&right_oid) {
                starts.push(right_oid);
            }
            if let Ok(Some(mb)) = gix_repo.merge_base(left_oid, right_oid) {
                let mb_id = mb.detach();
                if !hidden.contains(&mb_id) {
                    hidden.push(mb_id);
                }
            }
            symmetric_sides = Some((left_oid, right_oid));
        } else if let Some((lhs, rhs)) = raw_rev.split_once("..")
            && !rhs.starts_with('.')
        {
            let left_spec = if lhs.trim().is_empty() {
                "HEAD"
            } else {
                lhs.trim()
            };
            let right_spec = if rhs.trim().is_empty() {
                "HEAD"
            } else {
                rhs.trim()
            };
            let left_oid = resolve_commit_oid(gix_repo, left_spec)?;
            let right_oid = resolve_commit_oid(gix_repo, right_spec)?;
            if !starts.contains(&right_oid) {
                starts.push(right_oid);
            }
            if !hidden.contains(&left_oid) {
                hidden.push(left_oid);
            }
        } else if let Some(neg) = raw_rev.strip_prefix('^') {
            let oid = resolve_commit_oid(gix_repo, neg)?;
            if !hidden.contains(&oid) {
                hidden.push(oid);
            }
        } else {
            let oid = resolve_commit_oid(gix_repo, raw_rev)?;
            if !starts.contains(&oid) {
                starts.push(oid);
            }
        }
    }

    if starts.is_empty() {
        let head = resolve_workspace_head(repo, gix_repo)?;
        if let Some(cid) = head.commit_oid {
            starts.push(cid);
        }
    }

    let label = Some(raw_specs.join(" "));
    Ok((starts, hidden, label, symmetric_sides))
}

fn collect_ref_decorations(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
) -> Result<HashMap<ObjectId, Vec<String>>> {
    let mut map: HashMap<ObjectId, Vec<String>> = HashMap::new();
    let head = resolve_workspace_head(repo, gix_repo)?;

    if let Some(head_oid) = head.commit_oid {
        let label = match head.branch {
            Some(ref b) => format!("HEAD -> {b}"),
            None => "HEAD".to_string(),
        };
        map.entry(head_oid).or_default().push(label);
    }

    let platform = gix_repo.references().map_err(OzError::git_source)?;

    for item in platform.local_branches().map_err(OzError::git_source)? {
        let mut r = item.map_err(OzError::git_source)?;
        let name = r.name().shorten().to_str_lossy().into_owned();
        if head.branch.as_deref() == Some(name.as_str()) {
            continue;
        }
        if let Ok(c) = r.peel_to_commit() {
            map.entry(c.id).or_default().push(name);
        }
    }

    for item in platform.tags().map_err(OzError::git_source)? {
        let mut r = item.map_err(OzError::git_source)?;
        let name = r.name().shorten().to_str_lossy().into_owned();
        if let Ok(c) = r.peel_to_commit() {
            map.entry(c.id).or_default().push(format!("tag: {name}"));
        }
    }

    for item in platform.remote_branches().map_err(OzError::git_source)? {
        let mut r = item.map_err(OzError::git_source)?;
        let name = r.name().shorten().to_str_lossy().into_owned();
        if let Ok(c) = r.peel_to_commit() {
            map.entry(c.id).or_default().push(name);
        }
    }

    Ok(map)
}

fn advance_graph_lanes(
    lanes: &mut Vec<ObjectId>,
    commit_id: ObjectId,
    parents: &[ObjectId],
) -> (String, Vec<String>) {
    let col = if let Some(idx) = lanes.iter().position(|id| *id == commit_id) {
        idx
    } else {
        lanes.push(commit_id);
        lanes.len() - 1
    };

    let mut prefix = String::with_capacity(lanes.len() * 2 + 1);
    for (idx, _) in lanes.iter().enumerate() {
        if idx == col {
            prefix.push('*');
        } else {
            prefix.push('|');
        }
        prefix.push(' ');
    }

    let was_multi_parent = parents.len() > 1;
    let prev_len = lanes.len();

    if parents.is_empty() {
        lanes.remove(col);
    } else {
        lanes[col] = parents[0];
        if parents.len() > 1 {
            let mut present: HashSet<ObjectId> =
                HashSet::with_capacity(lanes.len() + parents.len());
            present.extend(lanes.iter().copied());
            let mut insert_at = col + 1;
            for &p in &parents[1..] {
                if present.insert(p) {
                    lanes.insert(insert_at, p);
                    insert_at += 1;
                }
            }
        }
    }

    // Deduplicate converging lanes.
    if lanes.len() > 1 {
        let mut seen = HashSet::with_capacity(lanes.len());
        lanes.retain(|id| seen.insert(*id));
    }

    let mut connectors = Vec::new();
    if was_multi_parent && lanes.len() > prev_len {
        let mut conn = String::new();
        for idx in 0..prev_len {
            if idx == col {
                conn.push_str("|\\");
            } else {
                conn.push_str("| ");
            }
        }
        connectors.push(conn.trim_end().to_string());
    } else if lanes.len() < prev_len && !parents.is_empty() {
        let mut conn = String::new();
        for idx in 0..prev_len {
            if idx == col {
                conn.push('|');
            } else if idx == prev_len - 1 {
                conn.push('/');
            } else {
                conn.push_str("| ");
            }
        }
        let trimmed = conn.trim_end().to_string();
        if trimmed.contains('/') {
            connectors.push(trimmed);
        }
    }

    (prefix, connectors)
}

struct LogPathFilter {
    normalized: String,
    glob: Option<globset::GlobMatcher>,
}

impl LogPathFilter {
    fn compile(raw: &str) -> Result<Self> {
        let replaced = raw.replace('\\', "/");
        let stripped = replaced.strip_prefix("./").unwrap_or(&replaced);
        let normalized = stripped
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_string();
        let has_glob = normalized.contains('*')
            || normalized.contains('?')
            || normalized.contains('[')
            || normalized.contains('{');
        let glob = if has_glob {
            Some(
                globset::GlobBuilder::new(&normalized)
                    .literal_separator(false)
                    .build()
                    .map_err(|err| OzError::InvalidUsage {
                        message: format!("invalid log path pattern '{raw}': {err}"),
                        recovery: vec!["Pass a valid path or glob after `--`.".to_string()],
                    })?
                    .compile_matcher(),
            )
        } else {
            None
        };
        Ok(Self { normalized, glob })
    }

    fn matches_path(&self, path: &str) -> bool {
        if self.normalized.is_empty() || self.normalized == "." {
            return true;
        }
        if path == self.normalized {
            return true;
        }
        if path
            .strip_prefix(&self.normalized)
            .is_some_and(|rest| rest.starts_with('/'))
        {
            return true;
        }
        self.glob.as_ref().is_some_and(|g| g.is_match(path))
    }
}

fn tree_entry_sig(
    gix_repo: &gix::Repository,
    tree_id: ObjectId,
    rel_path: &str,
) -> Result<Option<(gix::objs::tree::EntryKind, ObjectId)>> {
    if rel_path.is_empty() || rel_path == "." {
        return Ok(Some((gix::objs::tree::EntryKind::Tree, tree_id)));
    }
    let tree = gix_repo.find_tree(tree_id).map_err(OzError::git_source)?;
    let entry = tree
        .lookup_entry_by_path(rel_path)
        .map_err(OzError::git_source)?;
    Ok(entry.map(|e| (e.mode().kind(), e.object_id())))
}

fn trees_differ_on_paths(
    gix_repo: &gix::Repository,
    lhs_tree_id: ObjectId,
    rhs_tree_id: ObjectId,
    paths: &[LogPathFilter],
) -> Result<bool> {
    if lhs_tree_id == rhs_tree_id {
        return Ok(false);
    }
    let all_literal = paths.iter().all(|p| p.glob.is_none());
    if all_literal {
        for p in paths {
            let lhs_sig = tree_entry_sig(gix_repo, lhs_tree_id, &p.normalized)?;
            let rhs_sig = tree_entry_sig(gix_repo, rhs_tree_id, &p.normalized)?;
            if lhs_sig != rhs_sig {
                return Ok(true);
            }
        }
        return Ok(false);
    }

    let lhs_tree = gix_repo
        .find_tree(lhs_tree_id)
        .map_err(OzError::git_source)?;
    let rhs_tree = gix_repo
        .find_tree(rhs_tree_id)
        .map_err(OzError::git_source)?;
    let mut state = gix::diff::tree::State::default();
    let mut recorder = gix::diff::tree::Recorder::default()
        .track_location(Some(gix::diff::tree::recorder::Location::Path));
    let lhs_iter = gix::objs::TreeRefIter::from_bytes(&lhs_tree.data, lhs_tree.id.kind());
    let rhs_iter = gix::objs::TreeRefIter::from_bytes(&rhs_tree.data, rhs_tree.id.kind());
    gix::diff::tree(
        lhs_iter,
        rhs_iter,
        &mut state,
        &gix_repo.objects,
        &mut recorder,
    )
    .map_err(OzError::git_source)?;

    for record in recorder.records {
        let loc = match record {
            gix::diff::tree::recorder::Change::Addition { path, .. }
            | gix::diff::tree::recorder::Change::Deletion { path, .. }
            | gix::diff::tree::recorder::Change::Modification { path, .. } => path,
        };
        let path_str = loc.to_str_lossy();
        if paths.iter().any(|p| p.matches_path(&path_str)) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn commit_touches_paths(
    gix_repo: &gix::Repository,
    commit_tree_id_val: ObjectId,
    parent_ids: &[ObjectId],
    paths: &[LogPathFilter],
) -> Result<bool> {
    if parent_ids.is_empty() {
        let empty = ObjectId::empty_tree(gix_repo.object_hash());
        return trees_differ_on_paths(gix_repo, empty, commit_tree_id_val, paths);
    }
    // Standard Git TREESAME simplification: a commit is included when it differs
    // on the filtered path(s) from all of its parents (for 1-parent commits, differs from parent).
    for &pid in parent_ids {
        let p_tree = commit_tree_id(gix_repo, pid)?;
        if !trees_differ_on_paths(gix_repo, p_tree, commit_tree_id_val, paths)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn commit_diff_matches_filters(
    gix_repo: &gix::Repository,
    parent_tree_id: ObjectId,
    commit_tree_id: ObjectId,
    search_string: Option<&str>,
    diff_re: Option<&SimpleRegex>,
    path_filters: &[LogPathFilter],
) -> Result<bool> {
    if parent_tree_id == commit_tree_id {
        return Ok(false);
    }
    let lhs_tree = gix_repo
        .find_tree(parent_tree_id)
        .map_err(OzError::git_source)?;
    let rhs_tree = gix_repo
        .find_tree(commit_tree_id)
        .map_err(OzError::git_source)?;

    let mut state = gix::diff::tree::State::default();
    let mut recorder = gix::diff::tree::Recorder::default()
        .track_location(Some(gix::diff::tree::recorder::Location::Path));
    let lhs_iter = gix::objs::TreeRefIter::from_bytes(&lhs_tree.data, lhs_tree.id.kind());
    let rhs_iter = gix::objs::TreeRefIter::from_bytes(&rhs_tree.data, rhs_tree.id.kind());

    gix::diff::tree(
        lhs_iter,
        rhs_iter,
        &mut state,
        &gix_repo.objects,
        &mut recorder,
    )
    .map_err(OzError::git_source)?;

    for record in recorder.records {
        let (loc, old_oid, new_oid) = match record {
            gix::diff::tree::recorder::Change::Addition {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if entry_mode.is_tree() || entry_mode.kind() == EntryKind::Commit {
                    continue;
                }
                (path, None, Some(oid))
            }
            gix::diff::tree::recorder::Change::Deletion {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if entry_mode.is_tree() || entry_mode.kind() == EntryKind::Commit {
                    continue;
                }
                (path, Some(oid), None)
            }
            gix::diff::tree::recorder::Change::Modification {
                previous_entry_mode,
                previous_oid,
                entry_mode,
                oid,
                path,
                ..
            } => {
                if entry_mode.is_tree() || entry_mode.kind() == EntryKind::Commit {
                    continue;
                }
                let prev = if previous_entry_mode.is_tree()
                    || previous_entry_mode.kind() == EntryKind::Commit
                {
                    None
                } else {
                    Some(previous_oid)
                };
                (path, prev, Some(oid))
            }
        };
        if !path_filters.is_empty() {
            let loc_str = loc.to_str_lossy();
            if !path_filters.iter().any(|p| p.matches_path(&loc_str)) {
                continue;
            }
        }

        let old_bytes = match old_oid {
            Some(id) => read_blob_bytes(gix_repo, id)?,
            None => Vec::new(),
        };
        let new_bytes = match new_oid {
            Some(id) => read_blob_bytes(gix_repo, id)?,
            None => Vec::new(),
        };
        if is_binary_bytes(&old_bytes) || is_binary_bytes(&new_bytes) {
            continue;
        }

        if let Some(needle) = search_string {
            let old_text = String::from_utf8_lossy(&old_bytes);
            let new_text = String::from_utf8_lossy(&new_bytes);
            if old_text.matches(needle).count() == new_text.matches(needle).count() {
                continue;
            }
            if diff_re.is_none() {
                return Ok(true);
            }
        }

        if let Some(re) = diff_re {
            let input = InternedInput::new(old_bytes.as_slice(), new_bytes.as_slice());
            let mut diff = Diff::compute(Algorithm::Histogram, &input);
            diff.postprocess_lines(&input);
            let before_lines: Vec<&[u8]> = sources::byte_lines(&old_bytes).collect();
            let after_lines: Vec<&[u8]> = sources::byte_lines(&new_bytes).collect();
            let mut matched = false;
            for hunk in diff.hunks() {
                for line in &before_lines[hunk.before.start as usize..hunk.before.end as usize] {
                    let s = String::from_utf8_lossy(line);
                    let trimmed = s.trim_end_matches(['\r', '\n']);
                    if re.is_match(trimmed) {
                        matched = true;
                        break;
                    }
                }
                if matched {
                    break;
                }
                for line in &after_lines[hunk.after.start as usize..hunk.after.end as usize] {
                    let s = String::from_utf8_lossy(line);
                    let trimmed = s.trim_end_matches(['\r', '\n']);
                    if re.is_match(trimmed) {
                        matched = true;
                        break;
                    }
                }
                if matched {
                    break;
                }
            }
            if matched {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

// ============================================================================
// 2. `oz show <rev>`
// ============================================================================

/// Options controlling [`show_commit`].
#[derive(Debug, Clone, Default)]
pub struct ShowOptions {
    pub rev: Option<String>,
    pub stat_only: bool,
}

/// Per-file stat line inside [`ShowStatSummary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowFileStat {
    pub path: String,
    pub kind: ChangeKind,
    pub is_binary: bool,
    pub additions: u32,
    pub deletions: u32,
}

/// Summary diffstat block for `oz show`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowStatSummary {
    pub files_changed: usize,
    pub total_additions: u32,
    pub total_deletions: u32,
    pub entries: Vec<ShowFileStat>,
}

/// Detailed commit view returned by [`show_commit`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowReport {
    pub commit_oid: String,
    pub short_oid: String,
    pub tree_oid: String,
    pub parents: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_message: Option<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_time: DateTime<Utc>,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_time: DateTime<Utc>,
    pub subject: String,
    pub body: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    pub signed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_valid: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_fingerprint: Option<String>,
    pub stat: ShowStatSummary,
    pub files: Vec<FileChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unified_diff: Option<String>,
}

/// Inspects a commit (or tag peeling to a commit), returning metadata, `Change-Id`,
/// SSH signature status, diffstat, and unified diff.
pub fn show_commit(repo: &OzoneRepo, opts: &ShowOptions) -> Result<ShowReport> {
    let gix_repo = open_gix_for_workspace(repo)?;
    let rev_spec = opts.rev.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let resolved_id = if let Some(spec) = rev_spec {
        gix_repo
            .rev_parse_single(spec)
            .map_err(|err| rev_parse_error(spec, err))?
            .detach()
    } else {
        let head = resolve_workspace_head(repo, &gix_repo)?;
        head.commit_oid.ok_or_else(|| OzError::InvalidUsage {
            message: "repository has no commits yet (HEAD is unborn)".to_string(),
            recovery: vec!["Create an initial commit with `oz commit -m \"...\"`.".to_string()],
        })?
    };

    let obj = gix_repo
        .find_object(resolved_id)
        .map_err(OzError::git_source)?;

    let (tag_name, tag_message) = if obj.kind == gix::object::Kind::Tag {
        let tag_ref = gix::objs::TagRef::from_bytes(&obj.data, gix_repo.object_hash())
            .map_err(OzError::git_source)?;
        (
            Some(tag_ref.name.to_str_lossy().into_owned()),
            Some(tag_ref.message.to_str_lossy().trim().to_string()),
        )
    } else {
        (None, None)
    };

    let commit = obj.peel_to_commit().map_err(|err| OzError::InvalidUsage {
        message: format!("revision does not point to a commit: {err}"),
        recovery: Vec::new(),
    })?;
    let commit_id = commit.id;
    let decoded = commit.decode().map_err(OzError::git_source)?;

    let author = decoded.author().map_err(OzError::git_source)?;
    let committer = decoded.committer().map_err(OzError::git_source)?;

    let author_name = author.name.to_str_lossy().into_owned();
    let author_email = author.email.to_str_lossy().into_owned();
    let author_time = gix_time_to_utc(author.time().map_err(OzError::git_source)?);
    let committer_name = committer.name.to_str_lossy().into_owned();
    let committer_email = committer.email.to_str_lossy().into_owned();
    let committer_time = gix_time_to_utc(committer.time().map_err(OzError::git_source)?);
    let message = decoded.message.to_str_lossy().into_owned();
    let (subject, body) = split_subject_and_body(&message);
    let change_id = extract_change_id(&message);

    let sig_field = gix::objs::commit::signature_field_name(gix_repo.object_hash());
    let has_sig = decoded.extra_headers().find(sig_field).is_some();
    let (signed, signature_valid, signature_fingerprint) = if has_sig {
        match verify_commit_signature(repo, &commit_id) {
            Ok(v) => (true, Some(v.valid), Some(v.fingerprint)),
            Err(_) => (true, Some(false), None),
        }
    } else {
        (false, None, None)
    };

    let parents: Vec<ObjectId> = commit.parent_ids().map(gix::Id::detach).collect();
    let parent_tree_id = match parents.first() {
        Some(&pid) => commit_tree_id(&gix_repo, pid)?,
        None => ObjectId::empty_tree(gix_repo.object_hash()),
    };
    let commit_tree_id = decoded.tree();

    let files = diff_commit_trees(&gix_repo, parent_tree_id, commit_tree_id)?;
    let mut total_additions = 0u32;
    let mut total_deletions = 0u32;
    let mut stat_entries = Vec::with_capacity(files.len());
    let mut unified_buf = String::new();

    for f in &files {
        total_additions += f.additions;
        total_deletions += f.deletions;
        stat_entries.push(ShowFileStat {
            path: f.path.clone(),
            kind: f.kind,
            is_binary: f.is_binary,
            additions: f.additions,
            deletions: f.deletions,
        });
        if !opts.stat_only {
            unified_buf.push_str(&render_file_unified_diff(f));
        }
    }

    let oid_hex = commit_id.to_hex().to_string();
    let short_oid = oid_hex[..oid_hex.len().min(7)].to_string();

    Ok(ShowReport {
        commit_oid: oid_hex,
        short_oid,
        tree_oid: commit_tree_id.to_hex().to_string(),
        parents: parents.iter().map(|p| p.to_hex().to_string()).collect(),
        tag_name,
        tag_message,
        author_name,
        author_email,
        author_time,
        committer_name,
        committer_email,
        committer_time,
        subject,
        body,
        message,
        change_id,
        signed,
        signature_valid,
        signature_fingerprint,
        stat: ShowStatSummary {
            files_changed: files.len(),
            total_additions,
            total_deletions,
            entries: stat_entries,
        },
        files,
        unified_diff: (!opts.stat_only).then_some(unified_buf),
    })
}

// ============================================================================
// 2b. `oz show <rev>:<path>` (Blob read at revision) & `oz rev-parse`
// ============================================================================

/// Options controlling [`show_blob`].
#[derive(Debug, Clone, Default)]
pub struct ShowBlobOptions {
    /// Revision expression (commit, branch, tag, or OID). Defaults to `HEAD` when empty.
    pub rev: String,
    /// Repository-relative file path inside the revision's tree.
    pub path: String,
    /// Optional 1-based inclusive starting line number.
    pub start_line: Option<usize>,
    /// Optional 1-based inclusive ending line number.
    pub end_line: Option<usize>,
}

/// Structured blob view returned by [`show_blob`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowBlobReport {
    pub rev: String,
    pub commit_oid: String,
    pub short_oid: String,
    pub path: String,
    pub blob_oid: String,
    pub size_bytes: usize,
    pub is_binary: bool,
    pub total_lines: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub truncated: bool,
    pub content: String,
}

/// Parses a Git `<rev>:<path>` object specification.
/// Returns `None` for commit-message search syntax (`:/...`) or specs without a `:`.
#[must_use]
pub fn parse_rev_path_spec(spec: &str) -> Option<(String, String)> {
    let trimmed = spec.trim();
    if trimmed.starts_with(":/") {
        return None;
    }
    let (lhs, rhs) = trimmed.split_once(':')?;
    let norm_path = rhs
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();
    if norm_path.is_empty() {
        return None;
    }
    let rev = if lhs.is_empty() {
        "HEAD".to_string()
    } else {
        lhs.to_string()
    };
    Some((rev, norm_path))
}

/// Parses a 1-based inclusive line range (`300..380`, `300..=380`, `300-380`, `300:380`, `300`, `300..`, `..50`).
pub fn parse_line_range(raw: &str) -> Result<(Option<usize>, Option<usize>)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "empty line range".to_string(),
            recovery: vec![
                "Pass a 1-based range like `--lines 1..50` or `--lines 300-380`.".to_string(),
            ],
        });
    }

    let parse_num = |s: &str| -> Result<usize> {
        let n: usize = s.trim().parse().map_err(|_| OzError::InvalidUsage {
            message: format!("invalid line number '{s}' in line range '{raw}'"),
            recovery: vec!["Use positive 1-based integers like `--lines 1..50`.".to_string()],
        })?;
        if n == 0 {
            return Err(OzError::InvalidUsage {
                message: format!("line numbers are 1-based (got 0 in '{raw}')"),
                recovery: vec!["Start line ranges at 1, e.g. `--lines 1..50`.".to_string()],
            });
        }
        Ok(n)
    };

    let (start, end) = if let Some((a, b)) = trimmed.split_once("..=") {
        let s = if a.is_empty() {
            None
        } else {
            Some(parse_num(a)?)
        };
        let e = if b.is_empty() {
            None
        } else {
            Some(parse_num(b)?)
        };
        (s, e)
    } else if let Some((a, b)) = trimmed.split_once("..") {
        let s = if a.is_empty() {
            None
        } else {
            Some(parse_num(a)?)
        };
        let e = if b.is_empty() {
            None
        } else {
            Some(parse_num(b)?)
        };
        (s, e)
    } else if let Some((a, b)) = trimmed.split_once(':') {
        let s = if a.is_empty() {
            None
        } else {
            Some(parse_num(a)?)
        };
        let e = if b.is_empty() {
            None
        } else {
            Some(parse_num(b)?)
        };
        (s, e)
    } else if let Some((a, b)) = trimmed.split_once('-') {
        let s = if a.is_empty() {
            None
        } else {
            Some(parse_num(a)?)
        };
        let e = if b.is_empty() {
            None
        } else {
            Some(parse_num(b)?)
        };
        (s, e)
    } else {
        let n = parse_num(trimmed)?;
        (Some(n), Some(n))
    };

    if let (Some(s), Some(e)) = (start, end)
        && s > e
    {
        return Err(OzError::InvalidUsage {
            message: format!("invalid line range '{raw}': start line {s} > end line {e}"),
            recovery: vec!["Ensure start <= end (e.g. `--lines 10..40`).".to_string()],
        });
    }
    Ok((start, end))
}

/// Slices raw text bytes into a 1-based inclusive line range, returning
/// `(total_lines, start_line, end_line, truncated, sliced_content)`.
#[must_use]
pub fn slice_text_lines(
    bytes: &[u8],
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> (usize, usize, usize, bool, String) {
    let text = String::from_utf8_lossy(bytes);
    let all_lines: Vec<&str> = text.split_inclusive('\n').collect();
    let total_lines = all_lines.len();
    if total_lines == 0 {
        return (0, 0, 0, false, String::new());
    }
    let s = start_line.unwrap_or(1).max(1);
    if s > total_lines {
        return (total_lines, s, total_lines, false, String::new());
    }
    let e = end_line.unwrap_or(total_lines).min(total_lines);
    let truncated = s > 1 || e < total_lines;
    let content: String = all_lines[(s - 1)..e].concat();
    (total_lines, s, e, truncated, content)
}

/// Formats text content with 1-based line numbers starting at `start_line`.
#[must_use]
pub fn format_numbered_blob_lines(content: &str, start_line: usize) -> String {
    if content.is_empty() || start_line == 0 {
        return String::new();
    }
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let max_line = start_line + lines.len().saturating_sub(1);
    let width = max_line.to_string().len().max(4);
    let mut out = String::with_capacity(content.len() + lines.len() * (width + 4));
    for (idx, line) in lines.iter().enumerate() {
        let num = start_line + idx;
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{num:>width$} | {line}"));
        if !line.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Reads a single file blob from any commit/branch/tag/OID without mutating the working tree.
pub fn show_blob(repo: &OzoneRepo, opts: &ShowBlobOptions) -> Result<ShowBlobReport> {
    let gix_repo = open_gix_for_workspace(repo)?;
    let rev_label = if opts.rev.trim().is_empty() {
        "HEAD".to_string()
    } else {
        opts.rev.trim().to_string()
    };

    let commit_oid = if rev_label == "HEAD" {
        let head = resolve_workspace_head(repo, &gix_repo)?;
        head.commit_oid.ok_or_else(|| OzError::InvalidUsage {
            message: "repository has no commits yet (HEAD is unborn)".to_string(),
            recovery: vec!["Create an initial commit with `oz commit -m \"...\"`.".to_string()],
        })?
    } else {
        let id = gix_repo
            .rev_parse_single(rev_label.as_str())
            .map_err(|err| rev_parse_error(&rev_label, err))?
            .detach();
        gix_repo
            .find_object(id)
            .map_err(OzError::git_source)?
            .peel_to_commit()
            .map_err(|err| OzError::InvalidUsage {
                message: format!("revision '{rev_label}' does not point to a commit: {err}"),
                recovery: Vec::new(),
            })?
            .id
    };

    let tree_id = commit_tree_id(&gix_repo, commit_oid)?;
    let tree = gix_repo.find_tree(tree_id).map_err(OzError::git_source)?;

    let norm_path = opts
        .path
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();

    let entry = tree
        .lookup_entry_by_path(&norm_path)
        .map_err(OzError::git_source)?
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!("path '{norm_path}' does not exist in '{rev_label}'"),
            recovery: vec![format!(
                "Check the path at '{rev_label}' or run `oz show {rev_label} --stat`."
            )],
        })?;

    if entry.mode().is_tree() {
        return Err(OzError::InvalidUsage {
            message: format!("'{rev_label}:{norm_path}' is a directory, not a file blob"),
            recovery: vec![
                "Pass a file path inside that directory (e.g. `<rev>:dir/file.rs`).".to_string(),
            ],
        });
    }

    let blob_oid = entry.object_id();
    let blob = gix_repo.find_blob(blob_oid).map_err(OzError::git_source)?;
    let bytes = blob.data.as_slice();
    let size_bytes = bytes.len();
    let oid_hex = commit_oid.to_hex().to_string();
    let short_oid = oid_hex[..oid_hex.len().min(7)].to_string();

    if is_binary_bytes(bytes) {
        return Ok(ShowBlobReport {
            rev: rev_label,
            commit_oid: oid_hex,
            short_oid,
            path: norm_path,
            blob_oid: blob_oid.to_hex().to_string(),
            size_bytes,
            is_binary: true,
            total_lines: 0,
            start_line: 0,
            end_line: 0,
            truncated: false,
            content: String::new(),
        });
    }

    let (total_lines, start_line, end_line, truncated, content) =
        slice_text_lines(bytes, opts.start_line, opts.end_line);

    Ok(ShowBlobReport {
        rev: rev_label,
        commit_oid: oid_hex,
        short_oid,
        path: norm_path,
        blob_oid: blob_oid.to_hex().to_string(),
        size_bytes,
        is_binary: false,
        total_lines,
        start_line,
        end_line,
        truncated,
        content,
    })
}

/// Options controlling [`rev_parse`].
#[derive(Debug, Clone, Default)]
pub struct RevParseOptions {
    pub revs: Vec<String>,
    pub short: Option<Option<usize>>,
    pub verify: bool,
    pub abbrev_ref: bool,
    pub show_toplevel: bool,
    pub git_dir: bool,
    pub git_common_dir: bool,
    pub is_inside_work_tree: bool,
}

/// One resolved item in a [`RevParseReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevParseEntry {
    pub query: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_oid: Option<String>,
}

/// Structured output returned by [`rev_parse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevParseReport {
    pub values: Vec<String>,
    pub entries: Vec<RevParseEntry>,
}

/// Resolves revisions and repository metadata queries (`oz rev-parse`).
pub fn rev_parse(repo: &OzoneRepo, opts: &RevParseOptions) -> Result<RevParseReport> {
    let gix_repo = open_gix_for_workspace(repo)?;
    let mut entries = Vec::new();

    if opts.show_toplevel {
        entries.push(RevParseEntry {
            query: "--show-toplevel".to_string(),
            value: repo.repo_root.display().to_string(),
            full_oid: None,
        });
    }
    if opts.git_dir {
        entries.push(RevParseEntry {
            query: "--git-dir".to_string(),
            value: repo.git_dir.display().to_string(),
            full_oid: None,
        });
    }
    if opts.git_common_dir {
        entries.push(RevParseEntry {
            query: "--git-common-dir".to_string(),
            value: repo.git_common_dir.display().to_string(),
            full_oid: None,
        });
    }
    if opts.is_inside_work_tree {
        entries.push(RevParseEntry {
            query: "--is-inside-work-tree".to_string(),
            value: "true".to_string(),
            full_oid: None,
        });
    }

    if opts.verify && opts.revs.len() != 1 {
        return Err(OzError::InvalidUsage {
            message: "`--verify` requires a single revision argument".to_string(),
            recovery: vec!["Run `oz rev-parse --verify HEAD`.".to_string()],
        });
    }

    let short_len = opts.short.map(|opt_len| opt_len.unwrap_or(7).clamp(4, 40));

    for rev in &opts.revs {
        if opts.abbrev_ref {
            let value = if rev == "HEAD" {
                repo.branch.clone().unwrap_or_else(|| "HEAD".to_string())
            } else if let Ok(r) = gix_repo.find_reference(rev.as_str()) {
                let full = r.name().as_bstr().to_str_lossy().into_owned();
                full.strip_prefix("refs/heads/")
                    .or_else(|| full.strip_prefix("refs/remotes/"))
                    .or_else(|| full.strip_prefix("refs/tags/"))
                    .unwrap_or(&full)
                    .to_string()
            } else {
                return Err(OzError::InvalidUsage {
                    message: format!("cannot resolve ref '{rev}' for `--abbrev-ref`"),
                    recovery: vec!["Run `oz branch` to list available refs.".to_string()],
                });
            };
            entries.push(RevParseEntry {
                query: rev.clone(),
                value,
                full_oid: None,
            });
            continue;
        }

        let oid = if rev == "HEAD" {
            let head = resolve_workspace_head(repo, &gix_repo)?;
            head.commit_oid.ok_or_else(|| OzError::InvalidUsage {
                message: "cannot resolve 'HEAD': repository has no commits yet".to_string(),
                recovery: vec!["Create an initial commit with `oz commit -m \"...\"`.".to_string()],
            })?
        } else {
            gix_repo
                .rev_parse_single(rev.as_str())
                .map_err(|err| OzError::InvalidUsage {
                    message: format!("cannot resolve revision '{rev}': {err}"),
                    recovery: vec![
                        "Run `oz branch` or `oz log --oneline` to inspect refs.".to_string(),
                    ],
                })?
                .detach()
        };
        let full_hex = oid.to_hex().to_string();
        let value = match short_len {
            Some(n) => full_hex[..full_hex.len().min(n)].to_string(),
            None => full_hex.clone(),
        };
        entries.push(RevParseEntry {
            query: rev.clone(),
            value,
            full_oid: Some(full_hex),
        });
    }

    let values = entries.iter().map(|e| e.value.clone()).collect();
    Ok(RevParseReport { values, entries })
}

struct RawTreeChange {
    path: String,
    kind: ChangeKind,
    old_bytes: Vec<u8>,
    new_bytes: Vec<u8>,
}

fn diff_commit_trees(
    gix_repo: &gix::Repository,
    lhs_tree_id: ObjectId,
    rhs_tree_id: ObjectId,
) -> Result<Vec<FileChange>> {
    if lhs_tree_id == rhs_tree_id {
        return Ok(Vec::new());
    }
    let lhs_tree = gix_repo
        .find_tree(lhs_tree_id)
        .map_err(OzError::git_source)?;
    let rhs_tree = gix_repo
        .find_tree(rhs_tree_id)
        .map_err(OzError::git_source)?;

    let mut state = gix::diff::tree::State::default();
    let mut recorder = gix::diff::tree::Recorder::default()
        .track_location(Some(gix::diff::tree::recorder::Location::Path));
    let lhs_iter = gix::objs::TreeRefIter::from_bytes(&lhs_tree.data, lhs_tree.id.kind());
    let rhs_iter = gix::objs::TreeRefIter::from_bytes(&rhs_tree.data, rhs_tree.id.kind());
    gix::diff::tree(
        lhs_iter,
        rhs_iter,
        &mut state,
        &gix_repo.objects,
        &mut recorder,
    )
    .map_err(OzError::git_source)?;

    let mut raw_changes = Vec::new();
    for record in recorder.records {
        match record {
            gix::diff::tree::recorder::Change::Addition {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                let new_bytes = if entry_mode.kind() == EntryKind::Commit {
                    format!("Subproject commit {}\n", oid.to_hex()).into_bytes()
                } else {
                    read_blob_bytes(gix_repo, oid)?
                };
                raw_changes.push(RawTreeChange {
                    path: path.to_str_lossy().into_owned(),
                    kind: ChangeKind::Added,
                    old_bytes: Vec::new(),
                    new_bytes,
                });
            }
            gix::diff::tree::recorder::Change::Deletion {
                entry_mode,
                oid,
                path,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                let old_bytes = if entry_mode.kind() == EntryKind::Commit {
                    format!("Subproject commit {}\n", oid.to_hex()).into_bytes()
                } else {
                    read_blob_bytes(gix_repo, oid)?
                };
                raw_changes.push(RawTreeChange {
                    path: path.to_str_lossy().into_owned(),
                    kind: ChangeKind::Deleted,
                    old_bytes,
                    new_bytes: Vec::new(),
                });
            }
            gix::diff::tree::recorder::Change::Modification {
                previous_entry_mode,
                previous_oid,
                entry_mode,
                oid,
                path,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                let old_bytes = if previous_entry_mode.is_tree() {
                    Vec::new()
                } else if previous_entry_mode.kind() == EntryKind::Commit {
                    format!("Subproject commit {}\n", previous_oid.to_hex()).into_bytes()
                } else {
                    read_blob_bytes(gix_repo, previous_oid)?
                };
                let new_bytes = if entry_mode.kind() == EntryKind::Commit {
                    format!("Subproject commit {}\n", oid.to_hex()).into_bytes()
                } else {
                    read_blob_bytes(gix_repo, oid)?
                };
                raw_changes.push(RawTreeChange {
                    path: path.to_str_lossy().into_owned(),
                    kind: ChangeKind::Modified,
                    old_bytes,
                    new_bytes,
                });
            }
        }
    }

    raw_changes.sort_by(|a, b| a.path.cmp(&b.path));
    let mut out = Vec::with_capacity(raw_changes.len());
    for (idx, item) in raw_changes.into_iter().enumerate() {
        let num = idx + 1;
        let fid = file_id(num);
        let diff = compute_file_diff(&fid, num, &item.path, &item.old_bytes, &item.new_bytes);
        out.push(FileChange {
            id: fid,
            path: item.path,
            old_path: None,
            kind: item.kind,
            additions: diff.additions,
            deletions: diff.deletions,
            is_binary: diff.is_binary,
            pinned: false,
            out_of_scope: false,
            hunks: diff.hunks,
        });
    }
    Ok(out)
}

/// Options controlling [`compute_revision_diff`].
#[derive(Debug, Clone, Default)]
pub struct RevisionDiffOptions {
    /// Optional base revision (`--base <REV>`).
    pub base: Option<String>,
    /// Optional target revision (`--rev <REV>`, defaults to `HEAD` when only `base` is given).
    pub rev: Option<String>,
    /// Optional Git range expression (`A..B`, `A...B`, or single `<rev>`).
    pub range: Option<String>,
    /// Optional path / directory / file-ID / hunk-ID filters (including `:!<path>` exclusions).
    pub targets: Vec<String>,
    /// Optional explicit exclusions (`--exclude <path|glob|f1|h1a>`).
    pub excludes: Vec<String>,
}

/// Computes Histogram diffs between two commits/revisions (`A..B`, `A...B`, or `--base`/`--rev`),
/// optionally filtered by `targets`.
pub fn compute_revision_diff(repo: &OzoneRepo, opts: &RevisionDiffOptions) -> Result<DiffReport> {
    let gix_repo = open_gix_for_workspace(repo)?;
    let empty_tree_id = ObjectId::empty_tree(gix_repo.object_hash());

    let (lhs_commit_oid, rhs_commit_oid, use_merge_base) = if let Some(ref raw_range) = opts.range {
        let trimmed = raw_range.trim();
        if let Some((lhs_raw, rhs_raw)) = trimmed.split_once("...") {
            let lhs_spec = if lhs_raw.trim().is_empty() {
                "HEAD"
            } else {
                lhs_raw.trim()
            };
            let rhs_spec = if rhs_raw.trim().is_empty() {
                "HEAD"
            } else {
                rhs_raw.trim()
            };
            (
                resolve_commit_oid(&gix_repo, lhs_spec)?,
                resolve_commit_oid(&gix_repo, rhs_spec)?,
                true,
            )
        } else if let Some((lhs_raw, rhs_raw)) = trimmed.split_once("..") {
            let lhs_spec = if lhs_raw.trim().is_empty() {
                "HEAD"
            } else {
                lhs_raw.trim()
            };
            let rhs_spec = if rhs_raw.trim().is_empty() {
                "HEAD"
            } else {
                rhs_raw.trim()
            };
            (
                resolve_commit_oid(&gix_repo, lhs_spec)?,
                resolve_commit_oid(&gix_repo, rhs_spec)?,
                false,
            )
        } else {
            let lhs = resolve_commit_oid(&gix_repo, trimmed)?;
            let head = resolve_commit_oid(&gix_repo, "HEAD")?;
            (lhs, head, false)
        }
    } else {
        let lhs_spec = opts
            .base
            .as_deref()
            .or(opts.rev.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("HEAD");
        let rhs_spec = match (opts.base.as_deref(), opts.rev.as_deref()) {
            (Some(_), Some(r)) if !r.trim().is_empty() => r.trim(),
            _ => "HEAD",
        };
        (
            resolve_commit_oid(&gix_repo, lhs_spec)?,
            resolve_commit_oid(&gix_repo, rhs_spec)?,
            false,
        )
    };

    let effective_base_oid = if use_merge_base {
        gix_repo
            .merge_base(lhs_commit_oid, rhs_commit_oid)
            .ok()
            .flatten()
            .map(gix::Id::detach)
    } else {
        Some(lhs_commit_oid)
    };

    let lhs_tree = match effective_base_oid {
        Some(oid) => commit_tree_id(&gix_repo, oid)?,
        None => empty_tree_id,
    };
    let rhs_tree = commit_tree_id(&gix_repo, rhs_commit_oid)?;

    let all_files = diff_commit_trees(&gix_repo, lhs_tree, rhs_tree)?;
    let files = if opts.targets.is_empty() && opts.excludes.is_empty() {
        all_files
    } else {
        filter_changes_by_targets_and_excludes(&all_files, &opts.targets, &opts.excludes)?
    };

    let total_additions = files.iter().map(|f| f.additions).sum();
    let total_deletions = files.iter().map(|f| f.deletions).sum();

    Ok(DiffReport {
        workspace: repo.workspace.clone(),
        branch: repo.branch.clone(),
        head_oid: repo.head_oid.clone(),
        base_oid: Some(
            effective_base_oid
                .unwrap_or(empty_tree_id)
                .to_hex()
                .to_string(),
        ),
        target_oid: Some(rhs_commit_oid.to_hex().to_string()),
        files,
        total_additions,
        total_deletions,
    })
}

// ============================================================================
// 3. `oz stash` (`push` / `pop` / `list` / `drop`)
// ============================================================================

/// Stored metadata for one stashed working-tree snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashEntryRecord {
    pub id: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    pub base_tree_oid: String,
    pub dirty_tree_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StashManifest {
    schema_version: u32,
    entries: Vec<StashEntryRecord>,
}

/// Public stash summary with `stash@{N}` index label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashItem {
    pub index: usize,
    pub ref_label: String,
    pub id: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    pub base_tree_oid: String,
    pub dirty_tree_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub files_count: usize,
    pub paths: Vec<String>,
    pub created_at: DateTime<Utc>,
}

/// Outcome returned by [`stash_push`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashPushOutcome {
    pub stash: StashItem,
    pub cleaned_paths: Vec<String>,
}

/// Outcome returned by [`apply_stash_pop`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashPopOutcome {
    pub stash: StashItem,
    pub applied_paths: Vec<String>,
}

/// Outcome returned by [`stash_drop`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashDropOutcome {
    pub dropped: StashItem,
    pub remaining_count: usize,
}

/// Outcome returned by [`stash_list`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashListReport {
    pub count: usize,
    pub stashes: Vec<StashItem>,
}

/// Prepared in-memory merge result before applying `oz stash pop`.
pub struct PreparedStashPop {
    index: usize,
    record: StashEntryRecord,
    delta: Vec<TreeDeltaOp>,
}

/// Parses a stash selector such as `"stash@{0}"` or `"0"` (defaulting to `0`).
pub fn parse_stash_selector(spec: Option<&str>) -> Result<usize> {
    let Some(raw) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(0);
    };
    if let Some(inner) = raw
        .strip_prefix("stash@{")
        .and_then(|s| s.strip_suffix('}'))
    {
        return inner
            .trim()
            .parse::<usize>()
            .map_err(|_| OzError::InvalidUsage {
                message: format!("invalid stash reference '{raw}'"),
                recovery: vec!["Run `oz stash list` to see available stashes.".to_string()],
            });
    }
    raw.parse::<usize>().map_err(|_| OzError::InvalidUsage {
        message: format!("invalid stash reference '{raw}'"),
        recovery: vec!["Pass `stash@{0}` or a numeric index like `0`.".to_string()],
    })
}

fn read_stash_manifest(
    gix_repo: &gix::Repository,
) -> Result<(Option<ObjectId>, Vec<StashEntryRecord>)> {
    let Some(mut reference) = gix_repo
        .try_find_reference(OZ_STASH_REF)
        .map_err(OzError::git_source)?
    else {
        return Ok((None, Vec::new()));
    };
    let commit = reference.peel_to_commit().map_err(OzError::git_source)?;
    let tree = commit.tree().map_err(OzError::git_source)?;
    let Some(entry) = tree
        .lookup_entry_by_path("manifest.json")
        .map_err(OzError::git_source)?
    else {
        return Ok((Some(commit.id), Vec::new()));
    };
    let blob = gix_repo
        .find_blob(entry.object_id())
        .map_err(OzError::git_source)?;
    let manifest: StashManifest = serde_json::from_slice(&blob.data)?;
    Ok((Some(commit.id), manifest.entries))
}

fn write_stash_manifest(
    gix_repo: &mut gix::Repository,
    prev_commit: Option<ObjectId>,
    entries: &[StashEntryRecord],
    reflog_msg: &str,
) -> Result<()> {
    let stash_ref_full = FullName::try_from(OZ_STASH_REF)
        .map_err(|err| OzError::Git(format!("invalid stash ref: {err}")))?;

    if entries.is_empty() {
        let Some(prev) = prev_commit else {
            return Ok(());
        };
        gix_repo
            .edit_references([RefEdit::delete(
                stash_ref_full,
                PreviousValue::MustExistAndMatch(Target::Object(prev)),
            )])
            .map_err(OzError::git_source)?;
        return Ok(());
    }

    let empty_tree = ObjectId::empty_tree(gix_repo.object_hash());
    let manifest = StashManifest {
        schema_version: STASH_SCHEMA_VERSION,
        entries: entries.to_vec(),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let manifest_blob = gix_repo
        .write_blob(&manifest_bytes)
        .map_err(OzError::git_source)?
        .detach();

    let mut editor = gix_repo
        .edit_tree(empty_tree)
        .map_err(OzError::git_source)?;
    editor
        .upsert("manifest.json", EntryKind::Blob, manifest_blob)
        .map_err(OzError::git_source)?;

    for rec in entries {
        let dirty_oid = parse_oid(&rec.dirty_tree_oid)?;
        if dirty_oid != empty_tree {
            let p = format!("snapshots/{}/dirty", rec.id);
            editor
                .upsert(p.as_str(), EntryKind::Tree, dirty_oid)
                .map_err(OzError::git_source)?;
        }
        let base_oid = parse_oid(&rec.base_tree_oid)?;
        if base_oid != empty_tree {
            let p = format!("snapshots/{}/base", rec.id);
            editor
                .upsert(p.as_str(), EntryKind::Tree, base_oid)
                .map_err(OzError::git_source)?;
        }
    }

    let tree_id = editor.write().map_err(OzError::git_source)?.detach();
    let (author, committer) = resolve_actor_signatures(gix_repo)?;
    let mut author_buf = gix::date::parse::TimeBuf::default();
    let mut committer_buf = gix::date::parse::TimeBuf::default();
    let parents: Vec<ObjectId> = prev_commit.into_iter().collect();
    let new_commit = gix_repo
        .new_commit_as(
            committer.to_ref(&mut committer_buf),
            author.to_ref(&mut author_buf),
            reflog_msg,
            tree_id,
            parents,
        )
        .map_err(OzError::git_source)?;

    let expected = match prev_commit {
        Some(prev) => PreviousValue::MustExistAndMatch(Target::Object(prev)),
        None => PreviousValue::MustNotExist,
    };
    gix_repo
        .edit_references([RefEdit::update(
            stash_ref_full,
            new_commit.id,
            expected,
            reflog_msg,
        )])
        .map_err(OzError::git_source)?;
    Ok(())
}

fn to_stash_item(index: usize, rec: &StashEntryRecord) -> StashItem {
    StashItem {
        index,
        ref_label: format!("stash@{{{index}}}"),
        id: rec.id.clone(),
        message: rec.message.clone(),
        branch: rec.branch.clone(),
        head_oid: rec.head_oid.clone(),
        base_tree_oid: rec.base_tree_oid.clone(),
        dirty_tree_oid: rec.dirty_tree_oid.clone(),
        workspace: rec.workspace.clone(),
        files_count: rec.paths.len(),
        paths: rec.paths.clone(),
        created_at: rec.created_at,
    }
}

fn into_stash_item(index: usize, rec: StashEntryRecord) -> StashItem {
    let files_count = rec.paths.len();
    StashItem {
        index,
        ref_label: format!("stash@{{{index}}}"),
        id: rec.id,
        message: rec.message,
        branch: rec.branch,
        head_oid: rec.head_oid,
        base_tree_oid: rec.base_tree_oid,
        dirty_tree_oid: rec.dirty_tree_oid,
        workspace: rec.workspace,
        files_count,
        paths: rec.paths,
        created_at: rec.created_at,
    }
}

fn build_dirty_tree_from_workdir(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
) -> Result<(crate::status::WorkspaceHead, ObjectId, Vec<String>)> {
    let ctx = compute_status_context(repo)?;
    let mut editor = gix_repo
        .edit_tree(ctx.head.tree_id)
        .map_err(OzError::git_source)?;
    let mut paths = Vec::with_capacity(ctx.dirty_files.len());

    for raw in &ctx.dirty_files {
        paths.push(raw.path.clone());
        match raw.kind {
            ChangeKind::Deleted => {
                editor
                    .remove(raw.path.as_str())
                    .map_err(OzError::git_source)?;
            }
            ChangeKind::Conflicted => {
                return Err(OzError::Conflict {
                    message: format!("cannot stash conflicted path '{}'", raw.path),
                    conflicts: Vec::new(),
                    recovery: vec!["Resolve conflicts first.".to_string()],
                });
            }
            ChangeKind::Added
            | ChangeKind::Modified
            | ChangeKind::Untracked
            | ChangeKind::Renamed
            | ChangeKind::Copied
            | ChangeKind::TypeChanged => {
                let disk_path = ctx.source.disk_path(&raw.path);
                let meta = fs::symlink_metadata(&disk_path)?;
                let (bytes, entry_kind) = if meta.file_type().is_symlink() {
                    let target = fs::read_link(&disk_path)?;
                    #[cfg(unix)]
                    let b = {
                        use std::os::unix::ffi::OsStrExt;
                        target.as_os_str().as_bytes().to_vec()
                    };
                    #[cfg(not(unix))]
                    let b = target.to_string_lossy().as_bytes().to_vec();
                    (b, EntryKind::Link)
                } else {
                    let b = fs::read(&disk_path)?;
                    #[cfg(unix)]
                    let k = {
                        use std::os::unix::fs::PermissionsExt;
                        if meta.permissions().mode() & 0o111 != 0 {
                            EntryKind::BlobExecutable
                        } else {
                            EntryKind::Blob
                        }
                    };
                    #[cfg(not(unix))]
                    let k = EntryKind::Blob;
                    (b, k)
                };
                let blob_id = gix_repo
                    .write_blob(&bytes)
                    .map_err(OzError::git_source)?
                    .detach();
                editor
                    .upsert(raw.path.as_str(), entry_kind, blob_id)
                    .map_err(OzError::git_source)?;
            }
        }
    }

    let dirty_tree_id = editor.write().map_err(OzError::git_source)?.detach();
    Ok((ctx.head, dirty_tree_id, paths))
}

/// Saves current dirty working-tree changes into `refs/ozone/stash` and restores the working tree
/// cleanly back to `HEAD`'s tree.
pub fn stash_push(repo: &OzoneRepo, message: Option<&str>) -> Result<StashPushOutcome> {
    let mut gix_repo = open_gix_for_workspace(repo)?;
    let (head, dirty_tree_id, paths) = build_dirty_tree_from_workdir(repo, &gix_repo)?;
    if paths.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "no local changes to save".to_string(),
            recovery: vec!["Run `oz status` to check dirty files.".to_string()],
        });
    }

    let default_msg = match (head.branch.as_deref(), head.commit_oid) {
        (Some(br), Some(cid)) => {
            let short = cid.to_hex().to_string();
            let subj = gix_repo
                .find_commit(cid)
                .ok()
                .and_then(|c| {
                    c.decode()
                        .ok()
                        .map(|d| split_subject_and_body(&d.message.to_str_lossy()).0)
                })
                .unwrap_or_default();
            format!("WIP on {br}: {} {subj}", &short[..short.len().min(7)])
        }
        (Some(br), None) => format!("WIP on {br}: (initial)"),
        (None, Some(cid)) => {
            let short = cid.to_hex().to_string();
            format!("WIP on HEAD: {}", &short[..short.len().min(7)])
        }
        (None, None) => "WIP on unborn HEAD".to_string(),
    };
    let final_msg = message
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&default_msg)
        .to_string();

    let (prev_commit, mut entries) = read_stash_manifest(&gix_repo)?;
    let rec = StashEntryRecord {
        id: format!("st_{}", ulid::Ulid::generate()),
        message: final_msg,
        branch: head.branch,
        head_oid: head.commit_oid.map(|id| id.to_hex().to_string()),
        base_tree_oid: head.tree_id.to_hex().to_string(),
        dirty_tree_oid: dirty_tree_id.to_hex().to_string(),
        workspace: repo.workspace.clone(),
        paths: paths.clone(),
        created_at: Utc::now(),
    };

    entries.insert(0, rec.clone());
    write_stash_manifest(&mut gix_repo, prev_commit, &entries, "oz stash push")?;

    // Clean working tree from dirty_tree back to head.tree_id.
    let delta = compute_tree_delta(&gix_repo, dirty_tree_id, head.tree_id)?;
    materialize_tree_delta(repo, &gix_repo, &delta)?;
    sync_index_for_delta(repo, &gix_repo, head.tree_id, &delta)?;
    let _ = crate::commit::clear_pinned_paths(repo);

    Ok(StashPushOutcome {
        stash: to_stash_item(0, &rec),
        cleaned_paths: paths,
    })
}

/// Lists all stashed snapshots in `refs/ozone/stash` (`stash@{0}`, `stash@{1}`, ...).
pub fn stash_list(repo: &OzoneRepo) -> Result<StashListReport> {
    let gix_repo = open_gix_for_workspace(repo)?;
    let (_, entries) = read_stash_manifest(&gix_repo)?;
    let stashes: Vec<StashItem> = entries
        .iter()
        .enumerate()
        .map(|(idx, rec)| to_stash_item(idx, rec))
        .collect();
    Ok(StashListReport {
        count: stashes.len(),
        stashes,
    })
}

/// Performs read-only 3-way merge validation for `oz stash pop` before opening an `OpLog` transaction.
pub fn prepare_stash_pop(repo: &OzoneRepo, selector: Option<&str>) -> Result<PreparedStashPop> {
    let index = parse_stash_selector(selector)?;
    let gix_repo = open_gix_for_workspace(repo)?;
    let (_, entries) = read_stash_manifest(&gix_repo)?;
    let rec = entries
        .get(index)
        .cloned()
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!("stash@{{{index}}} does not exist"),
            recovery: vec!["Run `oz stash list` to see available stashes.".to_string()],
        })?;

    let (_, live_tree_id, _) = build_dirty_tree_from_workdir(repo, &gix_repo)?;
    let stash_base = parse_oid(&rec.base_tree_oid)?;
    let stash_dirty = parse_oid(&rec.dirty_tree_oid)?;

    let merge_opts = gix_repo.tree_merge_options().map_err(OzError::git_source)?;
    let stash_label = format!("stash@{{{index}}}");
    let labels = gix::merge::blob::builtin_driver::text::Labels {
        ancestor: Some("stash-base".into()),
        current: Some("working-tree".into()),
        other: Some(stash_label.as_str().into()),
    };
    let mut outcome = gix_repo
        .merge_trees(stash_base, live_tree_id, stash_dirty, labels, merge_opts)
        .map_err(OzError::git_source)?;

    if outcome.has_unresolved_conflicts(gix::merge::tree::TreatAsUnresolved::git()) {
        let conflicts = extract_tree_merge_conflicts(&outcome.conflicts);
        return Err(OzError::Conflict {
            message: format!(
                "{} conflict(s) applying stash@{{{index}}}; stash was preserved",
                conflicts.len()
            ),
            conflicts,
            recovery: vec![
                "Commit or stash conflicting local edits first, then retry `oz stash pop`."
                    .to_string(),
            ],
        });
    }

    let merged_tree_id = outcome.tree.write().map_err(OzError::git_source)?.detach();
    let delta = compute_tree_delta(&gix_repo, live_tree_id, merged_tree_id)?;

    Ok(PreparedStashPop {
        index,
        record: rec,
        delta,
    })
}

/// Applies a validated [`PreparedStashPop`] to the working tree and pops the entry from `refs/ozone/stash`.
pub fn apply_stash_pop(repo: &OzoneRepo, prepared: PreparedStashPop) -> Result<StashPopOutcome> {
    let PreparedStashPop {
        index,
        record,
        delta,
    } = prepared;
    let mut gix_repo = open_gix_for_workspace(repo)?;
    materialize_tree_delta(repo, &gix_repo, &delta)?;

    let applied_paths: Vec<String> = delta.into_iter().map(TreeDeltaOp::into_path).collect();
    let head = resolve_workspace_head(repo, &gix_repo)?;
    sync_index_paths_to_tree(repo, &gix_repo, head.tree_id, &applied_paths)?;

    let (prev_commit, mut entries) = read_stash_manifest(&gix_repo)?;
    if index < entries.len() && entries[index].id == record.id {
        entries.remove(index);
    } else if let Some(pos) = entries.iter().position(|e| e.id == record.id) {
        entries.remove(pos);
    }
    write_stash_manifest(&mut gix_repo, prev_commit, &entries, "oz stash pop")?;

    Ok(StashPopOutcome {
        stash: into_stash_item(index, record),
        applied_paths,
    })
}

/// Removes a stash entry from `refs/ozone/stash` without applying it.
pub fn stash_drop(repo: &OzoneRepo, selector: Option<&str>) -> Result<StashDropOutcome> {
    let index = parse_stash_selector(selector)?;
    let mut gix_repo = open_gix_for_workspace(repo)?;
    let (prev_commit, mut entries) = read_stash_manifest(&gix_repo)?;
    if index >= entries.len() {
        return Err(OzError::InvalidUsage {
            message: format!("stash@{{{index}}} does not exist"),
            recovery: vec!["Run `oz stash list` to inspect available stashes.".to_string()],
        });
    }
    let dropped_rec = entries.remove(index);
    write_stash_manifest(&mut gix_repo, prev_commit, &entries, "oz stash drop")?;
    Ok(StashDropOutcome {
        dropped: to_stash_item(index, &dropped_rec),
        remaining_count: entries.len(),
    })
}

fn extract_tree_merge_conflicts(
    conflicts: &[gix::merge::tree::Conflict],
) -> Vec<crate::envelope::ConflictEntry> {
    conflicts
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let path = c.theirs.location().to_str_lossy().into_owned();
            crate::envelope::ConflictEntry {
                id: format!("c{}", i + 1),
                path,
                message: format!("merge conflict ({:?})", c.resolution),
                base: None,
                ours: None,
                theirs: None,
                suggested_commands: Vec::new(),
            }
        })
        .collect()
}

// ============================================================================
// 4. `oz cherry-pick <rev>` & `oz revert <rev>`
// ============================================================================

/// Options controlling [`cherry_pick_commit`].
#[derive(Debug, Clone, Default)]
pub struct CherryPickOptions {
    pub rev: String,
    pub mainline: Option<usize>,
    pub no_commit: bool,
    pub append_origin: bool,
    pub sign: Option<bool>,
    pub signing_key: Option<String>,
}

/// Result returned by [`cherry_pick_commit`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CherryPickOutcome {
    pub source_commit_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_oid: Option<String>,
    pub tree_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    pub message: String,
    pub applied_paths: Vec<String>,
    pub no_commit: bool,
    pub signed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_fingerprint: Option<String>,
}

/// Options controlling [`revert_commit`].
#[derive(Debug, Clone, Default)]
pub struct RevertOptions {
    pub rev: String,
    pub mainline: Option<usize>,
    pub no_commit: bool,
    pub sign: Option<bool>,
    pub signing_key: Option<String>,
}

/// Result returned by [`revert_commit`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevertOutcome {
    pub reverted_commit_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_oid: Option<String>,
    pub tree_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    pub message: String,
    pub applied_paths: Vec<String>,
    pub no_commit: bool,
    pub signed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_fingerprint: Option<String>,
}

fn resolve_commit_parent_tree(
    gix_repo: &gix::Repository,
    commit: &gix::Commit<'_>,
    mainline: Option<usize>,
) -> Result<ObjectId> {
    let parents: Vec<ObjectId> = commit.parent_ids().map(gix::Id::detach).collect();
    if parents.is_empty() {
        return Ok(ObjectId::empty_tree(gix_repo.object_hash()));
    }
    if parents.len() > 1 {
        let Some(m) = mainline else {
            return Err(OzError::InvalidUsage {
                message: format!(
                    "commit {} is a merge commit ({} parents); pass `-m <1..{}>`",
                    commit.id,
                    parents.len(),
                    parents.len()
                ),
                recovery: vec!["Pass `-m 1` to select the first parent.".to_string()],
            });
        };
        if m == 0 || m > parents.len() {
            return Err(OzError::InvalidUsage {
                message: format!(
                    "mainline parent {m} out of range (commit {} has {} parents)",
                    commit.id,
                    parents.len()
                ),
                recovery: Vec::new(),
            });
        }
        return commit_tree_id(gix_repo, parents[m - 1]);
    }
    if let Some(m) = mainline
        && m != 1
    {
        return Err(OzError::InvalidUsage {
            message: format!(
                "mainline parent {m} invalid for non-merge commit {}",
                commit.id
            ),
            recovery: Vec::new(),
        });
    }
    commit_tree_id(gix_repo, parents[0])
}

fn check_dirty_collision(repo: &OzoneRepo, delta: &[TreeDeltaOp], action: &str) -> Result<()> {
    if delta.is_empty() {
        return Ok(());
    }
    let ctx = compute_status_context(repo)?;
    let mut colliding = Vec::new();
    for dirty in &ctx.dirty_files {
        for op in delta {
            if paths_collide(&dirty.path, op.path()) {
                colliding.push(dirty.path.clone());
                break;
            }
        }
    }
    if !colliding.is_empty() {
        return Err(OzError::Conflict {
            message: format!(
                "local uncommitted changes to {} would be overwritten by {action}",
                colliding.join(", ")
            ),
            conflicts: colliding
                .iter()
                .enumerate()
                .map(|(i, p)| crate::envelope::ConflictEntry {
                    id: format!("c{}", i + 1),
                    path: p.clone(),
                    message: format!("uncommitted local change collides with {action}"),
                    base: None,
                    ours: None,
                    theirs: None,
                    suggested_commands: vec![
                        "oz stash push".to_string(),
                        "oz commit -m \"wip\"".to_string(),
                    ],
                })
                .collect(),
            recovery: vec!["Run `oz stash push` or commit local changes first.".to_string()],
        });
    }
    Ok(())
}

struct CommitWriteSpec {
    tree_id: ObjectId,
    parents: Vec<ObjectId>,
    author: gix::actor::Signature,
    committer: gix::actor::Signature,
    message: String,
    target_ref: String,
    expected_head: Option<ObjectId>,
    reflog_op: &'static str,
    sign: Option<bool>,
    signing_key: Option<String>,
}

fn write_and_advance_commit(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    spec: CommitWriteSpec,
) -> Result<(ObjectId, bool, Option<String>)> {
    let should_sign = resolve_should_sign(repo, gix_repo, spec.sign);
    let active_key = if should_sign {
        let key_spec = resolve_signing_key_spec(repo, gix_repo, spec.signing_key.as_deref());
        let (priv_key, _) = resolve_signing_private_key(&key_spec, Some(&repo.repo_root))?;
        Some(priv_key)
    } else {
        None
    };

    let mut commit_obj = gix::objs::Commit {
        message: spec.message.as_bytes().into(),
        tree: spec.tree_id,
        author: spec.author,
        committer: spec.committer.clone(),
        encoding: None,
        parents: spec.parents.into_iter().collect(),
        extra_headers: Vec::new(),
    };

    let (signed, fingerprint) = if let Some(ref priv_key) = active_key {
        let mut unsigned_payload =
            Vec::with_capacity(usize::try_from(commit_obj.size()).unwrap_or(0));
        commit_obj
            .write_to(&mut unsigned_payload)
            .map_err(OzError::git_source)?;
        let armored = priv_key.sign_sshsig(&unsigned_payload, GIT_SSHSIG_NAMESPACE)?;
        let fp = priv_key.public_key().fingerprint_sha256();
        let sig_field = gix::objs::commit::signature_field_name(gix_repo.object_hash());
        commit_obj
            .extra_headers
            .push((sig_field.into(), armored.into()));
        (true, Some(fp))
    } else {
        (false, None)
    };

    let commit_id = gix_repo
        .write_object(&commit_obj)
        .map_err(OzError::git_source)?
        .detach();

    let ref_full_name =
        FullName::try_from(spec.target_ref.as_str()).map_err(OzError::git_source)?;
    let expected = match spec.expected_head.map(Target::Object) {
        Some(prev) => {
            if ref_full_name.as_bstr() == "HEAD" {
                PreviousValue::MustExistAndMatch(prev)
            } else {
                PreviousValue::ExistingMustMatch(prev)
            }
        }
        None => PreviousValue::MustNotExist,
    };
    let log_msg = gix::reference::log::message(
        spec.reflog_op,
        commit_obj.message.as_bstr(),
        commit_obj.parents.len(),
    );
    let mut committer_buf = gix::date::parse::TimeBuf::default();
    let committer_ref = spec.committer.to_ref(&mut committer_buf);
    gix_repo
        .edit_references_as(
            Some(RefEdit {
                change: Change::Update {
                    log: LogChange {
                        mode: RefLog::AndReference,
                        force_create_reflog: false,
                        message: log_msg,
                    },
                    expected,
                    new: Target::Object(commit_id),
                },
                name: ref_full_name,
                deref: true,
            }),
            Some(committer_ref),
        )
        .map_err(OzError::git_source)?;

    Ok((commit_id, signed, fingerprint))
}

/// Applies the patch introduced by `opts.rev` onto current `HEAD` via pure-Rust 3-way tree merge.
pub fn cherry_pick_commit(repo: &OzoneRepo, opts: &CherryPickOptions) -> Result<CherryPickOutcome> {
    let mut gix_repo = open_gix_for_workspace(repo)?;
    let source_oid = resolve_commit_oid(&gix_repo, &opts.rev)?;
    let (source_tree, parent_tree, orig_author, orig_message) = {
        let source_commit = gix_repo
            .find_commit(source_oid)
            .map_err(OzError::git_source)?;
        let source_tree = source_commit
            .tree_id()
            .map_err(OzError::git_source)?
            .detach();
        let parent_tree = resolve_commit_parent_tree(&gix_repo, &source_commit, opts.mainline)?;

        let decoded = source_commit.decode().map_err(OzError::git_source)?;
        let orig_author = decoded
            .author()
            .map_err(OzError::git_source)?
            .to_owned()
            .map_err(OzError::git_source)?;
        let orig_message = decoded.message.to_str_lossy().into_owned();
        (source_tree, parent_tree, orig_author, orig_message)
    };

    let head = resolve_workspace_head(repo, &gix_repo)?;
    let head_tree = head.tree_id;
    let merge_opts = gix_repo.tree_merge_options().map_err(OzError::git_source)?;
    let short_src = source_oid.to_hex().to_string();
    let short_src_7 = &short_src[..short_src.len().min(7)];
    let labels = gix::merge::blob::builtin_driver::text::Labels {
        ancestor: Some("parent".into()),
        current: Some(head.branch.as_deref().unwrap_or("HEAD").into()),
        other: Some(short_src_7.into()),
    };

    let mut outcome = gix_repo
        .merge_trees(parent_tree, head_tree, source_tree, labels, merge_opts)
        .map_err(OzError::git_source)?;

    if outcome.has_unresolved_conflicts(gix::merge::tree::TreatAsUnresolved::git()) {
        let conflicts = extract_tree_merge_conflicts(&outcome.conflicts);
        return Err(OzError::Conflict {
            message: format!(
                "{} conflict(s) cherry-picking {short_src_7}",
                conflicts.len()
            ),
            conflicts,
            recovery: vec![
                "Resolve conflicting commits or use `oz integrate` for conflict sandbox resolution."
                    .to_string(),
            ],
        });
    }

    let new_tree_id = outcome.tree.write().map_err(OzError::git_source)?.detach();
    let delta = compute_tree_delta(&gix_repo, head_tree, new_tree_id)?;
    check_dirty_collision(repo, &delta, "cherry-pick")?;

    let applied_paths: Vec<String> = delta.iter().map(|d| d.path().to_string()).collect();

    let (_, committer) = resolve_actor_signatures(&mut gix_repo)?;
    let mut raw_msg = orig_message.trim_end_matches(['\r', '\n']).to_string();
    if opts.append_origin {
        let _ = write!(raw_msg, "\n\n(cherry picked from commit {short_src})");
    }
    let parents: Vec<ObjectId> = head.commit_oid.into_iter().collect();
    let (final_msg, change_id) =
        ensure_change_id_trailer(&raw_msg, new_tree_id, &parents, &orig_author);

    if opts.no_commit {
        materialize_tree_delta(repo, &gix_repo, &delta)?;
        return Ok(CherryPickOutcome {
            source_commit_oid: short_src,
            commit_oid: None,
            tree_oid: new_tree_id.to_hex().to_string(),
            branch: head.branch,
            change_id: Some(change_id),
            message: final_msg,
            applied_paths,
            no_commit: true,
            signed: false,
            signature_fingerprint: None,
        });
    }

    let target_ref = target_branch_or_head_ref(repo, head.branch.as_deref())?;
    let (commit_id, signed, signature_fingerprint) = write_and_advance_commit(
        repo,
        &gix_repo,
        CommitWriteSpec {
            tree_id: new_tree_id,
            parents,
            author: orig_author,
            committer,
            message: final_msg.clone(),
            target_ref,
            expected_head: head.commit_oid,
            reflog_op: "cherry-pick",
            sign: opts.sign,
            signing_key: opts.signing_key.clone(),
        },
    )?;

    materialize_tree_delta(repo, &gix_repo, &delta)?;
    sync_index_for_delta(repo, &gix_repo, new_tree_id, &delta)?;

    Ok(CherryPickOutcome {
        source_commit_oid: short_src,
        commit_oid: Some(commit_id.to_hex().to_string()),
        tree_oid: new_tree_id.to_hex().to_string(),
        branch: head.branch,
        change_id: Some(change_id),
        message: final_msg,
        applied_paths,
        no_commit: false,
        signed,
        signature_fingerprint,
    })
}

/// Reverts the changes introduced by `opts.rev` onto current `HEAD` via pure-Rust 3-way tree merge.
pub fn revert_commit(repo: &OzoneRepo, opts: &RevertOptions) -> Result<RevertOutcome> {
    let mut gix_repo = open_gix_for_workspace(repo)?;
    let target_oid = resolve_commit_oid(&gix_repo, &opts.rev)?;
    let (target_tree, parent_tree, orig_subject) = {
        let target_commit = gix_repo
            .find_commit(target_oid)
            .map_err(OzError::git_source)?;
        let target_tree = target_commit
            .tree_id()
            .map_err(OzError::git_source)?
            .detach();
        let parent_tree = resolve_commit_parent_tree(&gix_repo, &target_commit, opts.mainline)?;

        let decoded = target_commit.decode().map_err(OzError::git_source)?;
        let (orig_subject, _) = split_subject_and_body(&decoded.message.to_str_lossy());
        (target_tree, parent_tree, orig_subject)
    };

    let head = resolve_workspace_head(repo, &gix_repo)?;
    let head_tree = head.tree_id;
    let merge_opts = gix_repo.tree_merge_options().map_err(OzError::git_source)?;
    let target_hex = target_oid.to_hex().to_string();
    let short_tgt = &target_hex[..target_hex.len().min(7)];
    let labels = gix::merge::blob::builtin_driver::text::Labels {
        ancestor: Some(short_tgt.into()),
        current: Some(head.branch.as_deref().unwrap_or("HEAD").into()),
        other: Some("parent".into()),
    };

    // Reverse 3-way merge: base = target_tree, ours = head_tree, theirs = parent_tree.
    let mut outcome = gix_repo
        .merge_trees(target_tree, head_tree, parent_tree, labels, merge_opts)
        .map_err(OzError::git_source)?;

    if outcome.has_unresolved_conflicts(gix::merge::tree::TreatAsUnresolved::git()) {
        let conflicts = extract_tree_merge_conflicts(&outcome.conflicts);
        return Err(OzError::Conflict {
            message: format!(
                "{} conflict(s) reverting {short_tgt}",
                conflicts.len()
            ),
            conflicts,
            recovery: vec![
                "Consider `oz undo` if reverting a recent Ozone operation, or resolve conflicting edits."
                    .to_string(),
            ],
        });
    }

    let new_tree_id = outcome.tree.write().map_err(OzError::git_source)?.detach();
    let delta = compute_tree_delta(&gix_repo, head_tree, new_tree_id)?;
    check_dirty_collision(repo, &delta, "revert")?;

    let applied_paths: Vec<String> = delta.iter().map(|d| d.path().to_string()).collect();

    let (author, committer) = resolve_actor_signatures(&mut gix_repo)?;
    let raw_msg = format!("Revert \"{orig_subject}\"\n\nThis reverts commit {target_hex}.");
    let parents: Vec<ObjectId> = head.commit_oid.into_iter().collect();
    let (final_msg, change_id) = ensure_change_id_trailer(&raw_msg, new_tree_id, &parents, &author);

    if opts.no_commit {
        materialize_tree_delta(repo, &gix_repo, &delta)?;
        return Ok(RevertOutcome {
            reverted_commit_oid: target_hex,
            commit_oid: None,
            tree_oid: new_tree_id.to_hex().to_string(),
            branch: head.branch,
            change_id: Some(change_id),
            message: final_msg,
            applied_paths,
            no_commit: true,
            signed: false,
            signature_fingerprint: None,
        });
    }

    let target_ref = target_branch_or_head_ref(repo, head.branch.as_deref())?;
    let (commit_id, signed, signature_fingerprint) = write_and_advance_commit(
        repo,
        &gix_repo,
        CommitWriteSpec {
            tree_id: new_tree_id,
            parents,
            author,
            committer,
            message: final_msg.clone(),
            target_ref,
            expected_head: head.commit_oid,
            reflog_op: "revert",
            sign: opts.sign,
            signing_key: opts.signing_key.clone(),
        },
    )?;

    materialize_tree_delta(repo, &gix_repo, &delta)?;
    sync_index_for_delta(repo, &gix_repo, new_tree_id, &delta)?;

    Ok(RevertOutcome {
        reverted_commit_oid: target_hex,
        commit_oid: Some(commit_id.to_hex().to_string()),
        tree_oid: new_tree_id.to_hex().to_string(),
        branch: head.branch,
        change_id: Some(change_id),
        message: final_msg,
        applied_paths,
        no_commit: false,
        signed,
        signature_fingerprint,
    })
}

// ============================================================================
// 5. `oz tag` (`list` / `create` / `delete` / `sign` / `verify`)
// ============================================================================

/// Options controlling [`tag_create`].
#[derive(Debug, Clone, Default)]
pub struct TagCreateOptions {
    pub name: String,
    pub target_rev: Option<String>,
    pub message: Option<String>,
    pub annotate: bool,
    pub sign: bool,
    pub force: bool,
    pub signing_key: Option<String>,
}

/// Tag summary entry returned by [`tag_list`] / [`tag_create`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagInfo {
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_oid: Option<String>,
    pub target_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tagger: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub signed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_fingerprint: Option<String>,
}

/// Outcome returned by [`tag_list`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagListReport {
    pub count: usize,
    pub tags: Vec<TagInfo>,
}

/// Outcome returned by [`tag_delete`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagDeleteOutcome {
    pub name: String,
    pub deleted_oid: String,
}

/// Verification details returned by [`tag_verify`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagVerifyOutcome {
    pub name: String,
    pub tag_oid: String,
    pub target_oid: String,
    pub valid: bool,
    pub namespace: String,
    pub hash_algorithm: String,
    pub key_algorithm: String,
    pub fingerprint: String,
    pub public_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tagger_email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_signer_matched: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_principal: Option<String>,
}

fn validate_tag_name(name: &str) -> Result<FullName> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('-')
        || trimmed.contains("..")
        || trimmed.chars().any(char::is_whitespace)
    {
        return Err(OzError::InvalidUsage {
            message: format!("invalid tag name '{name}'"),
            recovery: vec!["Pass a valid tag name such as `v1.0.0`.".to_string()],
        });
    }
    let full = format!("refs/tags/{trimmed}");
    FullName::try_from(full.as_str()).map_err(|err| OzError::InvalidUsage {
        message: format!("invalid tag name '{name}': {err}"),
        recovery: Vec::new(),
    })
}

/// Lists all tags in `refs/tags/*`, optionally filtered by a glob/substring pattern.
pub fn tag_list(repo: &OzoneRepo, pattern: Option<&str>) -> Result<TagListReport> {
    let gix_repo = open_gix_for_workspace(repo)?;
    let glob_matcher = pattern
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            let pat = if p.contains('*') || p.contains('?') || p.contains('[') {
                p.to_string()
            } else {
                format!("*{p}*")
            };
            globset::Glob::new(&pat)
                .map(|g| g.compile_matcher())
                .map_err(|err| OzError::InvalidUsage {
                    message: format!("invalid tag pattern '{p}': {err}"),
                    recovery: Vec::new(),
                })
        })
        .transpose()?;

    let platform = gix_repo.references().map_err(OzError::git_source)?;
    let mut tags = Vec::new();
    for item in platform.tags().map_err(OzError::git_source)? {
        let mut r = item.map_err(OzError::git_source)?;
        let name = r.name().shorten().to_str_lossy().into_owned();
        if let Some(ref matcher) = glob_matcher
            && !matcher.is_match(&name)
        {
            continue;
        }
        let direct_id = match r.target() {
            gix::refs::TargetRef::Object(id) => id.to_owned(),
            gix::refs::TargetRef::Symbolic(_) => {
                r.peel_to_id().map_err(OzError::git_source)?.detach()
            }
        };
        let obj = gix_repo
            .find_object(direct_id)
            .map_err(OzError::git_source)?;
        if obj.kind == gix::object::Kind::Tag {
            let tag_ref = gix::objs::TagRef::from_bytes(&obj.data, gix_repo.object_hash())
                .map_err(OzError::git_source)?;
            let target_oid = tag_ref.target().to_hex().to_string();
            let tagger = tag_ref
                .tagger()
                .ok()
                .flatten()
                .map(|t| format!("{} <{}>", t.name, t.email));
            let msg_text = tag_ref.message.to_str_lossy();
            let has_sig = gix::objs::TagRefIter::signature(&obj.data).is_some();
            let (kind, clean_msg) = if has_sig {
                let body = msg_text.trim();
                (
                    "signed".to_string(),
                    (!body.is_empty()).then_some(body.to_string()),
                )
            } else {
                let body = msg_text.trim();
                (
                    "annotated".to_string(),
                    (!body.is_empty()).then_some(body.to_string()),
                )
            };
            tags.push(TagInfo {
                name,
                kind,
                tag_oid: Some(direct_id.to_hex().to_string()),
                target_oid,
                tagger,
                message: clean_msg,
                signed: has_sig,
                signature_fingerprint: None,
            });
        } else {
            tags.push(TagInfo {
                name,
                kind: "lightweight".to_string(),
                tag_oid: None,
                target_oid: direct_id.to_hex().to_string(),
                tagger: None,
                message: None,
                signed: false,
                signature_fingerprint: None,
            });
        }
    }
    tags.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(TagListReport {
        count: tags.len(),
        tags,
    })
}

/// Creates a lightweight, annotated, or SSH-signed Git tag (`namespace = "git"`).
pub fn tag_create(repo: &OzoneRepo, opts: &TagCreateOptions) -> Result<TagInfo> {
    let full_name = validate_tag_name(&opts.name)?;
    let mut gix_repo = open_gix_for_workspace(repo)?;
    let ref_str = full_name.as_bstr().to_str_lossy().into_owned();
    let existing = gix_repo
        .try_find_reference(&ref_str)
        .map_err(OzError::git_source)?;
    if existing.is_some() && !opts.force {
        return Err(OzError::InvalidUsage {
            message: format!("tag '{}' already exists", opts.name),
            recovery: vec![format!(
                "Pass `-f` / `--force` to overwrite tag '{}'.",
                opts.name
            )],
        });
    }

    let target_oid = if let Some(spec) = opts.target_rev.as_deref() {
        resolve_commit_oid(&gix_repo, spec)?
    } else {
        let head = resolve_workspace_head(repo, &gix_repo)?;
        head.commit_oid.ok_or_else(|| OzError::InvalidUsage {
            message: "cannot create tag on unborn HEAD".to_string(),
            recovery: vec!["Create an initial commit first.".to_string()],
        })?
    };

    let wants_sign = opts.sign || opts.signing_key.is_some();
    let wants_annotated = wants_sign || opts.annotate || opts.message.is_some();

    if !wants_annotated {
        let expected = if opts.force {
            PreviousValue::Any
        } else {
            PreviousValue::MustNotExist
        };
        gix_repo
            .edit_references([RefEdit::update(
                full_name,
                target_oid,
                expected,
                format!("oz tag {}", opts.name),
            )])
            .map_err(OzError::git_source)?;
        return Ok(TagInfo {
            name: opts.name.clone(),
            kind: "lightweight".to_string(),
            tag_oid: None,
            target_oid: target_oid.to_hex().to_string(),
            tagger: None,
            message: None,
            signed: false,
            signature_fingerprint: None,
        });
    }

    let raw_msg = opts
        .message
        .as_deref()
        .map(|s| s.trim_end_matches(['\r', '\n']))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| OzError::InvalidUsage {
            message: "annotated or signed tag requires a non-empty `-m <message>`".to_string(),
            recovery: vec![format!(
                "Run `oz tag -s {} -m \"Release {}\"`.",
                opts.name, opts.name
            )],
        })?;

    let (_, tagger_sig) = resolve_actor_signatures(&mut gix_repo)?;
    let tagger_label = format!("{} <{}>", tagger_sig.name, tagger_sig.email);

    let (mut tag_obj, signature_fingerprint) = if wants_sign {
        let key_spec = resolve_signing_key_spec(repo, &gix_repo, opts.signing_key.as_deref());
        let (priv_key, _) = resolve_signing_private_key(&key_spec, Some(&repo.repo_root))?;

        let mut base_tag = gix::objs::Tag {
            target: target_oid,
            target_kind: gix::object::Kind::Commit,
            name: opts.name.as_str().into(),
            tagger: Some(tagger_sig),
            message: raw_msg.into(),
            signature: None,
        };
        // `Tag::write_to` inserts `\n` between `message` and `signature`, so Git's signed payload
        // includes that trailing newline before `-----BEGIN SSH SIGNATURE-----`.
        let mut unsigned_payload = Vec::new();
        base_tag
            .write_to(&mut unsigned_payload)
            .map_err(OzError::git_source)?;
        if !unsigned_payload.ends_with(b"\n") {
            unsigned_payload.push(b'\n');
        }
        let mut armored = priv_key.sign_sshsig(&unsigned_payload, GIT_SSHSIG_NAMESPACE)?;
        if !armored.ends_with('\n') {
            armored.push('\n');
        }
        let fp = priv_key.public_key().fingerprint_sha256();
        base_tag.signature = Some(armored.into());
        (base_tag, Some(fp))
    } else {
        let tag = gix::objs::Tag {
            target: target_oid,
            target_kind: gix::object::Kind::Commit,
            name: opts.name.as_str().into(),
            tagger: Some(tagger_sig),
            message: format!("{raw_msg}\n").into(),
            signature: None,
        };
        (tag, None)
    };

    let _ = &mut tag_obj;
    let tag_oid = gix_repo
        .write_object(&tag_obj)
        .map_err(OzError::git_source)?
        .detach();

    let expected = if opts.force {
        PreviousValue::Any
    } else {
        PreviousValue::MustNotExist
    };
    gix_repo
        .edit_references([RefEdit::update(
            full_name,
            tag_oid,
            expected,
            format!("oz tag {}", opts.name),
        )])
        .map_err(OzError::git_source)?;

    Ok(TagInfo {
        name: opts.name.clone(),
        kind: if wants_sign {
            "signed".to_string()
        } else {
            "annotated".to_string()
        },
        tag_oid: Some(tag_oid.to_hex().to_string()),
        target_oid: target_oid.to_hex().to_string(),
        tagger: Some(tagger_label),
        message: Some(raw_msg.to_string()),
        signed: wants_sign,
        signature_fingerprint,
    })
}

/// Deletes an existing tag (`refs/tags/<name>`).
pub fn tag_delete(repo: &OzoneRepo, name: &str) -> Result<TagDeleteOutcome> {
    let full_name = validate_tag_name(name)?;
    let gix_repo = open_gix_for_workspace(repo)?;
    let ref_str = full_name.as_bstr().to_str_lossy().into_owned();
    let existing = gix_repo
        .try_find_reference(&ref_str)
        .map_err(OzError::git_source)?
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!("tag '{name}' not found"),
            recovery: vec!["Run `oz tag` to list existing tags.".to_string()],
        })?;
    let deleted_oid = match existing.target() {
        gix::refs::TargetRef::Object(id) => id.to_owned(),
        gix::refs::TargetRef::Symbolic(_) => existing
            .into_fully_peeled_id()
            .map_err(OzError::git_source)?
            .detach(),
    };

    gix_repo
        .edit_references([RefEdit::delete(
            full_name,
            PreviousValue::MustExistAndMatch(Target::Object(deleted_oid)),
        )])
        .map_err(OzError::git_source)?;

    Ok(TagDeleteOutcome {
        name: name.to_string(),
        deleted_oid: deleted_oid.to_hex().to_string(),
    })
}

/// Cryptographically verifies an OpenSSH `SSHSIG` v1 signature on an annotated tag.
pub fn tag_verify(repo: &OzoneRepo, name: &str) -> Result<TagVerifyOutcome> {
    let full_name = validate_tag_name(name)?;
    let gix_repo = open_gix_for_workspace(repo)?;
    let ref_str = full_name.as_bstr().to_str_lossy().into_owned();
    let mut reference = gix_repo
        .try_find_reference(&ref_str)
        .map_err(OzError::git_source)?
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!("tag '{name}' not found"),
            recovery: vec!["Run `oz tag` to list tags.".to_string()],
        })?;
    let direct_id = match reference.target() {
        gix::refs::TargetRef::Object(id) => id.to_owned(),
        gix::refs::TargetRef::Symbolic(_) => reference
            .peel_to_id()
            .map_err(OzError::git_source)?
            .detach(),
    };
    let obj = gix_repo
        .find_object(direct_id)
        .map_err(OzError::git_source)?;
    if obj.kind != gix::object::Kind::Tag {
        return Err(OzError::CheckFailed {
            message: format!("tag '{name}' is a lightweight tag and cannot carry a signature"),
            recovery: vec![format!(
                "Create a signed tag with `oz tag -s {name} -m \"...\"`."
            )],
        });
    }
    let Some((sig_ref, signed_data)) = gix::objs::TagRefIter::signature(&obj.data) else {
        return Err(OzError::CheckFailed {
            message: format!("tag '{name}' is not signed"),
            recovery: Vec::new(),
        });
    };
    let tag_ref = gix::objs::TagRef::from_bytes(&obj.data, gix_repo.object_hash())
        .map_err(OzError::git_source)?;
    let target_oid = tag_ref.target().to_hex().to_string();
    let tagger_email = tag_ref
        .tagger()
        .ok()
        .flatten()
        .map(|t| t.email.to_str_lossy().into_owned());

    // Reconstruct contiguous unsigned bytes (slice before the signature block).
    let sig_start = obj.data.len() - sig_ref.data.len();
    let _ = signed_data;
    let unsigned_payload = &obj.data[..sig_start];
    let armored_sig = sig_ref.data.to_str_lossy();
    let parsed = verify_sshsig(unsigned_payload, &armored_sig, GIT_SSHSIG_NAMESPACE)?;

    let configured_allowed = gix_repo
        .config_snapshot()
        .string("gpg.ssh.allowedSignersFile")
        .map(|v| expand_tilde_path(&v.to_str_lossy()))
        .filter(|p| p.is_file());
    let (allowed_signer_matched, matched_principal) = if let Some(ref as_path) = configured_allowed
    {
        let (matched, principal) =
            match_allowed_signers_file(as_path, &parsed.public_key, tagger_email.as_deref())?;
        (Some(matched), principal)
    } else {
        (None, None)
    };

    Ok(TagVerifyOutcome {
        name: name.to_string(),
        tag_oid: direct_id.to_hex().to_string(),
        target_oid,
        valid: true,
        namespace: parsed.namespace,
        hash_algorithm: parsed.hash_algorithm,
        key_algorithm: "ssh-ed25519".to_string(),
        fingerprint: parsed.public_key.fingerprint_sha256(),
        public_key: parsed.public_key.to_openssh_line(),
        tagger_email,
        allowed_signer_matched,
        matched_principal,
    })
}

// ============================================================================
// 6. `oz branch` (`list` / `create` / `rename` / `delete`)
// ============================================================================

/// Summary entry for a single local or remote-tracking branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    pub is_current: bool,
    #[serde(default)]
    pub is_remote: bool,
    pub head_oid: String,
    pub short_oid: String,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// Options controlling [`branch_list_with_options`].
#[derive(Debug, Clone, Default)]
pub struct BranchListOptions {
    pub pattern: Option<String>,
    /// Include remote-tracking branches alongside local branches (`-a` / `--all`).
    pub include_remotes: bool,
    /// List only remote-tracking branches (`-r` / `--remotes`).
    pub remotes_only: bool,
}

/// Outcome returned by [`branch_list`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchListReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_branch: Option<String>,
    pub count: usize,
    pub branches: Vec<BranchInfo>,
}

/// Outcome returned by [`branch_create`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchCreateOutcome {
    pub name: String,
    pub head_oid: String,
}

/// Outcome returned by [`branch_rename`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchRenameOutcome {
    pub old_name: String,
    pub new_name: String,
    pub head_oid: String,
    pub updated_current_head: bool,
}

/// Outcome returned by [`branch_delete`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchDeleteOutcome {
    pub name: String,
    pub deleted_oid: String,
}

fn validate_branch_name(name: &str) -> Result<FullName> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('-')
        || trimmed.contains("..")
        || trimmed.chars().any(char::is_whitespace)
    {
        return Err(OzError::InvalidUsage {
            message: format!("invalid branch name '{name}'"),
            recovery: vec!["Pass a valid branch name such as `feat/login`.".to_string()],
        });
    }
    let full = format!("refs/heads/{trimmed}");
    FullName::try_from(full.as_str()).map_err(|err| OzError::InvalidUsage {
        message: format!("invalid branch name '{name}': {err}"),
        recovery: Vec::new(),
    })
}

fn map_workspace_branches(repo: &OzoneRepo) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let state_dir = repo.layout.state_dir();
    let Ok(entries) = fs::read_dir(state_dir) else {
        return map;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(raw) = fs::read_to_string(&path)
            && let Ok(ws) = serde_json::from_str::<WorkspaceState>(&raw)
        {
            map.insert(ws.branch, ws.name);
        }
    }
    map
}

/// Lists local branches (`refs/heads/*`) with current-branch indicator and latest commit subject.
pub fn branch_list(repo: &OzoneRepo, pattern: Option<&str>) -> Result<BranchListReport> {
    branch_list_with_options(
        repo,
        &BranchListOptions {
            pattern: pattern.map(str::to_string),
            include_remotes: false,
            remotes_only: false,
        },
    )
}

/// Lists local and/or remote-tracking branches (`-a` / `-r`) with full SHAs and commit subjects.
pub fn branch_list_with_options(
    repo: &OzoneRepo,
    opts: &BranchListOptions,
) -> Result<BranchListReport> {
    let gix_repo = open_gix_for_workspace(repo)?;
    let head = resolve_workspace_head(repo, &gix_repo)?;
    let ws_by_branch = map_workspace_branches(repo);
    let glob_matcher = opts
        .pattern
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            let pat = if p.contains('*') || p.contains('?') || p.contains('[') {
                p.to_string()
            } else {
                format!("*{p}*")
            };
            globset::Glob::new(&pat)
                .map(|g| g.compile_matcher())
                .map_err(|err| OzError::InvalidUsage {
                    message: format!("invalid branch pattern '{p}': {err}"),
                    recovery: Vec::new(),
                })
        })
        .transpose()?;

    let platform = gix_repo.references().map_err(OzError::git_source)?;
    let mut branches = Vec::new();

    if !opts.remotes_only {
        for item in platform.local_branches().map_err(OzError::git_source)? {
            let mut r = item.map_err(OzError::git_source)?;
            let name = r.name().shorten().to_str_lossy().into_owned();
            if let Some(ref matcher) = glob_matcher
                && !matcher.is_match(&name)
            {
                continue;
            }
            let commit = r.peel_to_commit().map_err(OzError::git_source)?;
            let oid_hex = commit.id.to_hex().to_string();
            let short_oid = oid_hex[..oid_hex.len().min(7)].to_string();
            let subject = commit
                .decode()
                .ok()
                .map_or_default(|d| split_subject_and_body(&d.message.to_str_lossy()).0);
            let is_current = head.branch.as_deref() == Some(name.as_str());
            branches.push(BranchInfo {
                workspace: ws_by_branch.get(&name).cloned(),
                name,
                is_current,
                is_remote: false,
                head_oid: oid_hex,
                short_oid,
                subject,
            });
        }
    }

    if opts.include_remotes || opts.remotes_only {
        for item in platform.remote_branches().map_err(OzError::git_source)? {
            let mut r = item.map_err(OzError::git_source)?;
            if matches!(r.target(), gix::refs::TargetRef::Symbolic(_)) {
                continue;
            }
            let name = r.name().shorten().to_str_lossy().into_owned();
            if let Some(ref matcher) = glob_matcher
                && !matcher.is_match(&name)
            {
                continue;
            }
            let Ok(commit) = r.peel_to_commit() else {
                continue;
            };
            let oid_hex = commit.id.to_hex().to_string();
            let short_oid = oid_hex[..oid_hex.len().min(7)].to_string();
            let subject = commit
                .decode()
                .ok()
                .map_or_default(|d| split_subject_and_body(&d.message.to_str_lossy()).0);
            branches.push(BranchInfo {
                workspace: None,
                name,
                is_current: false,
                is_remote: true,
                head_oid: oid_hex,
                short_oid,
                subject,
            });
        }
    }

    branches.sort_by(|a, b| {
        a.is_remote
            .cmp(&b.is_remote)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(BranchListReport {
        current_branch: head.branch,
        count: branches.len(),
        branches,
    })
}

// ============================================================================
// 8. `oz grep` (Working-Tree & Revision-Scoped Pure-Rust Tree Grep)
// ============================================================================

/// Options controlling [`grep_repo`] (`oz grep <pattern> [<rev>] [-- <paths...>]`).
#[derive(Debug, Clone, Default)]
pub struct GrepOptions {
    pub pattern: String,
    /// Optional commit, branch, or remote-tracking ref (e.g. `origin/main`) to search without checking out.
    pub rev: Option<String>,
    /// Optional path prefixes, exact file paths, or globs to scope the search (also accepts `:!<pat>` / `:(exclude)<pat>`).
    pub paths: Vec<String>,
    /// Optional path prefixes or globs to exclude (`--exclude`).
    pub excludes: Vec<String>,
    /// Case-insensitive matching (`-i` / `--ignore-case`).
    pub ignore_case: bool,
    /// Interpret `pattern` as a literal substring instead of a regular expression (`-F` / `--fixed-strings`).
    pub fixed_strings: bool,
    /// Only return matching file paths without individual line entries (`-l` / `--files-with-matches`).
    pub files_with_matches: bool,
    /// Optional cap on total returned line matches.
    pub max_matches: Option<usize>,
    /// Optional maximum display length (in characters) per matched line before truncating (`None` or `Some(0)` disables truncation).
    pub max_line_len: Option<usize>,
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

/// Single matching line returned by [`grep_repo`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepMatch {
    pub path: String,
    pub line_number: usize,
    pub line: String,
}

/// Structured grep result returned by [`grep_repo`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepReport {
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_oid: Option<String>,
    pub total_matches: usize,
    pub matched_files: Vec<String>,
    pub matches: Vec<GrepMatch>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub lines_truncated: usize,
}

fn normalize_grep_pat(raw: &str) -> String {
    let trimmed = raw.trim();
    let cow: std::borrow::Cow<'_, str> = if trimmed.contains('\\') {
        std::borrow::Cow::Owned(trimmed.replace('\\', "/"))
    } else {
        std::borrow::Cow::Borrowed(trimmed)
    };
    cow.trim_start_matches("./")
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_string()
}

fn compile_grep_glob(pat: &str) -> Option<globset::GlobMatcher> {
    if pat.contains('*') || pat.contains('?') || pat.contains('[') || pat.contains('{') {
        globset::GlobBuilder::new(pat)
            .literal_separator(false)
            .build()
            .ok()
            .map(|g| g.compile_matcher())
    } else {
        None
    }
}

fn truncate_grep_line(line: &str, max_len: usize) -> (String, bool) {
    let total_chars = line.chars().count();
    if max_len == 0 || total_chars <= max_len {
        return (line.to_string(), false);
    }
    let prefix: String = line.chars().take(max_len).collect();
    let remaining = total_chars - max_len;
    (format!("{prefix}... (+{remaining} chars truncated)"), true)
}

/// Reads `path` only after sniffing its first [`crate::diff::BINARY_SNIFF_BYTES`] (8192 bytes)
/// for a NUL byte (`0x00`), avoiding reading huge binary files into memory.
fn read_non_binary_file(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read as _;

    let mut file = fs::File::open(path)?;
    let mut sniff = [0u8; crate::diff::BINARY_SNIFF_BYTES];
    let mut filled = 0usize;
    while filled < sniff.len() {
        match file.read(&mut sniff[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    if filled == 0 {
        return Ok(Some(Vec::new()));
    }
    if sniff[..filled].contains(&0) {
        return Ok(None);
    }
    if filled < sniff.len() {
        return Ok(Some(sniff[..filled].to_vec()));
    }
    let cap = file
        .metadata()
        .map_or(filled, |m| usize::try_from(m.len()).unwrap_or(filled));
    let mut buf = Vec::with_capacity(cap.max(filled));
    buf.extend_from_slice(&sniff[..filled]);
    file.read_to_end(&mut buf)?;
    Ok(Some(buf))
}

struct GrepLineHit {
    line_number: usize,
    line: String,
    was_cut: bool,
}

struct FileGrepResult {
    path: String,
    matches: Vec<GrepLineHit>,
    truncated: bool,
}

struct GrepWorkerFlush<'a> {
    local: Vec<FileGrepResult>,
    shared: &'a std::sync::Mutex<Vec<FileGrepResult>>,
}

impl Drop for GrepWorkerFlush<'_> {
    fn drop(&mut self) {
        if self.local.is_empty() {
            return;
        }
        let mut guard = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.append(&mut self.local);
    }
}

/// Searches tracked working-tree files or a committed Git revision's tree (`origin/main`, `HEAD`, etc.)
/// without mutating the working tree.
pub fn grep_repo(repo: &OzoneRepo, opts: &GrepOptions) -> Result<GrepReport> {
    if opts.pattern.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "grep pattern cannot be empty".to_string(),
            recovery: vec!["oz grep <pattern> [<rev>] [-- <paths...>]".to_string()],
        });
    }

    let regex = if opts.fixed_strings {
        SimpleRegex::compile_literal(&opts.pattern, opts.ignore_case)?
    } else {
        SimpleRegex::compile(&opts.pattern, opts.ignore_case)?
    };

    let mut normalized_paths: Vec<String> = Vec::new();
    let mut normalized_excludes: Vec<String> = opts
        .excludes
        .iter()
        .map(|p| normalize_grep_pat(p))
        .filter(|p| !p.is_empty())
        .collect();
    for raw in &opts.paths {
        let trimmed = raw.trim();
        if let Some(ex) = trimmed
            .strip_prefix(":!")
            .or_else(|| trimmed.strip_prefix(":(exclude)"))
        {
            let norm = normalize_grep_pat(ex);
            if !norm.is_empty() {
                normalized_excludes.push(norm);
            }
        } else {
            let norm = normalize_grep_pat(trimmed);
            if !norm.is_empty() {
                normalized_paths.push(norm);
            }
        }
    }

    let glob_matchers: Vec<globset::GlobMatcher> = normalized_paths
        .iter()
        .filter_map(|p| compile_grep_glob(p))
        .collect();
    let exclude_globs: Vec<globset::GlobMatcher> = normalized_excludes
        .iter()
        .filter_map(|p| compile_grep_glob(p))
        .collect();

    let path_excluded = |rel: &str| -> bool {
        if normalized_excludes.is_empty() {
            return false;
        }
        if normalized_excludes.iter().any(|p| {
            rel == p
                || (rel.len() > p.len()
                    && rel.starts_with(p.as_str())
                    && rel.as_bytes()[p.len()] == b'/')
        }) {
            return true;
        }
        exclude_globs.iter().any(|m| m.is_match(rel))
    };

    let path_included = |rel: &str| -> bool {
        if path_excluded(rel) {
            return false;
        }
        if normalized_paths.is_empty() {
            return true;
        }
        if normalized_paths.iter().any(|p| {
            rel == p
                || (rel.len() > p.len()
                    && rel.starts_with(p.as_str())
                    && rel.as_bytes()[p.len()] == b'/')
        }) {
            return true;
        }
        glob_matchers.iter().any(|m| m.is_match(rel))
    };

    let dir_could_match = |dir_prefix: &str| -> bool {
        if normalized_excludes.iter().any(|p| {
            dir_prefix == p
                || (dir_prefix.len() > p.len()
                    && dir_prefix.starts_with(p.as_str())
                    && dir_prefix.as_bytes()[p.len()] == b'/')
        }) {
            return false;
        }
        if normalized_paths.is_empty() || !glob_matchers.is_empty() {
            return true;
        }
        normalized_paths.iter().any(|p| {
            dir_prefix == p
                || (p.len() > dir_prefix.len()
                    && p.starts_with(dir_prefix)
                    && p.as_bytes()[dir_prefix.len()] == b'/')
                || (dir_prefix.len() > p.len()
                    && dir_prefix.starts_with(p.as_str())
                    && dir_prefix.as_bytes()[p.len()] == b'/')
        })
    };

    let mut matches: Vec<GrepMatch> = Vec::new();
    let mut matched_files: Vec<String> = Vec::new();
    let mut truncated = false;
    let mut lines_truncated = 0usize;
    let max_matches = opts.max_matches.unwrap_or(usize::MAX);
    let max_line_len = opts.max_line_len.unwrap_or(0);

    let scan_non_binary_bytes = |rel_path: String, bytes: &[u8]| -> Option<FileGrepResult> {
        let text = String::from_utf8_lossy(bytes);
        if opts.files_with_matches {
            for raw_line in text.lines() {
                let line = raw_line.trim_end_matches('\r');
                if regex.is_match(line) {
                    return Some(FileGrepResult {
                        path: rel_path,
                        matches: Vec::new(),
                        truncated: false,
                    });
                }
            }
            return None;
        }

        let mut file_hits: Vec<GrepLineHit> = Vec::new();
        let mut file_truncated = false;
        for (idx, raw_line) in text.lines().enumerate() {
            let line = raw_line.trim_end_matches('\r');
            if regex.is_match(line) {
                if file_hits.len() >= max_matches {
                    file_truncated = true;
                    break;
                }
                let (rendered_line, was_cut) = truncate_grep_line(line, max_line_len);
                file_hits.push(GrepLineHit {
                    line_number: idx + 1,
                    line: rendered_line,
                    was_cut,
                });
            }
        }
        if file_hits.is_empty() && !file_truncated {
            None
        } else {
            Some(FileGrepResult {
                path: rel_path,
                matches: file_hits,
                truncated: file_truncated,
            })
        }
    };

    let commit_oid = if let Some(ref rev_spec) = opts.rev {
        let mut scan_tree_bytes = |rel_path: &str, bytes: &[u8]| -> bool {
            if truncated || is_binary_bytes(bytes) {
                return truncated;
            }
            let text = String::from_utf8_lossy(bytes);
            let mut file_hit = false;
            for (idx, raw_line) in text.lines().enumerate() {
                let line = raw_line.trim_end_matches('\r');
                if regex.is_match(line) {
                    if !file_hit {
                        file_hit = true;
                        matched_files.push(rel_path.to_string());
                        if opts.files_with_matches {
                            return false;
                        }
                    }
                    if matches.len() >= max_matches {
                        truncated = true;
                        return true;
                    }
                    let (rendered_line, was_cut) = truncate_grep_line(line, max_line_len);
                    if was_cut {
                        lines_truncated += 1;
                    }
                    matches.push(GrepMatch {
                        path: rel_path.to_string(),
                        line_number: idx + 1,
                        line: rendered_line,
                    });
                }
            }
            false
        };

        let gix_repo = open_gix_for_workspace(repo)?;
        let cid = resolve_commit_oid(&gix_repo, rev_spec)?;
        let tree_id = commit_tree_id(&gix_repo, cid)?;
        let mut stack: Vec<(String, ObjectId)> = vec![(String::new(), tree_id)];
        let mut stop = false;
        while let Some((prefix, tid)) = stack.pop() {
            if stop {
                break;
            }
            let tree = gix_repo.find_tree(tid).map_err(OzError::git_source)?;
            let mut subtrees = Vec::new();
            let mut blobs = Vec::new();
            for entry in tree.iter() {
                let Ok(entry) = entry else {
                    continue;
                };
                let name = entry.filename().to_str_lossy().into_owned();
                let rel = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                if entry.mode().is_tree() {
                    if dir_could_match(&rel) {
                        subtrees.push((rel, entry.oid().to_owned()));
                    }
                } else if entry.mode().kind() != EntryKind::Commit && path_included(&rel) {
                    blobs.push((rel, entry.oid().to_owned()));
                }
            }
            blobs.sort_by(|a, b| a.0.cmp(&b.0));
            for (rel, oid) in blobs {
                if let Ok(bytes) = read_blob_bytes(&gix_repo, oid)
                    && scan_tree_bytes(&rel, &bytes)
                {
                    stop = true;
                    break;
                }
            }
            subtrees.sort_by(|a, b| b.0.cmp(&a.0));
            stack.extend(subtrees);
        }
        Some(cid.to_hex().to_string())
    } else {
        let shared_results: std::sync::Mutex<Vec<FileGrepResult>> =
            std::sync::Mutex::new(Vec::new());
        let mut builder = ignore::WalkBuilder::new(&repo.workdir_path);
        builder
            .hidden(false)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(true)
            .threads(rayon::current_num_threads().clamp(1, 16))
            .filter_entry(|e| e.file_name() != ".git");

        let workdir_root = &repo.workdir_path;
        let shared_ref = &shared_results;
        let path_included_ref = &path_included;
        let scan_ref = &scan_non_binary_bytes;

        builder.build_parallel().run(|| {
            let mut collector = GrepWorkerFlush {
                local: Vec::new(),
                shared: shared_ref,
            };
            Box::new(move |result| {
                let Ok(entry) = result else {
                    return ignore::WalkState::Continue;
                };
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    return ignore::WalkState::Continue;
                }
                let Ok(rel_os) = entry.path().strip_prefix(workdir_root) else {
                    return ignore::WalkState::Continue;
                };
                let rel_lossy = rel_os.to_string_lossy();
                let rel: std::borrow::Cow<'_, str> = if rel_lossy.contains('\\') {
                    std::borrow::Cow::Owned(rel_lossy.replace('\\', "/"))
                } else {
                    rel_lossy
                };
                if !path_included_ref(&rel) {
                    return ignore::WalkState::Continue;
                }
                if let Ok(Some(bytes)) = read_non_binary_file(entry.path())
                    && let Some(hit) = scan_ref(rel.into_owned(), &bytes)
                {
                    collector.local.push(hit);
                }
                ignore::WalkState::Continue
            })
        });

        let mut file_results = shared_results
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        file_results.sort_by(|a, b| a.path.cmp(&b.path));
        if opts.files_with_matches {
            matched_files = file_results.into_iter().map(|r| r.path).collect();
        } else {
            'merge: for file_res in file_results {
                let mut pushed_file = false;
                for lm in file_res.matches {
                    if matches.len() >= max_matches {
                        truncated = true;
                        break 'merge;
                    }
                    if !pushed_file {
                        pushed_file = true;
                        matched_files.push(file_res.path.clone());
                    }
                    if lm.was_cut {
                        lines_truncated += 1;
                    }
                    matches.push(GrepMatch {
                        path: file_res.path.clone(),
                        line_number: lm.line_number,
                        line: lm.line,
                    });
                }
                if file_res.truncated {
                    truncated = true;
                    break 'merge;
                }
            }
            matches.sort_by(|a, b| {
                (a.path.as_str(), a.line_number).cmp(&(b.path.as_str(), b.line_number))
            });
        }
        None
    };

    let total_matches = if opts.files_with_matches {
        matched_files.len()
    } else {
        matches.len()
    };

    Ok(GrepReport {
        pattern: opts.pattern.clone(),
        rev: opts.rev.clone(),
        commit_oid,
        total_matches,
        matched_files,
        matches,
        truncated,
        lines_truncated,
    })
}

/// Creates a new local branch `name` pointing at `start_point` (defaults to `HEAD`).
pub fn branch_create(
    repo: &OzoneRepo,
    name: &str,
    start_point: Option<&str>,
    force: bool,
) -> Result<BranchCreateOutcome> {
    let full_name = validate_branch_name(name)?;
    let gix_repo = open_gix_for_workspace(repo)?;
    let ref_str = full_name.as_bstr().to_str_lossy().into_owned();
    let existing = gix_repo
        .try_find_reference(&ref_str)
        .map_err(OzError::git_source)?;
    if existing.is_some() && !force {
        return Err(OzError::InvalidUsage {
            message: format!("a branch named '{name}' already exists"),
            recovery: vec![format!("Pass `-f` / `--force` or `oz git switch {name}`.")],
        });
    }
    if existing.is_some()
        && let Some(other_wt) = find_worktree_using_branch(repo, &full_name)?
    {
        return Err(OzError::Conflict {
            message: format!("branch '{name}' is checked out in worktree '{other_wt}'"),
            conflicts: Vec::new(),
            recovery: vec!["Choose a different branch name.".to_string()],
        });
    }

    let oid = if let Some(spec) = start_point {
        resolve_commit_oid(&gix_repo, spec)?
    } else {
        let head = resolve_workspace_head(repo, &gix_repo)?;
        head.commit_oid.ok_or_else(|| OzError::InvalidUsage {
            message: "cannot create branch on unborn HEAD".to_string(),
            recovery: vec!["Create an initial commit first.".to_string()],
        })?
    };

    let expected = if force {
        PreviousValue::Any
    } else {
        PreviousValue::MustNotExist
    };
    gix_repo
        .edit_references([RefEdit::update(
            full_name,
            oid,
            expected,
            format!("oz branch {name}"),
        )])
        .map_err(OzError::git_source)?;

    Ok(BranchCreateOutcome {
        name: name.to_string(),
        head_oid: oid.to_hex().to_string(),
    })
}

/// Renames a local branch from `old_name` (or current branch if `None`) to `new_name`.
pub fn branch_rename(
    repo: &OzoneRepo,
    old_name: Option<&str>,
    new_name: &str,
    force: bool,
) -> Result<BranchRenameOutcome> {
    let gix_repo = open_gix_for_workspace(repo)?;
    let head = resolve_workspace_head(repo, &gix_repo)?;
    let resolved_old = match old_name {
        Some(n) => n.to_string(),
        None => head.branch.clone().ok_or_else(|| OzError::InvalidUsage {
            message: "HEAD is detached; specify `<old_branch> <new_branch>`".to_string(),
            recovery: vec!["oz branch -m <old> <new>".to_string()],
        })?,
    };
    if resolved_old == new_name {
        return Err(OzError::InvalidUsage {
            message: format!("old and new branch names are identical ('{new_name}')"),
            recovery: Vec::new(),
        });
    }

    let old_full = validate_branch_name(&resolved_old)?;
    let new_full = validate_branch_name(new_name)?;

    if let Some(other_wt) = find_worktree_using_branch(repo, &old_full)? {
        return Err(OzError::Conflict {
            message: format!(
                "cannot rename branch '{resolved_old}': checked out in worktree '{other_wt}'"
            ),
            conflicts: Vec::new(),
            recovery: Vec::new(),
        });
    }
    if let Some(other_wt) = find_worktree_using_branch(repo, &new_full)? {
        return Err(OzError::Conflict {
            message: format!("cannot rename to '{new_name}': checked out in worktree '{other_wt}'"),
            conflicts: Vec::new(),
            recovery: Vec::new(),
        });
    }

    let old_ref_str = old_full.as_bstr().to_str_lossy().into_owned();
    let new_ref_str = new_full.as_bstr().to_str_lossy().into_owned();

    let mut old_ref = gix_repo
        .try_find_reference(&old_ref_str)
        .map_err(OzError::git_source)?
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!("branch '{resolved_old}' not found"),
            recovery: vec!["Run `oz branch` to list existing branches.".to_string()],
        })?;
    let oid = old_ref.peel_to_commit().map_err(OzError::git_source)?.id;

    let new_exists = gix_repo
        .try_find_reference(&new_ref_str)
        .map_err(OzError::git_source)?
        .is_some();
    if new_exists && !force {
        return Err(OzError::InvalidUsage {
            message: format!("a branch named '{new_name}' already exists"),
            recovery: vec![format!("Pass `-M` / `--force` to overwrite '{new_name}'.")],
        });
    }

    let is_current = head.branch.as_deref() == Some(resolved_old.as_str());
    let mut edits = Vec::new();
    let new_expected = if force {
        PreviousValue::Any
    } else {
        PreviousValue::MustNotExist
    };
    edits.push(RefEdit::update(
        new_full.clone(),
        oid,
        new_expected,
        format!("oz branch -m {resolved_old} {new_name}"),
    ));

    if is_current && (repo.workspace.is_none() || repo.is_linked_worktree) {
        edits.push(RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: format!("Branch: renamed {old_ref_str} to {new_ref_str}").into(),
                },
                expected: PreviousValue::Any,
                new: Target::Symbolic(new_full),
            },
            name: FullName::try_from("HEAD").map_err(OzError::git_source)?,
            deref: false,
        });
    }

    edits.push(RefEdit::delete(
        old_full,
        PreviousValue::MustExistAndMatch(Target::Object(oid)),
    ));

    gix_repo
        .edit_references(edits)
        .map_err(OzError::git_source)?;

    // If operating in a virtual Ozone workspace whose state file points to `resolved_old`,
    // update `state.branch` so `resolve_workspace_head` stays aligned.
    if is_current
        && let Some(ref ws_name) = repo.workspace
        && !repo.is_linked_worktree
    {
        let state_path = repo.layout.workspace_state_path(ws_name);
        if state_path.is_file()
            && let Ok(raw) = fs::read_to_string(&state_path)
            && let Ok(mut ws) = serde_json::from_str::<WorkspaceState>(&raw)
            && ws.branch == resolved_old
        {
            ws.branch = new_name.to_string();
            ws.updated_at = Utc::now();
            let serialized = serde_json::to_vec_pretty(&ws)?;
            fs::write(&state_path, serialized)?;
        }
    }

    Ok(BranchRenameOutcome {
        old_name: resolved_old,
        new_name: new_name.to_string(),
        head_oid: oid.to_hex().to_string(),
        updated_current_head: is_current,
    })
}

/// Deletes local branch `name`, enforcing merge-safety checks unless `force` (`-D`) is true.
pub fn branch_delete(repo: &OzoneRepo, name: &str, force: bool) -> Result<BranchDeleteOutcome> {
    let full_name = validate_branch_name(name)?;
    let gix_repo = open_gix_for_workspace(repo)?;
    let head = resolve_workspace_head(repo, &gix_repo)?;

    if head.branch.as_deref() == Some(name) {
        return Err(OzError::InvalidUsage {
            message: format!("cannot delete checked-out branch '{name}'"),
            recovery: vec![
                "Switch to another branch first with `oz git switch <other>`.".to_string(),
            ],
        });
    }
    if let Some(other_wt) = find_worktree_using_branch(repo, &full_name)? {
        return Err(OzError::Conflict {
            message: format!("cannot delete branch '{name}': checked out in worktree '{other_wt}'"),
            conflicts: Vec::new(),
            recovery: vec![format!(
                "Remove the worktree or switch it off '{name}' first."
            )],
        });
    }

    let ref_str = full_name.as_bstr().to_str_lossy().into_owned();
    let mut branch_ref = gix_repo
        .try_find_reference(&ref_str)
        .map_err(OzError::git_source)?
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!("branch '{name}' not found"),
            recovery: vec!["Run `oz branch` to list local branches.".to_string()],
        })?;
    let branch_oid = branch_ref.peel_to_commit().map_err(OzError::git_source)?.id;

    if !force && let Some(head_oid) = head.commit_oid {
        let merged = if branch_oid == head_oid {
            true
        } else {
            gix_repo
                .merge_base(branch_oid, head_oid)
                .map_err(OzError::git_source)?
                .is_some_and(|mb| mb.detach() == branch_oid)
        };
        if !merged {
            return Err(OzError::InvalidUsage {
                message: format!("branch '{name}' is not fully merged into HEAD"),
                recovery: vec![format!("Run `oz branch -D {name}` to force-delete it.")],
            });
        }
    }

    gix_repo
        .edit_references([RefEdit::delete(
            full_name,
            PreviousValue::MustExistAndMatch(Target::Object(branch_oid)),
        )])
        .map_err(OzError::git_source)?;

    Ok(BranchDeleteOutcome {
        name: name.to_string(),
        deleted_oid: branch_oid.to_hex().to_string(),
    })
}

// ============================================================================
// 7. `oz restore` (selective working-tree restore)
// ============================================================================

/// Options controlling [`restore_paths`].
#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    pub targets: Vec<String>,
    pub source: Option<String>,
    pub remove_untracked: bool,
}

/// Structured summary returned by [`restore_paths`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreOutcome {
    pub source_oid: String,
    pub restored_paths: Vec<String>,
    pub removed_paths: Vec<String>,
}

fn looks_like_file_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('f') else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

fn looks_like_hunk_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('h') else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let suffix = &rest[digits..];
    !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_lowercase())
}

fn normalize_restore_target(workdir: &Path, raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "restore target path cannot be empty".to_string(),
            recovery: Vec::new(),
        });
    }
    let candidate = Path::new(trimmed);
    let rel_path = if candidate.is_absolute() {
        let canon_workdir = fs::canonicalize(workdir).unwrap_or_else(|_| workdir.to_path_buf());
        let stripped = candidate
            .strip_prefix(workdir)
            .or_else(|_| candidate.strip_prefix(&canon_workdir))
            .map_err(|_| OzError::InvalidUsage {
                message: format!(
                    "path '{}' is outside repository root '{}'",
                    raw,
                    workdir.display()
                ),
                recovery: Vec::new(),
            })?;
        stripped.to_path_buf()
    } else {
        candidate.to_path_buf()
    };

    let mut parts = Vec::new();
    for comp in rel_path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(p) => {
                let s = p.to_string_lossy();
                if s == ".git" && parts.is_empty() {
                    return Err(OzError::InvalidUsage {
                        message: "cannot restore paths inside `.git/`".to_string(),
                        recovery: Vec::new(),
                    });
                }
                parts.push(s.into_owned());
            }
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(OzError::InvalidUsage {
                    message: format!("invalid restore target '{raw}'"),
                    recovery: Vec::new(),
                });
            }
        }
    }

    Ok(if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    })
}

fn collect_tree_leaf_entries(
    gix_repo: &gix::Repository,
    tree_id: ObjectId,
    prefix: &str,
    out: &mut Vec<(String, ObjectId, EntryKind)>,
) -> Result<()> {
    let tree = gix_repo.find_tree(tree_id).map_err(OzError::git_source)?;
    for entry_res in tree.iter() {
        let entry = entry_res.map_err(OzError::git_source)?;
        let name = entry.filename().to_str_lossy();
        let rel = if prefix.is_empty() || prefix == "." {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry.mode().kind();
        if kind == EntryKind::Tree {
            collect_tree_leaf_entries(gix_repo, entry.oid().to_owned(), &rel, out)?;
        } else if kind != EntryKind::Commit {
            out.push((rel, entry.oid().to_owned(), kind));
        }
    }
    Ok(())
}

/// Selectively restores working-tree paths or file IDs (`f1`) from `HEAD` or `--source <rev>`,
/// leaving all other dirty files untouched and keeping `.git/index` aligned with `HEAD`.
pub fn restore_paths(repo: &OzoneRepo, opts: &RestoreOptions) -> Result<RestoreOutcome> {
    if opts.targets.is_empty() {
        return Err(OzError::InvalidUsage {
            message:
                "oz restore requires at least one path or file ID (e.g. `oz restore README.md`)"
                    .to_string(),
            recovery: vec![
                "Run `oz status` to list dirty files and IDs (`f1`, `f2`, ...).".to_string(),
            ],
        });
    }

    let gix_repo = open_gix_for_workspace(repo)?;
    let head = resolve_workspace_head(repo, &gix_repo)?;

    let source_commit_oid = if let Some(ref rev) = opts.source {
        Some(resolve_commit_oid(&gix_repo, rev)?)
    } else if head.commit_oid.is_some() {
        head.commit_oid
    } else if opts.remove_untracked {
        None
    } else {
        return Err(OzError::InvalidUsage {
            message: "cannot restore from unborn HEAD without commits".to_string(),
            recovery: vec![
                "Pass `--remove-untracked` to remove untracked paths, or create an initial commit first."
                    .to_string(),
            ],
        });
    };

    let source_tree_id = match source_commit_oid {
        Some(oid) => commit_tree_id(&gix_repo, oid)?,
        None => ObjectId::empty_tree(gix_repo.object_hash()),
    };
    let empty_tree_id = ObjectId::empty_tree(gix_repo.object_hash());
    let source_tree = if source_tree_id == empty_tree_id {
        None
    } else {
        Some(
            gix_repo
                .find_tree(source_tree_id)
                .map_err(OzError::git_source)?,
        )
    };

    let needs_status = opts.targets.iter().any(|t| looks_like_file_id(t.trim()));
    let file_id_map: HashMap<String, String> = if needs_status {
        let status = crate::status::compute_status(repo)?;
        status.changes.into_iter().map(|c| (c.id, c.path)).collect()
    } else {
        HashMap::new()
    };

    let mut to_restore: HashMap<String, (ObjectId, EntryKind)> = HashMap::new();
    let mut to_remove: BTreeSet<String> = BTreeSet::new();

    for raw_target in &opts.targets {
        let trimmed = raw_target.trim();
        if looks_like_hunk_id(trimmed) {
            return Err(OzError::InvalidUsage {
                message: format!(
                    "target '{trimmed}' is a hunk ID; `oz restore` operates on file paths or file IDs (`f1`)"
                ),
                recovery: vec![
                    "Use `oz restore <path|f1>` for whole-file restore, `oz commit --pick ...` for partial commits, or `oz undo` to rewind an operation."
                        .to_string(),
                ],
            });
        }

        let resolved_raw =
            if looks_like_file_id(trimmed) && !repo.workdir_path.join(trimmed).exists() {
                match file_id_map.get(trimmed) {
                    Some(path) => path.as_str(),
                    None => {
                        return Err(OzError::InvalidUsage {
                            message: format!("unknown file ID '{trimmed}'"),
                            recovery: vec![
                                "Run `oz status` to inspect current `[f1]` file IDs.".to_string(),
                            ],
                        });
                    }
                }
            } else {
                trimmed
            };

        let rel = normalize_restore_target(&repo.workdir_path, resolved_raw)?;
        if rel == "." {
            if let Some(ref tree) = source_tree {
                let mut entries = Vec::new();
                collect_tree_leaf_entries(&gix_repo, tree.id, ".", &mut entries)?;
                for (path, oid, kind) in entries {
                    to_restore.insert(path, (oid, kind));
                }
            }
            continue;
        }

        let lookup = match source_tree.as_ref() {
            Some(tree) => tree
                .lookup_entry_by_path(rel.as_str())
                .map_err(OzError::git_source)?,
            None => None,
        };

        if let Some(entry) = lookup {
            let kind = entry.mode().kind();
            if kind == EntryKind::Tree {
                let mut entries = Vec::new();
                collect_tree_leaf_entries(&gix_repo, entry.oid().to_owned(), &rel, &mut entries)?;
                for (path, oid, entry_kind) in entries {
                    to_restore.insert(path, (oid, entry_kind));
                }
            } else if kind != EntryKind::Commit {
                to_restore.insert(rel, (entry.oid().to_owned(), kind));
            }
        } else {
            let disk_path = repo.workdir_path.join(&rel);
            let exists_on_disk = fs::symlink_metadata(&disk_path).is_ok();
            if opts.remove_untracked && exists_on_disk {
                to_remove.insert(rel);
            } else {
                let src_label = opts.source.as_deref().unwrap_or("HEAD");
                return Err(OzError::InvalidUsage {
                    message: format!(
                        "pathspec '{raw_target}' did not match any file(s) in {src_label}"
                    ),
                    recovery: vec![
                        "Pass `--remove-untracked` if you intentionally want to delete an untracked path."
                            .to_string(),
                    ],
                });
            }
        }
    }

    let mut restored_paths: Vec<String> = Vec::with_capacity(to_restore.len());
    let mut restore_entries: Vec<(String, ObjectId, EntryKind)> = to_restore
        .into_iter()
        .map(|(p, (o, k))| (p, o, k))
        .collect();
    restore_entries.sort_by(|a, b| a.0.cmp(&b.0));

    for (rel_path, oid, kind) in &restore_entries {
        write_tree_blob_to_disk(&repo.workdir_path, &gix_repo, rel_path, *oid, *kind)?;
        restored_paths.push(rel_path.clone());
    }

    let mut removed_paths: Vec<String> = Vec::with_capacity(to_remove.len());
    for rel_path in to_remove {
        let disk_path = repo.workdir_path.join(&rel_path);
        match fs::symlink_metadata(&disk_path) {
            Ok(meta) => {
                if meta.is_dir() {
                    fs::remove_dir_all(&disk_path)?;
                } else {
                    fs::remove_file(&disk_path)?;
                }
                prune_empty_parents(&repo.workdir_path, &disk_path);
                removed_paths.push(rel_path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(OzError::Io(err)),
        }
    }

    let mut touched_set: BTreeSet<String> = BTreeSet::new();
    for p in &restored_paths {
        touched_set.insert(p.clone());
    }
    for p in &removed_paths {
        touched_set.insert(p.clone());
    }
    let touched_paths: Vec<String> = touched_set.into_iter().collect();
    sync_index_paths_to_tree(repo, &gix_repo, head.tree_id, &touched_paths)?;

    Ok(RestoreOutcome {
        source_oid: source_commit_oid
            .map_or_else(|| "unborn".to_string(), |id| id.to_hex().to_string()),
        restored_paths,
        removed_paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_regex_supports_alternation_classes_quantifiers_and_anchors() {
        let re = SimpleRegex::compile(r"^fn\s+(foo|bar)_[0-9]+$", false).expect("compile");
        assert!(re.is_match("fn foo_42"));
        assert!(re.is_match("fn\tbar_0"));
        assert!(!re.is_match("pub fn foo_42"));
        assert!(!re.is_match("fn baz_42"));

        let ci = SimpleRegex::compile("fix.*bug", true).expect("compile ci");
        assert!(ci.is_match("chore: FIX critical BUG in parser"));
    }

    #[test]
    fn simple_regex_supports_word_boundaries_and_bounded_quantifiers() {
        let wb = SimpleRegex::compile(r"\bunsafe\b", false).expect("compile word boundary");
        assert!(wb.is_match("unsafe fn ptr_op()"));
        assert!(wb.is_match("let x = unsafe { *p };"));
        assert!(!wb.is_match("fn my_unsafe_helper()"));
        assert!(!wb.is_match("unsafely_cast()"));

        let non_wb = SimpleRegex::compile(r"\Bunsafe\B", false).expect("compile non-word boundary");
        assert!(non_wb.is_match("my_unsafe_helper"));
        assert!(!non_wb.is_match("unsafe fn f()"));

        let quant =
            SimpleRegex::compile(r"\b[0-9a-f]{4,6}\b", false).expect("compile {m,n} quantifier");
        assert!(!quant.is_match("abc"));
        assert!(quant.is_match("dead"));
        assert!(quant.is_match("c0ffee"));
        assert!(!quant.is_match("deadbeef"));

        let invalid = SimpleRegex::compile(r"[unclosed", false).expect_err("invalid regex");
        assert!(matches!(invalid, OzError::InvalidUsage { .. }));
    }

    #[test]
    fn grep_repo_parallel_fixed_case_insensitive_binary_skip_and_deterministic_sort() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        let _ = gix::init(root).expect("gix init");

        fs::create_dir_all(root.join("z_dir")).expect("mkdir z_dir");
        fs::create_dir_all(root.join("a_dir")).expect("mkdir a_dir");

        // Regex metacharacters in fixed-string case-insensitive query `-F -i`:
        // literal "foo.bar[0]+" must match "FOO.BAR[0]+" and NOT regex-match "fooXbar000".
        fs::write(
            root.join("z_dir/second.rs"),
            "let nope = \"fooXbar000\";\nlet hit = \"FOO.BAR[0]+\";\nunsafe fn z() {}\n",
        )
        .expect("write z_dir/second.rs");
        fs::write(
            root.join("a_dir/first.rs"),
            "let hit1 = \"foo.bar[0]+\";\nlet skip = \"my_unsafe_ident\";\nlet hit2 = \"Prefix Foo.Bar[0]+ suffix\";\nunsafe { }\n",
        )
        .expect("write a_dir/first.rs");

        // Large binary file (> BINARY_SNIFF_BYTES) with NUL in first 8 KiB and matching text after 8 KiB:
        // must be skipped via 8 KiB prefix sniff!
        let mut bin_payload = vec![b'A'; crate::diff::BINARY_SNIFF_BYTES * 3];
        bin_payload[128] = 0;
        bin_payload.extend_from_slice(b"\nFOO.BAR[0]+\nunsafe fn hidden_in_bin() {}\n");
        fs::write(root.join("a_dir/huge.bin"), &bin_payload).expect("write binary");

        let repo = OzoneRepo::discover(Some(root)).expect("discover repo");

        // 1. `-F -i` fixed case-insensitive without regex false positives, sorted by (path, line_number)
        let fixed_ci = grep_repo(
            &repo,
            &GrepOptions {
                pattern: "foo.bar[0]+".to_string(),
                ignore_case: true,
                fixed_strings: true,
                ..GrepOptions::default()
            },
        )
        .expect("grep -F -i");
        assert!(!fixed_ci.truncated);
        assert_eq!(
            fixed_ci.matched_files,
            vec!["a_dir/first.rs", "z_dir/second.rs"]
        );
        let coords: Vec<(&str, usize)> = fixed_ci
            .matches
            .iter()
            .map(|m| (m.path.as_str(), m.line_number))
            .collect();
        assert_eq!(
            coords,
            vec![
                ("a_dir/first.rs", 1),
                ("a_dir/first.rs", 3),
                ("z_dir/second.rs", 2),
            ]
        );

        // 2. `\bunsafe\b` word boundary skips `my_unsafe_ident` and binary file
        let wb_report = grep_repo(
            &repo,
            &GrepOptions {
                pattern: r"\bunsafe\b".to_string(),
                ..GrepOptions::default()
            },
        )
        .expect("grep word boundary");
        let wb_coords: Vec<(&str, usize)> = wb_report
            .matches
            .iter()
            .map(|m| (m.path.as_str(), m.line_number))
            .collect();
        assert_eq!(
            wb_coords,
            vec![("a_dir/first.rs", 4), ("z_dir/second.rs", 3)]
        );

        // 3. Deterministic `max_matches` truncation & `files_with_matches`
        let capped = grep_repo(
            &repo,
            &GrepOptions {
                pattern: "foo.bar[0]+".to_string(),
                ignore_case: true,
                fixed_strings: true,
                max_matches: Some(2),
                ..GrepOptions::default()
            },
        )
        .expect("grep max_matches");
        assert!(capped.truncated);
        assert_eq!(capped.matches.len(), 2);
        assert_eq!(capped.matches[0].path, "a_dir/first.rs");
        assert_eq!(capped.matches[0].line_number, 1);
        assert_eq!(capped.matches[1].path, "a_dir/first.rs");
        assert_eq!(capped.matches[1].line_number, 3);

        let files_only = grep_repo(
            &repo,
            &GrepOptions {
                pattern: r"\bunsafe\b".to_string(),
                files_with_matches: true,
                ..GrepOptions::default()
            },
        )
        .expect("grep -l");
        assert_eq!(files_only.total_matches, 2);
        assert_eq!(
            files_only.matched_files,
            vec!["a_dir/first.rs", "z_dir/second.rs"]
        );
        assert_eq!(files_only.matches, Vec::<GrepMatch>::new());
    }

    #[test]
    fn since_timestamp_parses_iso_dates_and_relative_durations() {
        let now = Utc.with_ymd_and_hms(2026, 11, 21, 15, 30, 0).unwrap();

        assert_eq!(
            parse_since_timestamp("2026-11-20T12:00:00Z", now).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap()
        );
        assert_eq!(
            parse_since_timestamp("2026-11-19", now).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 19, 0, 0, 0).unwrap()
        );
        assert_eq!(
            parse_since_timestamp("2 days ago", now).unwrap(),
            now - Duration::days(2)
        );
        assert_eq!(
            parse_since_timestamp("12h", now).unwrap(),
            now - Duration::hours(12)
        );
        assert_eq!(
            parse_since_timestamp("yesterday", now).unwrap(),
            now - Duration::days(1)
        );
        assert!(parse_since_timestamp("not-a-date", now).is_err());
    }

    #[test]
    fn stash_selector_parses_index_and_ref_syntax() {
        assert_eq!(parse_stash_selector(None).unwrap(), 0);
        assert_eq!(parse_stash_selector(Some("0")).unwrap(), 0);
        assert_eq!(parse_stash_selector(Some("stash@{2}")).unwrap(), 2);
        assert!(parse_stash_selector(Some("stash@{abc}")).is_err());
    }
}
