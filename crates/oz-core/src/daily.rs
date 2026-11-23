// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Daily human developer operations in pure Rust (`gix` + `imara-diff` + `SSHSIG`):
//! - `log_commits` (`oz log`: compact commit graph, `--oneline`, `-n`, `--author`, `--grep`, `-S`, `-G`, `--since`)
//! - `show_commit` (`oz show`: commit metadata, `Change-Id`, signature status, `--stat`, unified diff)
//! - `stash_*` (`oz stash`: `push`, `pop`, `list`, `drop` stored as reachable Git objects under `refs/ozone/stash`)
//! - `cherry_pick_commit` (`oz cherry-pick`) and `revert_commit` (`oz revert`)
//! - `tag_*` (`oz tag`: create, list, delete, and SSH-signed tags via `ssh_sign`)
//! - `branch_*` (`oz branch`: list, create, rename, delete)

use crate::commit::{compute_change_id, ensure_change_id_trailer, verify_commit_signature};
use crate::diff::{
    compute_file_diff, file_id, is_binary_bytes, render_file_unified_diff,
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
use gix::hash::ObjectId;
use gix::objs::WriteTo as _;
use gix::objs::tree::EntryKind;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::{FullName, Target};
use gix::revision::walk::Sorting;
use gix::traverse::commit::simple::CommitTimeOrder;
use imara_diff::{Algorithm, Diff, InternedInput, sources};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

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
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(commit
        .tree_id()
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach())
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
        .map_err(|err| OzError::InvalidUsage {
            message: format!("cannot resolve revision '{trimmed}': {err}"),
            recovery: vec!["Run `oz log --oneline` or `oz branch` to inspect revisions.".to_string()],
        })?;
    let obj = id.object().map_err(|err| OzError::Git(err.to_string()))?;
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
    match trimmed.split_once("\n\n") {
        Some((first, rest)) => (
            first.lines().map(str::trim).collect::<Vec<_>>().join(" "),
            rest.to_string(),
        ),
        None => {
            let mut lines = trimmed.lines();
            let first = lines.next().unwrap_or("").trim().to_string();
            let rest: Vec<&str> = lines.collect();
            (first, rest.join("\n"))
        }
    }
}

