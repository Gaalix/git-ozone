// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pre/post mutation transaction wrapper capturing `ref_deltas` and `ws_snapshots`.

use crate::dirty::{
    SnapshotSlice, build_dirty_tree, ensure_mount_healthy, make_ws_snapshot, open_gix_for_workspace,
    read_ref_oid_hex, resolve_workspace_head, scan_dirty_files, snapshots_match,
    sync_workspace_state_file, workspace_key,
};
use crate::new_op_id;
use crate::store::{append_entry, ensure_oplog_dirs};
use crate::types::{ActorInfo, DirtyFileSnapshot, OpKind, OpLogEntry, RefDelta};
use chrono::Utc;
use gix::bstr::ByteSlice;
use gix::hash::ObjectId;
use oz_core::{FileChange, OzError, OzoneRepo, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Pre-mutation snapshot captured before executing a mutating command (`oz commit`, `oz ws spawn`, etc.).
#[derive(Debug, Clone)]
pub struct MutationTransaction {
    command: String,
    actor: ActorInfo,
    ws_key: String,
    watch_all_branches: bool,
    watch_remote_refs: bool,
    watched_refs: Vec<String>,
    pre_refs: BTreeMap<String, String>,
    pre_branch: Option<String>,
    pre_head_commit: Option<ObjectId>,
    pre_base_tree: ObjectId,
    pre_dirty_tree: ObjectId,
    pre_dirty_files: Vec<DirtyFileSnapshot>,
}

impl MutationTransaction {
    /// Captures pre-mutation workspace state and only this workspace's active branch ref
    /// (`refs/heads/<branch>`, or `HEAD` if detached), keeping concurrent workspaces isolated.
    pub fn begin(
        repo: &OzoneRepo,
        command: impl Into<String>,
        actor: ActorInfo,
        pre_dirty_override: Option<&[FileChange]>,
    ) -> Result<Self> {
        Self::begin_with_options(
            repo,
            command,
            actor,
            &[],
            false,
            false,
            pre_dirty_override,
        )
    }

    /// Captures pre-mutation workspace state, this workspace's active ref, and any explicit `watched_refs`.
    pub fn begin_with_refs(
        repo: &OzoneRepo,
        command: impl Into<String>,
        actor: ActorInfo,
        watched_refs: &[&str],
        pre_dirty_override: Option<&[FileChange]>,
    ) -> Result<Self> {
        Self::begin_with_options(
            repo,
            command,
            actor,
            watched_refs,
            false,
            false,
            pre_dirty_override,
        )
    }

    /// Captures pre-mutation workspace state plus ALL local branch refs (`refs/heads/*`) for
    /// repo-wide operations such as multi-branch integration.
    pub fn begin_watching_all_branches(
        repo: &OzoneRepo,
        command: impl Into<String>,
        actor: ActorInfo,
        pre_dirty_override: Option<&[FileChange]>,
    ) -> Result<Self> {
        Self::begin_with_options(
            repo,
            command,
            actor,
            &[],
            true,
            false,
            pre_dirty_override,
        )
    }

    /// Captures pre-mutation workspace state plus ALL local (`refs/heads/*`) and remote-tracking
    /// (`refs/remotes/*`) branch refs for network commands such as `fetch`, `push`, and `pull`.
    pub fn begin_watching_all_refs(
        repo: &OzoneRepo,
        command: impl Into<String>,
        actor: ActorInfo,
        pre_dirty_override: Option<&[FileChange]>,
    ) -> Result<Self> {
        Self::begin_with_options(
            repo,
            command,
            actor,
            &[],
            true,
            true,
            pre_dirty_override,
        )
    }

    fn begin_with_options(
        repo: &OzoneRepo,
        command: impl Into<String>,
        actor: ActorInfo,
        watched_refs: &[&str],
        watch_all_branches: bool,
        watch_remote_refs: bool,
        pre_dirty_override: Option<&[FileChange]>,
    ) -> Result<Self> {
        ensure_mount_healthy(repo)?;
        ensure_oplog_dirs(&repo.layout)?;

        let gix_repo = open_gix_for_workspace(repo)?;
        let (pre_branch, pre_head_commit, pre_base_tree) =
            resolve_workspace_head(repo, &gix_repo)?;
        let pre_dirty_files =
            scan_dirty_files(repo, &gix_repo, pre_base_tree, pre_dirty_override)?;
        let pre_dirty_tree = build_dirty_tree(&gix_repo, pre_base_tree, &pre_dirty_files)?;

        let watched: Vec<String> = watched_refs.iter().map(|r| (*r).to_string()).collect();
        let pre_refs = snapshot_refs(
            &gix_repo,
            pre_branch.as_deref(),
            &watched,
            watch_all_branches,
            watch_remote_refs,
        )?;

        Ok(Self {
            command: command.into(),
            actor,
            ws_key: workspace_key(repo).to_string(),
            watch_all_branches,
            watch_remote_refs,
            watched_refs: watched,
            pre_refs,
            pre_branch,
            pre_head_commit,
            pre_base_tree,
            pre_dirty_tree,
            pre_dirty_files,
        })
    }

    /// Captures post-mutation refs and workspace snapshots and appends an [`OpKind::Mutate`]
    /// entry to the OpLog.
    pub fn finish(
        self,
        repo: &OzoneRepo,
        post_dirty_override: Option<&[FileChange]>,
    ) -> Result<OpLogEntry> {
        self.finish_with_message(repo, None, post_dirty_override)
    }

    /// Same as [`Self::finish`], attaching an optional human/agent summary `message`.
    pub fn finish_with_message(
        self,
        repo: &OzoneRepo,
        message: Option<&str>,
        post_dirty_override: Option<&[FileChange]>,
    ) -> Result<OpLogEntry> {
        let stored = self
            .finish_inner(repo, message, post_dirty_override, false)?
            .expect("finish_inner with only_if_changed=false always returns Some");
        Ok(stored)
    }

    /// Appends an [`OpKind::Mutate`] entry only if at least one watched ref, `HEAD`, or dirty
    /// working-tree file changed between `begin` and `finish`.
    pub fn finish_if_changed(
        self,
        repo: &OzoneRepo,
        post_dirty_override: Option<&[FileChange]>,
    ) -> Result<Option<OpLogEntry>> {
        self.finish_inner(repo, None, post_dirty_override, true)
    }

    fn finish_inner(
        self,
        repo: &OzoneRepo,
        message: Option<&str>,
        post_dirty_override: Option<&[FileChange]>,
        only_if_changed: bool,
    ) -> Result<Option<OpLogEntry>> {
        ensure_mount_healthy(repo)?;
        let gix_repo = open_gix_for_workspace(repo)?;

        let (post_branch, post_head_commit, post_base_tree) =
            resolve_workspace_head(repo, &gix_repo)?;
        let post_dirty_files =
            scan_dirty_files(repo, &gix_repo, post_base_tree, post_dirty_override)?;
        let post_dirty_tree = build_dirty_tree(&gix_repo, post_base_tree, &post_dirty_files)?;
        let branch_for_refs = post_branch.as_deref().or(self.pre_branch.as_deref());
        let post_refs = snapshot_refs(
            &gix_repo,
            branch_for_refs,
            &self.watched_refs,
            self.watch_all_branches,
            self.watch_remote_refs,
        )?;

        let mut all_ref_names = BTreeSet::new();
        all_ref_names.extend(self.pre_refs.keys().cloned());
        all_ref_names.extend(post_refs.keys().cloned());

        let both_detached = self.pre_branch.is_none() && post_branch.is_none();
        let mut ref_deltas = BTreeMap::new();
        for name in all_ref_names {
            if name == "HEAD" && !both_detached {
                // Symbolic <-> detached HEAD transitions are captured via WsSnapshot branch fields
                // so `oz undo` never attempts `RefEdit::delete("HEAD")`.
                continue;
            }
            let old = self.pre_refs.get(&name).cloned();
            let new = post_refs.get(&name).cloned();
            if old != new {
                ref_deltas.insert(name, RefDelta { old, new });
            }
        }

        if only_if_changed
            && ref_deltas.is_empty()
            && self.pre_branch == post_branch
            && self.pre_head_commit == post_head_commit
            && snapshots_match(
                &self.pre_base_tree.to_hex().to_string(),
                &self.pre_dirty_files,
                &post_base_tree.to_hex().to_string(),
                &post_dirty_files,
            )
        {
            return Ok(None);
        }

        let ws_snap = make_ws_snapshot(
            SnapshotSlice {
                base_tree: post_base_tree,
                dirty_tree: Some(post_dirty_tree),
                dirty_files: post_dirty_files.clone(),
                head_commit: post_head_commit,
                branch: post_branch.clone(),
            },
            Some(SnapshotSlice {
                base_tree: self.pre_base_tree,
                dirty_tree: Some(self.pre_dirty_tree),
                dirty_files: self.pre_dirty_files,
                head_commit: self.pre_head_commit,
                branch: self.pre_branch,
            }),
        );

        let mut ws_snapshots = BTreeMap::new();
        ws_snapshots.insert(self.ws_key, ws_snap);

        let entry = OpLogEntry {
            op_id: new_op_id(),
            parents: Vec::new(),
            timestamp: Utc::now(),
            actor: self.actor,
            kind: OpKind::Mutate,
            command: self.command,
            message: message.map(str::to_string),
            inverse_of: None,
            undone_ops: Vec::new(),
            rewound_uncheckpointed: false,
            ref_deltas,
            ws_snapshots,
        };

        let stored = append_entry(&repo.layout, entry)?;
        sync_workspace_state_file(
            repo,
            &gix_repo,
            post_head_commit,
            post_base_tree,
            &post_dirty_files,
            None,
            post_branch.as_deref(),
        )?;

        Ok(Some(stored))
    }
}

/// Wraps a mutating operation in a pre/post OpLog transaction (`OpKind::Mutate`).
///
/// If `mutate` returns `Err`, no OpLog entry is appended.
pub fn record_mutation<T, F>(
    repo: &OzoneRepo,
    command: impl Into<String>,
    actor: ActorInfo,
    dirty_override: Option<&[FileChange]>,
    mutate: F,
) -> Result<(T, OpLogEntry)>
where
    F: FnOnce() -> Result<T>,
{
    let tx = MutationTransaction::begin(repo, command, actor, dirty_override)?;
    let result = mutate()?;
    let entry = tx.finish(repo, None)?;
    Ok((result, entry))
}

/// Wraps a mutating operation while watching explicit refs in addition to this workspace's
/// active branch ref and optionally accepting a post-mutation dirty file list returned by `mutate`.
pub fn record_mutation_with_refs<T, F>(
    repo: &OzoneRepo,
    command: impl Into<String>,
    actor: ActorInfo,
    watched_refs: &[&str],
    pre_dirty_override: Option<&[FileChange]>,
    mutate: F,
) -> Result<(T, OpLogEntry)>
where
    F: FnOnce() -> Result<(T, Option<Vec<FileChange>>)>,
{
    let tx =
        MutationTransaction::begin_with_refs(repo, command, actor, watched_refs, pre_dirty_override)?;
    let (result, post_dirty) = mutate()?;
    let entry = tx.finish(repo, post_dirty.as_deref())?;
    Ok((result, entry))
}

/// Wraps a repo-wide mutating operation while watching ALL local branch refs (`refs/heads/*`).
pub fn record_mutation_all_branches<T, F>(
    repo: &OzoneRepo,
    command: impl Into<String>,
    actor: ActorInfo,
    dirty_override: Option<&[FileChange]>,
    mutate: F,
) -> Result<(T, OpLogEntry)>
where
    F: FnOnce() -> Result<T>,
{
    let tx = MutationTransaction::begin_watching_all_branches(repo, command, actor, dirty_override)?;
    let result = mutate()?;
    let entry = tx.finish(repo, None)?;
    Ok((result, entry))
}

fn snapshot_refs(
    gix_repo: &gix::Repository,
    active_branch: Option<&str>,
    watched_refs: &[String],
    watch_all_branches: bool,
    watch_remote_refs: bool,
) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();

    if watch_all_branches {
        let platform = gix_repo
            .references()
            .map_err(|err| OzError::Git(err.to_string()))?;
        let branches = platform
            .local_branches()
            .map_err(|err| OzError::Git(err.to_string()))?;

        for item in branches {
            let mut reference = item.map_err(|err| OzError::Git(err.to_string()))?;
            if matches!(reference.target(), gix::refs::TargetRef::Symbolic(_)) {
                continue;
            }
            let name = reference.name().as_bstr().to_str_lossy().into_owned();
            if let Ok(oid) = reference.peel_to_id() {
                map.insert(name, oid.detach().to_hex().to_string());
            }
        }
    } else if let Some(branch) = active_branch {
        let ref_name = format!("refs/heads/{branch}");
        if let Some(oid_hex) = read_ref_oid_hex(gix_repo, &ref_name)? {
            map.insert(ref_name, oid_hex);
        }
    }

    if watch_remote_refs {
        let platform = gix_repo
            .references()
            .map_err(|err| OzError::Git(err.to_string()))?;
        let remotes = platform
            .remote_branches()
            .map_err(|err| OzError::Git(err.to_string()))?;

        for item in remotes {
            let mut reference = item.map_err(|err| OzError::Git(err.to_string()))?;
            if matches!(reference.target(), gix::refs::TargetRef::Symbolic(_)) {
                continue;
            }
            let name = reference.name().as_bstr().to_str_lossy().into_owned();
            if let Ok(oid) = reference.peel_to_id() {
                map.insert(name, oid.detach().to_hex().to_string());
            }
        }
    }

    // Track detached HEAD explicitly if HEAD is not symbolic.
    if let Ok(head) = gix_repo.head()
        && head.is_detached()
        && let Some(id) = head.id()
    {
        map.insert("HEAD".to_string(), id.detach().to_hex().to_string());
    }

    for ref_name in watched_refs {
        if let Some(oid_hex) = read_ref_oid_hex(gix_repo, ref_name)? {
            map.insert(ref_name.clone(), oid_hex);
        }
    }

    Ok(map)
}