/// Extracts `Change-Id:` trailer from the trailing paragraph of a commit message.
#[must_use]
pub fn extract_change_id(message: &str) -> Option<String> {
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
            .map_err(|err| OzError::Git(err.to_string()))?
            .to_owned()
            .map_err(|err| OzError::Git(err.to_string()))?,
        None => gix_repo
            .committer_or_set_fallback("Ozone", "oz@ozone.dev")
            .map_err(|err| OzError::Git(err.to_string()))?
            .to_owned()
            .map_err(|err| OzError::Git(err.to_string()))?,
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
        .map_err(|err| OzError::Git(err.to_string()))?;
    let rhs_tree = gix_repo
        .find_tree(rhs_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;

    let mut state = gix::diff::tree::State::default();
    let mut recorder = gix::diff::tree::Recorder::default()
        .track_location(Some(gix::diff::tree::recorder::Location::Path));
    let lhs_iter = gix::objs::TreeRefIter::from_bytes(&lhs_tree.data, lhs_tree.id.kind());
    let rhs_iter = gix::objs::TreeRefIter::from_bytes(&rhs_tree.data, rhs_tree.id.kind());

    gix::diff::tree(lhs_iter, rhs_iter, &mut state, &gix_repo.objects, &mut recorder)
        .map_err(|err| OzError::Git(err.to_string()))?;

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

    let blob = gix_repo
        .find_blob(oid)
        .map_err(|err| OzError::Git(err.to_string()))?;

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
        .map_err(|err| OzError::Git(err.to_string()))
}

fn load_or_rebuild_index(
    index_path: &Path,
    gix_repo: &gix::Repository,
    target_tree_id: ObjectId,
) -> Result<(gix::index::File, bool)> {
    if !index_path.is_file() {
        let mut rebuilt = gix_repo
            .index_from_tree(&target_tree_id)
            .map_err(|err| OzError::Git(err.to_string()))?;
        write_index_no_ext(&mut rebuilt)?;
        return Ok((rebuilt, true));
    }
    match gix::index::File::at(
        index_path,
        gix_repo.object_hash(),
        true,
        gix::index::decode::Options::default(),
    ) {
        Ok(idx) => Ok((idx, false)),
        Err(_) => {
            let mut rebuilt = gix_repo
                .index_from_tree(&target_tree_id)
                .map_err(|err| OzError::Git(err.to_string()))?;
            write_index_no_ext(&mut rebuilt)?;
            Ok((rebuilt, true))
        }
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
                .map_err(|err| OzError::Git(err.to_string()))?,
        )
    };
    let mut patches = Vec::new();
    for rel in paths {
        let lookup = match target_tree.as_ref() {
            Some(tree) => tree
                .lookup_entry_by_path(rel.as_str())
                .map_err(|err| OzError::Git(err.to_string()))?,
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

/// Pure-Rust Thompson-NFA regular expression matcher used for `oz log -G` and `--grep`.
#[derive(Debug, Clone)]
struct SimpleRegex {
    nodes: Vec<NfaNode>,
    start: usize,
    anchored_start: bool,
    case_insensitive: bool,
}

#[derive(Debug, Clone)]
enum CharMatcher {
    AnyNonNewline,
    Literal(char),
    Whitespace,
    NonWhitespace,
    Digit,
    NonDigit,
    Word,
    NonWord,
    Class { negated: bool, items: Vec<ClassItem> },
}

#[derive(Debug, Clone)]
enum ClassItem {
    Single(char),
    Range(char, char),
}

impl CharMatcher {
    fn matches(&self, c: char, case_insensitive: bool) -> bool {
        match self {
            Self::AnyNonNewline => c != '\n',
            Self::Literal(expected) => {
                if case_insensitive {
                    c.eq_ignore_ascii_case(expected)
                } else {
                    c == *expected
                }
            }
            Self::Whitespace => c.is_whitespace(),
            Self::NonWhitespace => !c.is_whitespace(),
            Self::Digit => c.is_ascii_digit(),
            Self::NonDigit => !c.is_ascii_digit(),
            Self::Word => c.is_ascii_alphanumeric() || c == '_',
            Self::NonWord => !(c.is_ascii_alphanumeric() || c == '_'),
            Self::Class { negated, items } => {
                let target = if case_insensitive {
                    c.to_ascii_lowercase()
                } else {
                    c
                };
                let hit = items.iter().any(|item| match item {
                    ClassItem::Single(x) => {
                        let cx = if case_insensitive {
                            x.to_ascii_lowercase()
                        } else {
                            *x
                        };
                        target == cx
                    }
                    ClassItem::Range(lo, hi) => {
                        let a = if case_insensitive {
                            lo.to_ascii_lowercase()
                        } else {
                            *lo
                        };
                        let b = if case_insensitive {
                            hi.to_ascii_lowercase()
                        } else {
                            *hi
                        };
                        target >= a && target <= b
                    }
                });
                if *negated { !hit } else { hit }
            }
        }
    }
}

#[derive(Debug, Clone)]
enum NfaNode {
    Match,
    EndAnchor(usize),
    Char(CharMatcher, usize),
    Split(usize, usize),
}

#[derive(Debug, Clone)]
struct Frag {
    start: usize,
    outs: Vec<usize>,
}

impl SimpleRegex {
    fn compile(pattern: &str, case_insensitive: bool) -> Result<Self> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut pos = 0usize;
        let anchored_start = chars.first() == Some(&'^');
        if anchored_start {
            pos += 1;
        }
        let mut nodes = Vec::new();
        let frag = Self::parse_alt(&chars, &mut pos, &mut nodes)?;
        if pos != chars.len() {
            return Err(OzError::InvalidUsage {
                message: format!("invalid regex pattern '{pattern}' near position {pos}"),
                recovery: Vec::new(),
            });
        }
        let match_id = nodes.len();
        nodes.push(NfaNode::Match);
        Self::patch(&mut nodes, &frag.outs, match_id);
        Ok(Self {
            nodes,
            start: frag.start,
            anchored_start,
            case_insensitive,
        })
    }

    fn patch(nodes: &mut [NfaNode], outs: &[usize], target: usize) {
        for &idx in outs {
            match &mut nodes[idx] {
                NfaNode::Char(_, next) | NfaNode::EndAnchor(next) => *next = target,
                NfaNode::Split(a, b) => {
                    if *a == usize::MAX {
                        *a = target;
                    }
                    if *b == usize::MAX {
                        *b = target;
                    }
                }
                NfaNode::Match => {}
            }
        }
    }

    fn parse_alt(chars: &[char], pos: &mut usize, nodes: &mut Vec<NfaNode>) -> Result<Frag> {
        let mut lhs = Self::parse_seq(chars, pos, nodes)?;
        while *pos < chars.len() && chars[*pos] == '|' {
            *pos += 1;
            let rhs = Self::parse_seq(chars, pos, nodes)?;
            let split_id = nodes.len();
            nodes.push(NfaNode::Split(lhs.start, rhs.start));
            let mut outs = lhs.outs;
            outs.extend(rhs.outs);
            lhs = Frag {
                start: split_id,
                outs,
            };
        }
        Ok(lhs)
    }

    fn parse_seq(chars: &[char], pos: &mut usize, nodes: &mut Vec<NfaNode>) -> Result<Frag> {
        let mut acc: Option<Frag> = None;
        while *pos < chars.len() && chars[*pos] != ')' && chars[*pos] != '|' {
            let atom = Self::parse_quantified(chars, pos, nodes)?;
            acc = Some(match acc {
                None => atom,
                Some(prev) => {
                    Self::patch(nodes, &prev.outs, atom.start);
                    Frag {
                        start: prev.start,
                        outs: atom.outs,
                    }
                }
            });
        }
        Ok(acc.unwrap_or_else(|| {
            let id = nodes.len();
            nodes.push(NfaNode::Split(usize::MAX, usize::MAX));
            Frag {
                start: id,
                outs: vec![id],
            }
        }))
    }

    fn parse_quantified(
        chars: &[char],
        pos: &mut usize,
        nodes: &mut Vec<NfaNode>,
    ) -> Result<Frag> {
        let base = Self::parse_atom(chars, pos, nodes)?;
        if *pos >= chars.len() {
            return Ok(base);
        }
        match chars[*pos] {
            '*' => {
                *pos += 1;
                let split_id = nodes.len();
                nodes.push(NfaNode::Split(base.start, usize::MAX));
                Self::patch(nodes, &base.outs, split_id);
                Ok(Frag {
                    start: split_id,
                    outs: vec![split_id],
                })
            }
            '+' => {
                *pos += 1;
                let split_id = nodes.len();
                nodes.push(NfaNode::Split(base.start, usize::MAX));
                Self::patch(nodes, &base.outs, split_id);
                Ok(Frag {
                    start: base.start,
                    outs: vec![split_id],
                })
            }
            '?' => {
                *pos += 1;
                let split_id = nodes.len();
                nodes.push(NfaNode::Split(base.start, usize::MAX));
                let mut outs = base.outs;
                outs.push(split_id);
                Ok(Frag {
                    start: split_id,
                    outs,
                })
            }
            _ => Ok(base),
        }
    }

    fn parse_atom(chars: &[char], pos: &mut usize, nodes: &mut Vec<NfaNode>) -> Result<Frag> {
        let ch = chars[*pos];
        match ch {
            '(' => {
                *pos += 1;
                let inner = Self::parse_alt(chars, pos, nodes)?;
                if *pos >= chars.len() || chars[*pos] != ')' {
                    return Err(OzError::InvalidUsage {
                        message: "unclosed '(' in regex pattern".to_string(),
                        recovery: Vec::new(),
                    });
                }
                *pos += 1;
                Ok(inner)
            }
            '$' => {
                *pos += 1;
                let id = nodes.len();
                nodes.push(NfaNode::EndAnchor(usize::MAX));
                Ok(Frag {
                    start: id,
                    outs: vec![id],
                })
            }
            '.' => {
                *pos += 1;
                let id = nodes.len();
                nodes.push(NfaNode::Char(CharMatcher::AnyNonNewline, usize::MAX));
                Ok(Frag {
                    start: id,
                    outs: vec![id],
                })
            }
            '[' => {
                let matcher = Self::parse_class(chars, pos)?;
                let id = nodes.len();
                nodes.push(NfaNode::Char(matcher, usize::MAX));
                Ok(Frag {
                    start: id,
                    outs: vec![id],
                })
            }
            '\\' => {
                *pos += 1;
                if *pos >= chars.len() {
                    return Err(OzError::InvalidUsage {
                        message: "trailing '\\' in regex pattern".to_string(),
                        recovery: Vec::new(),
                    });
                }
                let esc = chars[*pos];
                *pos += 1;
                let matcher = match esc {
                    's' => CharMatcher::Whitespace,
                    'S' => CharMatcher::NonWhitespace,
                    'd' => CharMatcher::Digit,
                    'D' => CharMatcher::NonDigit,
                    'w' => CharMatcher::Word,
                    'W' => CharMatcher::NonWord,
                    'n' => CharMatcher::Literal('\n'),
                    't' => CharMatcher::Literal('\t'),
                    other => CharMatcher::Literal(other),
                };
                let id = nodes.len();
                nodes.push(NfaNode::Char(matcher, usize::MAX));
                Ok(Frag {
                    start: id,
                    outs: vec![id],
                })
            }
            '*' | '+' | '?' => Err(OzError::InvalidUsage {
                message: format!("unexpected quantifier '{ch}' in regex pattern"),
                recovery: Vec::new(),
            }),
            other => {
                *pos += 1;
                let id = nodes.len();
                nodes.push(NfaNode::Char(CharMatcher::Literal(other), usize::MAX));
                Ok(Frag {
                    start: id,
                    outs: vec![id],
                })
            }
        }
    }

    fn parse_class(chars: &[char], pos: &mut usize) -> Result<CharMatcher> {
        *pos += 1; // skip '['
        let mut negated = false;
        if *pos < chars.len() && chars[*pos] == '^' {
            negated = true;
            *pos += 1;
        }
        let mut items = Vec::new();
        while *pos < chars.len() && chars[*pos] != ']' {
            let start_c = if chars[*pos] == '\\' && *pos + 1 < chars.len() {
                *pos += 1;
                let c = chars[*pos];
                *pos += 1;
                c
            } else {
                let c = chars[*pos];
                *pos += 1;
                c
            };
            if *pos + 1 < chars.len() && chars[*pos] == '-' && chars[*pos + 1] != ']' {
                *pos += 1; // skip '-'
                let end_c = if chars[*pos] == '\\' && *pos + 1 < chars.len() {
                    *pos += 1;
                    let c = chars[*pos];
                    *pos += 1;
                    c
                } else {
                    let c = chars[*pos];
                    *pos += 1;
                    c
                };
                items.push(ClassItem::Range(start_c, end_c));
            } else {
                items.push(ClassItem::Single(start_c));
            }
        }
        if *pos >= chars.len() || chars[*pos] != ']' {
            return Err(OzError::InvalidUsage {
                message: "unclosed '[' character class in regex pattern".to_string(),
                recovery: Vec::new(),
            });
        }
        *pos += 1;
        Ok(CharMatcher::Class { negated, items })
    }

    fn is_match(&self, text: &str) -> bool {
        if self.anchored_start {
            return self.matches_at(text);
        }
        if self.matches_at(text) {
            return true;
        }
        for (idx, _) in text.char_indices() {
            if idx > 0 && self.matches_at(&text[idx..]) {
                return true;
            }
        }
        false
    }

    fn matches_at(&self, text: &str) -> bool {
        let mut current = Vec::new();
        let mut visited = vec![false; self.nodes.len()];
        self.add_state(self.start, text.is_empty(), &mut current, &mut visited);
        if current.iter().any(|&s| matches!(self.nodes[s], NfaNode::Match)) {
            return true;
        }

        let mut iter = text.chars().peekable();
        while let Some(c) = iter.next() {
            let at_end_after = iter.peek().is_none();
            let mut next_states = Vec::new();
            visited.fill(false);
            for &state in &current {
                if let NfaNode::Char(ref matcher, next) = self.nodes[state]
                    && matcher.matches(c, self.case_insensitive)
                {
                    self.add_state(next, at_end_after, &mut next_states, &mut visited);
                }
            }
            if next_states.is_empty() {
                return false;
            }
            if next_states
                .iter()
                .any(|&s| matches!(self.nodes[s], NfaNode::Match))
            {
                return true;
            }
            current = next_states;
        }
        false
    }

    fn add_state(
        &self,
        idx: usize,
        at_end: bool,
        out: &mut Vec<usize>,
        visited: &mut [bool],
    ) {
        if idx >= self.nodes.len() || visited[idx] {
            return;
        }
        visited[idx] = true;
        match self.nodes[idx] {
            NfaNode::Split(a, b) => {
                self.add_state(a, at_end, out, visited);
                self.add_state(b, at_end, out, visited);
            }
            NfaNode::EndAnchor(next) => {
                if at_end {
                    self.add_state(next, at_end, out, visited);
                }
            }
            NfaNode::Char(_, _) | NfaNode::Match => out.push(idx),
        }
    }
}

/// Parses `--since` timestamps (RFC3339, `YYYY-MM-DD`, Unix epoch, or relative `"2 days ago"` / `"12h"`).
pub fn parse_since_timestamp(raw: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "--since value cannot be empty".to_string(),
            recovery: vec!["Pass e.g. `--since \"2 days ago\"` or `--since 2026-11-20`.".to_string()],
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

    let without_ago = lower.strip_suffix("ago").map(str::trim).unwrap_or(&lower);
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
    pub all: bool,
    pub limit: Option<usize>,
    pub author: Option<String>,
    pub grep: Option<String>,
    pub search_string: Option<String>,
    pub search_diff_regex: Option<String>,
    pub since: Option<String>,
    pub graph: bool,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            rev: None,
            all: false,
            limit: None,
            author: None,
            grep: None,
            search_string: None,
            search_diff_regex: None,
            since: None,
            graph: true,
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
        .map(|pat| SimpleRegex::compile(pat, false))
        .transpose()?;
    let since_cutoff = opts
        .since
        .as_deref()
        .map(|s| parse_since_timestamp(s, Utc::now()))
        .transpose()?;

    let (starts, hidden, target_label) = resolve_log_bounds(repo, &gix_repo, opts)?;
    if starts.is_empty() {
        return Ok(LogReport {
            target_rev: target_label,
            unborn: true,
            total_returned: 0,
            commits: Vec::new(),
        });
    }

    let ref_decorations = collect_ref_decorations(repo, &gix_repo)?;
    let has_content_filter = opts.author.is_some()
        || opts.grep.is_some()
        || opts.search_string.is_some()
        || opts.search_diff_regex.is_some()
        || since_cutoff.is_some();

    let mut walk_platform = gix_repo
        .rev_walk(starts)
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst));
    if !hidden.is_empty() {
        walk_platform = walk_platform.with_hidden(hidden);
    }
    let walk = walk_platform
        .all()
        .map_err(|err| OzError::Git(err.to_string()))?;

    let mut commits = Vec::new();
    let mut lanes: Vec<ObjectId> = Vec::new();

    for item in walk {
        if let Some(limit) = opts.limit
            && commits.len() >= limit
        {
            break;
        }
        let info = item.map_err(|err| OzError::Git(err.to_string()))?;
        let commit_id = info.id;
        let commit = gix_repo
            .find_commit(commit_id)
            .map_err(|err| OzError::Git(err.to_string()))?;
        let decoded = commit
            .decode()
            .map_err(|err| OzError::Git(err.to_string()))?;

        let author = decoded
            .author()
            .map_err(|err| OzError::Git(err.to_string()))?;
        let committer = decoded
            .committer()
            .map_err(|err| OzError::Git(err.to_string()))?;

        let author_name = author.name.to_str_lossy().into_owned();
        let author_email = author.email.to_str_lossy().into_owned();
        let author_time = gix_time_to_utc(author.time().map_err(|err| OzError::Git(err.to_string()))?);
        let committer_name = committer.name.to_str_lossy().into_owned();
        let committer_email = committer.email.to_str_lossy().into_owned();
        let committer_time =
            gix_time_to_utc(committer.time().map_err(|err| OzError::Git(err.to_string()))?);
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

        commits.push(LogCommitEntry {
            oid: oid_hex,
            short_oid,
            tree_oid: tree_id.to_hex().to_string(),
            parents: parent_ids
                .iter()
                .map(|p| p.to_hex().to_string())
                .collect(),
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

fn resolve_log_bounds(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    opts: &LogOptions,
) -> Result<(Vec<ObjectId>, Vec<ObjectId>, Option<String>)> {
    if opts.all {
        let mut starts = Vec::new();
        let platform = gix_repo
            .references()
            .map_err(|err| OzError::Git(err.to_string()))?;
        for item in platform
            .local_branches()
            .map_err(|err| OzError::Git(err.to_string()))?
        {
            let mut r = item.map_err(|err| OzError::Git(err.to_string()))?;
            if let Ok(c) = r.peel_to_commit() {
                starts.push(c.id);
            }
        }
        for item in platform
            .tags()
            .map_err(|err| OzError::Git(err.to_string()))?
        {
            let mut r = item.map_err(|err| OzError::Git(err.to_string()))?;
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
        return Ok((starts, Vec::new(), Some("--all".to_string())));
    }

    let Some(ref raw_rev) = opts.rev else {
        let head = resolve_workspace_head(repo, gix_repo)?;
        return match head.commit_oid {
            Some(cid) => Ok((vec![cid], Vec::new(), head.branch.or(Some("HEAD".to_string())))),
            None => Ok((Vec::new(), Vec::new(), head.branch)),
        };
    };

    if let Some((lhs, rhs)) = raw_rev.split_once("..")
        && !rhs.starts_with('.')
    {
        let left_spec = if lhs.trim().is_empty() { "HEAD" } else { lhs.trim() };
        let right_spec = if rhs.trim().is_empty() { "HEAD" } else { rhs.trim() };
        let left_oid = resolve_commit_oid(gix_repo, left_spec)?;
        let right_oid = resolve_commit_oid(gix_repo, right_spec)?;
        return Ok((vec![right_oid], vec![left_oid], Some(raw_rev.clone())));
    }

    let oid = resolve_commit_oid(gix_repo, raw_rev)?;
    Ok((vec![oid], Vec::new(), Some(raw_rev.clone())))
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

    let platform = gix_repo
        .references()
        .map_err(|err| OzError::Git(err.to_string()))?;

    for item in platform
        .local_branches()
        .map_err(|err| OzError::Git(err.to_string()))?
    {
        let mut r = item.map_err(|err| OzError::Git(err.to_string()))?;
        let name = r.name().shorten().to_str_lossy().into_owned();
        if head.branch.as_deref() == Some(name.as_str()) {
            continue;
        }
        if let Ok(c) = r.peel_to_commit() {
            map.entry(c.id).or_default().push(name);
        }
    }

    for item in platform
        .tags()
        .map_err(|err| OzError::Git(err.to_string()))?
    {
        let mut r = item.map_err(|err| OzError::Git(err.to_string()))?;
        let name = r.name().shorten().to_str_lossy().into_owned();
        if let Ok(c) = r.peel_to_commit() {
            map.entry(c.id).or_default().push(format!("tag: {name}"));
        }
    }

    for item in platform
        .remote_branches()
        .map_err(|err| OzError::Git(err.to_string()))?
    {
        let mut r = item.map_err(|err| OzError::Git(err.to_string()))?;
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
    let col = match lanes.iter().position(|id| *id == commit_id) {
        Some(idx) => idx,
        None => {
            lanes.push(commit_id);
            lanes.len() - 1
        }
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
        let mut insert_at = col + 1;
        for &p in &parents[1..] {
            if !lanes.contains(&p) {
                lanes.insert(insert_at, p);
                insert_at += 1;
            }
        }
    }

    // Deduplicate converging lanes.
    let mut seen = HashSet::new();
    lanes.retain(|id| seen.insert(*id));

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

fn commit_diff_matches_filters(
    gix_repo: &gix::Repository,
    parent_tree_id: ObjectId,
    commit_tree_id: ObjectId,
    search_string: Option<&str>,
    diff_re: Option<&SimpleRegex>,
) -> Result<bool> {
    if parent_tree_id == commit_tree_id {
        return Ok(false);
    }
    let lhs_tree = gix_repo
        .find_tree(parent_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let rhs_tree = gix_repo
        .find_tree(commit_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;

    let mut state = gix::diff::tree::State::default();
    let mut recorder = gix::diff::tree::Recorder::default()
        .track_location(Some(gix::diff::tree::recorder::Location::Path));
    let lhs_iter = gix::objs::TreeRefIter::from_bytes(&lhs_tree.data, lhs_tree.id.kind());
    let rhs_iter = gix::objs::TreeRefIter::from_bytes(&rhs_tree.data, rhs_tree.id.kind());

    gix::diff::tree(lhs_iter, rhs_iter, &mut state, &gix_repo.objects, &mut recorder)
        .map_err(|err| OzError::Git(err.to_string()))?;

    for record in recorder.records {
        let (old_oid, new_oid) = match record {
            gix::diff::tree::recorder::Change::Addition {
                entry_mode, oid, ..
            } => {
                if entry_mode.is_tree() || entry_mode.kind() == EntryKind::Commit {
                    continue;
                }
                (None, Some(oid))
            }
            gix::diff::tree::recorder::Change::Deletion {
                entry_mode, oid, ..
            } => {
                if entry_mode.is_tree() || entry_mode.kind() == EntryKind::Commit {
                    continue;
                }
                (Some(oid), None)
            }
            gix::diff::tree::recorder::Change::Modification {
                previous_entry_mode,
                previous_oid,
                entry_mode,
                oid,
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
                (prev, Some(oid))
            }
        };

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

    let resolved_id = match rev_spec {
        Some(spec) => gix_repo
            .rev_parse_single(spec)
            .map_err(|err| OzError::InvalidUsage {
                message: format!("cannot resolve revision '{spec}': {err}"),
                recovery: vec!["Run `oz log --oneline` to inspect revisions.".to_string()],
            })?
            .detach(),
        None => {
            let head = resolve_workspace_head(repo, &gix_repo)?;
            head.commit_oid.ok_or_else(|| OzError::InvalidUsage {
                message: "repository has no commits yet (HEAD is unborn)".to_string(),
                recovery: vec!["Create an initial commit with `oz commit -m \"...\"`.".to_string()],
            })?
        }
    };

    let obj = gix_repo
        .find_object(resolved_id)
        .map_err(|err| OzError::Git(err.to_string()))?;

    let (tag_name, tag_message) = if obj.kind == gix::object::Kind::Tag {
        let tag_ref = gix::objs::TagRef::from_bytes(&obj.data, gix_repo.object_hash())
            .map_err(|err| OzError::Git(err.to_string()))?;
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
    let decoded = commit
        .decode()
        .map_err(|err| OzError::Git(err.to_string()))?;

    let author = decoded
        .author()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let committer = decoded
        .committer()
        .map_err(|err| OzError::Git(err.to_string()))?;

    let author_name = author.name.to_str_lossy().into_owned();
    let author_email = author.email.to_str_lossy().into_owned();
    let author_time = gix_time_to_utc(author.time().map_err(|err| OzError::Git(err.to_string()))?);
    let committer_name = committer.name.to_str_lossy().into_owned();
    let committer_email = committer.email.to_str_lossy().into_owned();
    let committer_time =
        gix_time_to_utc(committer.time().map_err(|err| OzError::Git(err.to_string()))?);
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

    let parents: Vec<ObjectId> = commit.parent_ids().map(|id| id.detach()).collect();
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
        .map_err(|err| OzError::Git(err.to_string()))?;
    let rhs_tree = gix_repo
        .find_tree(rhs_tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;

    let mut state = gix::diff::tree::State::default();
    let mut recorder = gix::diff::tree::Recorder::default()
        .track_location(Some(gix::diff::tree::recorder::Location::Path));
    let lhs_iter = gix::objs::TreeRefIter::from_bytes(&lhs_tree.data, lhs_tree.id.kind());
    let rhs_iter = gix::objs::TreeRefIter::from_bytes(&rhs_tree.data, rhs_tree.id.kind());
    gix::diff::tree(lhs_iter, rhs_iter, &mut state, &gix_repo.objects, &mut recorder)
        .map_err(|err| OzError::Git(err.to_string()))?;

    struct RawTreeChange {
        path: String,
        kind: ChangeKind,
        old_bytes: Vec<u8>,
        new_bytes: Vec<u8>,
    }

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

/// Outcome returned by [`stash_pop`].
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
    live_tree_id: ObjectId,
    merged_tree_id: ObjectId,
    delta: Vec<TreeDeltaOp>,
}

/// Parses a stash selector such as `"stash@{0}"` or `"0"` (defaulting to `0`).
pub fn parse_stash_selector(spec: Option<&str>) -> Result<usize> {
    let Some(raw) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(0);
    };
    if let Some(inner) = raw.strip_prefix("stash@{").and_then(|s| s.strip_suffix('}')) {
        return inner.trim().parse::<usize>().map_err(|_| OzError::InvalidUsage {
            message: format!("invalid stash reference '{raw}'"),
            recovery: vec!["Run `oz stash list` to see available stashes.".to_string()],
        });
    }
    raw.parse::<usize>().map_err(|_| OzError::InvalidUsage {
        message: format!("invalid stash reference '{raw}'"),
        recovery: vec!["Pass `stash@{0}` or a numeric index like `0`.".to_string()],
    })
}

fn read_stash_manifest(gix_repo: &gix::Repository) -> Result<(Option<ObjectId>, Vec<StashEntryRecord>)> {
    let Some(mut reference) = gix_repo
        .try_find_reference(OZ_STASH_REF)
        .map_err(|err| OzError::Git(err.to_string()))?
    else {
        return Ok((None, Vec::new()));
    };
    let commit = reference
        .peel_to_commit()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let tree = commit.tree().map_err(|err| OzError::Git(err.to_string()))?;
    let Some(entry) = tree
        .lookup_entry_by_path("manifest.json")
        .map_err(|err| OzError::Git(err.to_string()))?
    else {
        return Ok((Some(commit.id), Vec::new()));
    };
    let blob = gix_repo
        .find_blob(entry.object_id())
        .map_err(|err| OzError::Git(err.to_string()))?;
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
            .map_err(|err| OzError::Git(err.to_string()))?;
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
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();

    let mut editor = gix_repo
        .edit_tree(empty_tree)
        .map_err(|err| OzError::Git(err.to_string()))?;
    editor
        .upsert("manifest.json", EntryKind::Blob, manifest_blob)
        .map_err(|err| OzError::Git(err.to_string()))?;

    for rec in entries {
        let dirty_oid = parse_oid(&rec.dirty_tree_oid)?;
        if dirty_oid != empty_tree {
            let p = format!("snapshots/{}/dirty", rec.id);
            editor
                .upsert(p.as_str(), EntryKind::Tree, dirty_oid)
                .map_err(|err| OzError::Git(err.to_string()))?;
        }
        let base_oid = parse_oid(&rec.base_tree_oid)?;
        if base_oid != empty_tree {
            let p = format!("snapshots/{}/base", rec.id);
            editor
                .upsert(p.as_str(), EntryKind::Tree, base_oid)
                .map_err(|err| OzError::Git(err.to_string()))?;
        }
    }

    let tree_id = editor
        .write()
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();
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
        .map_err(|err| OzError::Git(err.to_string()))?;

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
        .map_err(|err| OzError::Git(err.to_string()))?;
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

fn build_dirty_tree_from_workdir(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
) -> Result<(crate::status::WorkspaceHead, ObjectId, Vec<String>)> {
    let ctx = compute_status_context(repo)?;
    let mut editor = gix_repo
        .edit_tree(ctx.head.tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let mut paths = Vec::with_capacity(ctx.dirty_files.len());

    for raw in &ctx.dirty_files {
        paths.push(raw.path.clone());
        match raw.kind {
            ChangeKind::Deleted => {
                editor
                    .remove(raw.path.as_str())
                    .map_err(|err| OzError::Git(err.to_string()))?;
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
                    .map_err(|err| OzError::Git(err.to_string()))?
                    .detach();
                editor
                    .upsert(raw.path.as_str(), entry_kind, blob_id)
                    .map_err(|err| OzError::Git(err.to_string()))?;
            }
        }
    }

    let dirty_tree_id = editor
        .write()
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();
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
                .and_then(|c| c.decode().ok().map(|d| split_subject_and_body(&d.message.to_str_lossy()).0))
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

/// Performs read-only 3-way merge validation for `oz stash pop` before opening an OpLog transaction.
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

    let merge_opts = gix_repo
        .tree_merge_options()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let stash_label = format!("stash@{{{index}}}");
    let labels = gix::merge::blob::builtin_driver::text::Labels {
        ancestor: Some("stash-base".into()),
        current: Some("working-tree".into()),
        other: Some(stash_label.as_str().into()),
    };
    let mut outcome = gix_repo
        .merge_trees(stash_base, live_tree_id, stash_dirty, labels, merge_opts)
        .map_err(|err| OzError::Git(err.to_string()))?;

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

    let merged_tree_id = outcome
        .tree
        .write()
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();
    let delta = compute_tree_delta(&gix_repo, live_tree_id, merged_tree_id)?;

    Ok(PreparedStashPop {
        index,
        record: rec,
        live_tree_id,
        merged_tree_id,
        delta,
    })
}

/// Applies a validated [`PreparedStashPop`] to the working tree and pops the entry from `refs/ozone/stash`.
pub fn apply_stash_pop(repo: &OzoneRepo, prepared: PreparedStashPop) -> Result<StashPopOutcome> {
    let mut gix_repo = open_gix_for_workspace(repo)?;
    let _ = prepared.live_tree_id;
    materialize_tree_delta(repo, &gix_repo, &prepared.delta)?;

    let applied_paths: Vec<String> = prepared
        .delta
        .iter()
        .map(|d| d.path().to_string())
        .collect();
    let head = resolve_workspace_head(repo, &gix_repo)?;
    sync_index_paths_to_tree(repo, &gix_repo, head.tree_id, &applied_paths)?;

    let (prev_commit, mut entries) = read_stash_manifest(&gix_repo)?;
    if prepared.index < entries.len() && entries[prepared.index].id == prepared.record.id {
        entries.remove(prepared.index);
    } else if let Some(pos) = entries.iter().position(|e| e.id == prepared.record.id) {
        entries.remove(pos);
    }
    write_stash_manifest(&mut gix_repo, prev_commit, &entries, "oz stash pop")?;

    let _ = prepared.merged_tree_id;

    Ok(StashPopOutcome {
        stash: to_stash_item(prepared.index, &prepared.record),
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
    let parents: Vec<ObjectId> = commit.parent_ids().map(|id| id.detach()).collect();
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
            message: format!("mainline parent {m} invalid for non-merge commit {}", commit.id),
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
        let mut unsigned_payload = Vec::with_capacity(commit_obj.size() as usize);
        commit_obj
            .write_to(&mut unsigned_payload)
            .map_err(|err| OzError::Git(err.to_string()))?;
        let armored = priv_key.sign_sshsig(&unsigned_payload, GIT_SSHSIG_NAMESPACE)?;
        let fp = priv_key.public_key().fingerprint_sha256();
        let sig_field = gix::objs::commit::signature_field_name(gix_repo.object_hash());
        commit_obj.extra_headers.push((sig_field.into(), armored.into()));
        (true, Some(fp))
    } else {
        (false, None)
    };

    let commit_id = gix_repo
        .write_object(&commit_obj)
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();

    let ref_full_name =
        FullName::try_from(spec.target_ref.as_str()).map_err(|err| OzError::Git(err.to_string()))?;
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
        .map_err(|err| OzError::Git(err.to_string()))?;

    Ok((commit_id, signed, fingerprint))
}

/// Applies the patch introduced by `opts.rev` onto current `HEAD` via pure-Rust 3-way tree merge.
pub fn cherry_pick_commit(repo: &OzoneRepo, opts: &CherryPickOptions) -> Result<CherryPickOutcome> {
    let mut gix_repo = open_gix_for_workspace(repo)?;
    let source_oid = resolve_commit_oid(&gix_repo, &opts.rev)?;
    let (source_tree, parent_tree, orig_author, orig_message) = {
        let source_commit = gix_repo
            .find_commit(source_oid)
            .map_err(|err| OzError::Git(err.to_string()))?;
        let source_tree = source_commit
            .tree_id()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach();
        let parent_tree = resolve_commit_parent_tree(&gix_repo, &source_commit, opts.mainline)?;

        let decoded = source_commit
            .decode()
            .map_err(|err| OzError::Git(err.to_string()))?;
        let orig_author = decoded
            .author()
            .map_err(|err| OzError::Git(err.to_string()))?
            .to_owned()
            .map_err(|err| OzError::Git(err.to_string()))?;
        let orig_message = decoded.message.to_str_lossy().into_owned();
        (source_tree, parent_tree, orig_author, orig_message)
    };

    let head = resolve_workspace_head(repo, &gix_repo)?;
    let head_tree = head.tree_id;
    let merge_opts = gix_repo
        .tree_merge_options()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let short_src = source_oid.to_hex().to_string();
    let short_src_7 = &short_src[..short_src.len().min(7)];
    let labels = gix::merge::blob::builtin_driver::text::Labels {
        ancestor: Some("parent".into()),
        current: Some(head.branch.as_deref().unwrap_or("HEAD").into()),
        other: Some(short_src_7.into()),
    };

    let mut outcome = gix_repo
        .merge_trees(parent_tree, head_tree, source_tree, labels, merge_opts)
        .map_err(|err| OzError::Git(err.to_string()))?;

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

    let new_tree_id = outcome
        .tree
        .write()
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();
    let delta = compute_tree_delta(&gix_repo, head_tree, new_tree_id)?;
    check_dirty_collision(repo, &delta, "cherry-pick")?;

    let applied_paths: Vec<String> = delta.iter().map(|d| d.path().to_string()).collect();

    let (_, committer) = resolve_actor_signatures(&mut gix_repo)?;
    let mut raw_msg = orig_message.trim_end_matches(['\r', '\n']).to_string();
    if opts.append_origin {
        raw_msg.push_str(&format!("\n\n(cherry picked from commit {short_src})"));
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
            .map_err(|err| OzError::Git(err.to_string()))?;
        let target_tree = target_commit
            .tree_id()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach();
        let parent_tree = resolve_commit_parent_tree(&gix_repo, &target_commit, opts.mainline)?;

        let decoded = target_commit
            .decode()
            .map_err(|err| OzError::Git(err.to_string()))?;
        let (orig_subject, _) = split_subject_and_body(&decoded.message.to_str_lossy());
        (target_tree, parent_tree, orig_subject)
    };

    let head = resolve_workspace_head(repo, &gix_repo)?;
    let head_tree = head.tree_id;
    let merge_opts = gix_repo
        .tree_merge_options()
        .map_err(|err| OzError::Git(err.to_string()))?;
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
        .map_err(|err| OzError::Git(err.to_string()))?;

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

    let new_tree_id = outcome
        .tree
        .write()
        .map_err(|err| OzError::Git(err.to_string()))?
        .detach();
    let delta = compute_tree_delta(&gix_repo, head_tree, new_tree_id)?;
    check_dirty_collision(repo, &delta, "revert")?;

    let applied_paths: Vec<String> = delta.iter().map(|d| d.path().to_string()).collect();

    let (author, committer) = resolve_actor_signatures(&mut gix_repo)?;
    let raw_msg = format!("Revert \"{orig_subject}\"\n\nThis reverts commit {target_hex}.");
    let parents: Vec<ObjectId> = head.commit_oid.into_iter().collect();
    let (final_msg, change_id) =
        ensure_change_id_trailer(&raw_msg, new_tree_id, &parents, &author);

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

    let platform = gix_repo
        .references()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let mut tags = Vec::new();
    for item in platform
        .tags()
        .map_err(|err| OzError::Git(err.to_string()))?
    {
        let mut r = item.map_err(|err| OzError::Git(err.to_string()))?;
        let name = r.name().shorten().to_str_lossy().into_owned();
        if let Some(ref matcher) = glob_matcher
            && !matcher.is_match(&name)
        {
            continue;
        }
        let direct_id = match r.target() {
            gix::refs::TargetRef::Object(id) => id.to_owned(),
            gix::refs::TargetRef::Symbolic(_) => r
                .peel_to_id()
                .map_err(|err| OzError::Git(err.to_string()))?
                .detach(),
        };
        let obj = gix_repo
            .find_object(direct_id)
            .map_err(|err| OzError::Git(err.to_string()))?;
        if obj.kind == gix::object::Kind::Tag {
            let tag_ref = gix::objs::TagRef::from_bytes(&obj.data, gix_repo.object_hash())
                .map_err(|err| OzError::Git(err.to_string()))?;
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
                ("signed".to_string(), (!body.is_empty()).then_some(body.to_string()))
            } else {
                let body = msg_text.trim();
                ("annotated".to_string(), (!body.is_empty()).then_some(body.to_string()))
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
        .map_err(|err| OzError::Git(err.to_string()))?;
    if existing.is_some() && !opts.force {
        return Err(OzError::InvalidUsage {
            message: format!("tag '{}' already exists", opts.name),
            recovery: vec![format!("Pass `-f` / `--force` to overwrite tag '{}'.", opts.name)],
        });
    }

    let target_oid = match opts.target_rev.as_deref() {
        Some(spec) => resolve_commit_oid(&gix_repo, spec)?,
        None => {
            let head = resolve_workspace_head(repo, &gix_repo)?;
            head.commit_oid.ok_or_else(|| OzError::InvalidUsage {
                message: "cannot create tag on unborn HEAD".to_string(),
                recovery: vec!["Create an initial commit first.".to_string()],
            })?
        }
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
            .map_err(|err| OzError::Git(err.to_string()))?;
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
            recovery: vec![format!("Run `oz tag -s {} -m \"Release {}\"`.", opts.name, opts.name)],
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
            .map_err(|err| OzError::Git(err.to_string()))?;
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
        .map_err(|err| OzError::Git(err.to_string()))?
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
        .map_err(|err| OzError::Git(err.to_string()))?;

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
        .map_err(|err| OzError::Git(err.to_string()))?
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!("tag '{name}' not found"),
            recovery: vec!["Run `oz tag` to list existing tags.".to_string()],
        })?;
    let deleted_oid = match existing.target() {
        gix::refs::TargetRef::Object(id) => id.to_owned(),
        gix::refs::TargetRef::Symbolic(_) => existing
            .into_fully_peeled_id()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach(),
    };

    gix_repo
        .edit_references([RefEdit::delete(
            full_name,
            PreviousValue::MustExistAndMatch(Target::Object(deleted_oid)),
        )])
        .map_err(|err| OzError::Git(err.to_string()))?;

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
        .map_err(|err| OzError::Git(err.to_string()))?
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!("tag '{name}' not found"),
            recovery: vec!["Run `oz tag` to list tags.".to_string()],
        })?;
    let direct_id = match reference.target() {
        gix::refs::TargetRef::Object(id) => id.to_owned(),
        gix::refs::TargetRef::Symbolic(_) => reference
            .peel_to_id()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach(),
    };
    let obj = gix_repo
        .find_object(direct_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    if obj.kind != gix::object::Kind::Tag {
        return Err(OzError::CheckFailed {
            message: format!("tag '{name}' is a lightweight tag and cannot carry a signature"),
            recovery: vec![format!("Create a signed tag with `oz tag -s {name} -m \"...\"`.")],
        });
    }
    let Some((sig_ref, signed_data)) = gix::objs::TagRefIter::signature(&obj.data) else {
        return Err(OzError::CheckFailed {
            message: format!("tag '{name}' is not signed"),
            recovery: Vec::new(),
        });
    };
    let tag_ref = gix::objs::TagRef::from_bytes(&obj.data, gix_repo.object_hash())
        .map_err(|err| OzError::Git(err.to_string()))?;
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

/// Summary entry for a single local branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    pub is_current: bool,
    pub head_oid: String,
    pub short_oid: String,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
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
    let gix_repo = open_gix_for_workspace(repo)?;
    let head = resolve_workspace_head(repo, &gix_repo)?;
    let ws_by_branch = map_workspace_branches(repo);
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
                    message: format!("invalid branch pattern '{p}': {err}"),
                    recovery: Vec::new(),
                })
        })
        .transpose()?;

    let platform = gix_repo
        .references()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let mut branches = Vec::new();
    for item in platform
        .local_branches()
        .map_err(|err| OzError::Git(err.to_string()))?
    {
        let mut r = item.map_err(|err| OzError::Git(err.to_string()))?;
        let name = r.name().shorten().to_str_lossy().into_owned();
        if let Some(ref matcher) = glob_matcher
            && !matcher.is_match(&name)
        {
            continue;
        }
        let commit = r
            .peel_to_commit()
            .map_err(|err| OzError::Git(err.to_string()))?;
        let oid_hex = commit.id.to_hex().to_string();
        let short_oid = oid_hex[..oid_hex.len().min(7)].to_string();
        let subject = commit
            .decode()
            .ok()
            .map(|d| split_subject_and_body(&d.message.to_str_lossy()).0)
            .unwrap_or_default();
        let is_current = head.branch.as_deref() == Some(name.as_str());
        branches.push(BranchInfo {
            workspace: ws_by_branch.get(&name).cloned(),
            name,
            is_current,
            head_oid: oid_hex,
            short_oid,
            subject,
        });
    }
    branches.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(BranchListReport {
        current_branch: head.branch,
        count: branches.len(),
        branches,
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
        .map_err(|err| OzError::Git(err.to_string()))?;
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

    let oid = match start_point {
        Some(spec) => resolve_commit_oid(&gix_repo, spec)?,
        None => {
            let head = resolve_workspace_head(repo, &gix_repo)?;
            head.commit_oid.ok_or_else(|| OzError::InvalidUsage {
                message: "cannot create branch on unborn HEAD".to_string(),
                recovery: vec!["Create an initial commit first.".to_string()],
            })?
        }
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
        .map_err(|err| OzError::Git(err.to_string()))?;

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
            message: format!(
                "cannot rename to '{new_name}': checked out in worktree '{other_wt}'"
            ),
            conflicts: Vec::new(),
            recovery: Vec::new(),
        });
    }

    let old_ref_str = old_full.as_bstr().to_str_lossy().into_owned();
    let new_ref_str = new_full.as_bstr().to_str_lossy().into_owned();

    let mut old_ref = gix_repo
        .try_find_reference(&old_ref_str)
        .map_err(|err| OzError::Git(err.to_string()))?
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!("branch '{resolved_old}' not found"),
            recovery: vec!["Run `oz branch` to list existing branches.".to_string()],
        })?;
    let oid = old_ref
        .peel_to_commit()
        .map_err(|err| OzError::Git(err.to_string()))?
        .id;

    let new_exists = gix_repo
        .try_find_reference(&new_ref_str)
        .map_err(|err| OzError::Git(err.to_string()))?
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
            name: FullName::try_from("HEAD").expect("HEAD is valid"),
            deref: false,
        });
    }

    edits.push(RefEdit::delete(
        old_full,
        PreviousValue::MustExistAndMatch(Target::Object(oid)),
    ));

    gix_repo
        .edit_references(edits)
        .map_err(|err| OzError::Git(err.to_string()))?;

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
            recovery: vec!["Switch to another branch first with `oz git switch <other>`.".to_string()],
        });
    }
    if let Some(other_wt) = find_worktree_using_branch(repo, &full_name)? {
        return Err(OzError::Conflict {
            message: format!("cannot delete branch '{name}': checked out in worktree '{other_wt}'"),
            conflicts: Vec::new(),
            recovery: vec![format!("Remove the worktree or switch it off '{name}' first.")],
        });
    }

    let ref_str = full_name.as_bstr().to_str_lossy().into_owned();
    let mut branch_ref = gix_repo
        .try_find_reference(&ref_str)
        .map_err(|err| OzError::Git(err.to_string()))?
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!("branch '{name}' not found"),
            recovery: vec!["Run `oz branch` to list local branches.".to_string()],
        })?;
    let branch_oid = branch_ref
        .peel_to_commit()
        .map_err(|err| OzError::Git(err.to_string()))?
        .id;

    if !force && let Some(head_oid) = head.commit_oid {
        let merged = if branch_oid == head_oid {
            true
        } else {
            gix_repo
                .merge_base(branch_oid, head_oid)
                .map_err(|err| OzError::Git(err.to_string()))?
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
        .map_err(|err| OzError::Git(err.to_string()))?;

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
    let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
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
    let tree = gix_repo
        .find_tree(tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    for entry_res in tree.iter() {
        let entry = entry_res.map_err(|err| OzError::Git(err.to_string()))?;
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
            message: "oz restore requires at least one path or file ID (e.g. `oz restore README.md`)".to_string(),
            recovery: vec!["Run `oz status` to list dirty files and IDs (`f1`, `f2`, ...).".to_string()],
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
                .map_err(|err| OzError::Git(err.to_string()))?,
        )
    };

    let needs_status = opts.targets.iter().any(|t| looks_like_file_id(t.trim()));
    let file_id_map: HashMap<String, String> = if needs_status {
        let status = crate::status::compute_status(repo)?;
        status
            .changes
            .into_iter()
            .map(|c| (c.id, c.path))
            .collect()
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

        let resolved_raw = if looks_like_file_id(trimmed)
            && !repo.workdir_path.join(trimmed).exists()
        {
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
                .map_err(|err| OzError::Git(err.to_string()))?,
            None => None,
        };

        match lookup {
            Some(entry) => {
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
            }
            None => {
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
    }

    let mut restored_paths: Vec<String> = Vec::with_capacity(to_restore.len());
    let mut restore_entries: Vec<(String, ObjectId, EntryKind)> =
        to_restore.into_iter().map(|(p, (o, k))| (p, o, k)).collect();
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
            .map(|id| id.to_hex().to_string())
            .unwrap_or_else(|| "unborn".to_string()),
        restored_paths,
        removed_paths,
    })
}

/// Convenience helper returning `compute_change_id` usage check (kept re-exported cleanly).
#[doc(hidden)]
pub fn compute_daily_change_id(
    tree_id: ObjectId,
    parents: &[ObjectId],
    author: &gix::actor::Signature,
    message: &str,
) -> String {
    compute_change_id(tree_id, parents, author, message)
}

/// Resolves the canonical repository root for diagnostics.
#[must_use]
pub fn daily_repo_root(repo: &OzoneRepo) -> PathBuf {
    repo.repo_root.clone()
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
