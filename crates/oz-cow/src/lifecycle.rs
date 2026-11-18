// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Workspace lifecycle management: `spawn_workspace`, `list_workspaces`,
//! `remove_workspace`, and `gc_workspaces`.

use crate::backends::{backend_for_kind, remove_dir_if_exists};
#[cfg(target_os = "linux")]
use crate::backends::Fuse3VfsBackend;
#[cfg(target_os = "windows")]
use crate::backends::ProjFsVfsBackend;
use crate::detect::select_backend_fast;
use crate::{SpawnRequest, SpawnedWorkspace};
use chrono::Utc;
use globset::Glob;
use oz_core::{
    BackendKind, MountHealth, MountRecord, OzError, OzoneLayout, OzoneRepo, Result,
    WorkspaceState, probe_mount_health_os, validate_workspace_name,
};
use oz_warm::{
    BaselineSource, apply_baseline_mtimes, compute_changed_paths_between_commits,
    find_nearest_prior_baseline, gc_baselines, is_repo_quiescent, resolve_or_capture_baseline,
    detect_warm_targets,
};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

/// Options controlling [`spawn_workspace`].
#[derive(Debug, Clone)]
pub struct SpawnWorkspaceOptions {
    pub name: String,
    pub branch: Option<String>,
    pub from_ref: Option<String>,
    pub backend: Option<BackendKind>,
    pub scope: Vec<String>,
    pub deny: Vec<String>,
    pub warm: bool,
}

impl SpawnWorkspaceOptions {
    /// Creates spawn options for workspace `name` with `warm: true`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            branch: None,
            from_ref: None,
            backend: None,
            scope: Vec::new(),
            deny: Vec::new(),
            warm: true,
        }
    }
}

impl Default for SpawnWorkspaceOptions {
    fn default() -> Self {
        Self::new("")
    }
}

/// Complete outcome returned by [`spawn_workspace`].
#[derive(Debug, Clone)]
pub struct SpawnedWorkspaceInfo {
    pub workspace: SpawnedWorkspace,
    pub state: WorkspaceState,
    pub git_worktree_dir: PathBuf,
    pub baseline_source: Option<BaselineSource>,
}

/// Summary entry returned by [`list_workspaces`].
#[derive(Debug, Clone)]
pub struct WorkspaceSummary {
    pub state: WorkspaceState,
    pub mount_record: Option<MountRecord>,
    pub mount_health: MountHealth,
    pub healthy: bool,
    pub branch_head_oid: Option<String>,
}

/// Options controlling [`remove_workspace`].
#[derive(Debug, Clone, Default)]
pub struct RemoveWorkspaceOptions {
    pub force: bool,
    pub delete_branch: bool,
}

/// Summary returned by [`remove_workspace`].
#[derive(Debug, Clone)]
pub struct RemovedWorkspace {
    pub name: String,
    pub worktree_path: PathBuf,
    pub branch: Option<String>,
    pub branch_deleted: bool,
}

/// Options controlling [`gc_workspaces`].
#[derive(Debug, Clone)]
pub struct GcWorkspacesOptions {
    pub target_branch: Option<String>,
    pub prune_merged: bool,
    pub prune_missing: bool,
    pub dry_run: bool,
}

impl Default for GcWorkspacesOptions {
    fn default() -> Self {
        Self {
            target_branch: None,
            prune_merged: true,
            prune_missing: true,
            dry_run: false,
        }
    }
}

/// Summary returned by [`gc_workspaces`].
#[derive(Debug, Clone, Default)]
pub struct GcWorkspacesReport {
    pub removed: Vec<String>,
    pub skipped_dirty: Vec<String>,
}

/// Spawns an isolated CoW or VFS workspace at `../<repo>.oz/<ws>`, creating a real local
/// branch `refs/heads/<branch>`, colocated `.git/worktrees/<ws>/` metadata, warm baseline
/// caches + `mtime` preservation, and persistent `.git/ozone/state/<ws>.json`.
pub fn spawn_workspace(
    repo: &OzoneRepo,
    opts: &SpawnWorkspaceOptions,
) -> Result<SpawnedWorkspaceInfo> {
    validate_workspace_name(&opts.name)?;
    repo.layout.ensure_initialized()?;

    let scope = if opts.scope.is_empty() {
        repo.config.guardrails.scope.clone()
    } else {
        opts.scope.clone()
    };
    let mut deny = repo.config.guardrails.deny.clone();
    for pat in &opts.deny {
        if !deny.contains(pat) {
            deny.push(pat.clone());
        }
    }
    validate_globs(&scope, "scope")?;
    validate_globs(&deny, "deny")?;

    let state_path = repo.layout.workspace_state_path(&opts.name);
    let wt_git_dir = repo.git_common_dir.join("worktrees").join(&opts.name);
    if state_path.exists() || wt_git_dir.exists() {
        return Err(OzError::InvalidUsage {
            message: format!("workspace '{}' already exists", opts.name),
            recovery: vec![format!("oz ws rm {}", opts.name)],
        });
    }

    let target_path = repo
        .config
        .render_workspace_path(&repo.repo_root, &opts.name)?;
    if target_path.exists() {
        return Err(OzError::InvalidUsage {
            message: format!(
                "workspace path {} already exists on disk",
                target_path.display()
            ),
            recovery: vec![format!("oz ws rm {}", opts.name)],
        });
    }
    if let Some(container) = target_path.parent() {
        std::fs::create_dir_all(container)?;
    }

    let gix_repo = repo.open_gix()?;
    let branch = opts.branch.clone().unwrap_or_else(|| opts.name.clone());
    let branch_ref_name = format!("refs/heads/{branch}");
    validate_branch_ref_name(&branch_ref_name)?;

    let prepared_branch = prepare_branch_target(
        &gix_repo,
        &repo.git_common_dir,
        &branch_ref_name,
        opts.from_ref.as_deref(),
    )?;
    let base_commit = prepared_branch.base_commit;
    let base_tree = prepared_branch.base_tree;
    let base_branch = prepared_branch.base_branch;
    let created_branch = prepared_branch.created_branch;

    let spawn_result = (|| -> Result<SpawnedWorkspaceInfo> {
        let backend_kind = select_backend_fast(
            &repo.repo_root,
            Some(&repo.layout.ozone_dir),
            opts.backend.or(repo.config.backend),
        )?;

        let mut baseline_dir: Option<PathBuf> = None;
        let mut baseline_id: Option<String> = None;
        let mut baseline_meta = None;
        let mut baseline_source = None;

        if opts.warm
            && let Some(ref base_oid) = base_commit
        {
            let resolved = if repo.head_oid.as_deref() == Some(base_oid.as_str()) {
                resolve_or_capture_baseline(
                    &repo.repo_root,
                    &repo.layout.ozone_dir,
                    base_oid,
                    &repo.config,
                )?
            } else {
                let prior = find_nearest_prior_baseline(
                    &repo.repo_root,
                    &repo.layout.ozone_dir,
                    Some(base_oid),
                )?;
                if let Some(found) = prior {
                    Some(oz_warm::BaselineResolution {
                        baseline: found,
                        source: BaselineSource::Existing,
                    })
                } else if let Some(ref main_head) = repo.head_oid {
                    let targets = detect_warm_targets(&repo.repo_root, &repo.config);
                    if is_repo_quiescent(&repo.repo_root, &targets)? {
                        resolve_or_capture_baseline(
                            &repo.repo_root,
                            &repo.layout.ozone_dir,
                            main_head,
                            &repo.config,
                        )?
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            if let Some(res) = resolved {
                if res.source == BaselineSource::Captured {
                    let _ = gc_baselines(&repo.layout.ozone_dir, repo.config.max_baseline_gb);
                }
                baseline_id = Some(res.baseline.metadata.commit_oid.clone());
                baseline_dir = Some(res.baseline.root_dir.clone());
                baseline_meta = Some(res.baseline.metadata);
                baseline_source = Some(res.source);
            }
        }

        create_colocated_worktree_metadata(
            &gix_repo,
            &repo.layout.ozone_dir,
            &wt_git_dir,
            &target_path,
            &branch_ref_name,
            base_tree.as_deref(),
        )?;

        let spawn_req = SpawnRequest {
            workspace_name: opts.name.clone(),
            branch: branch.clone(),
            base_commit: base_commit.clone(),
            base_tree: base_tree.clone(),
            repo_root: repo.repo_root.clone(),
            git_dir: repo.git_common_dir.clone(),
            target_path: target_path.clone(),
            upper_dir: backend_kind
                .requires_daemon()
                .then(|| repo.layout.ws_upper_dir(&opts.name)),
            meta_dir: backend_kind
                .requires_daemon()
                .then(|| repo.layout.ws_tombstone_dir(&opts.name)),
            baseline_dir: baseline_dir.clone(),
            scope: scope.clone(),
            deny: deny.clone(),
            materialize_files: true,
        };

        let spawned = spawn_with_backend(repo, backend_kind, &spawn_req)?;

        if !backend_kind.requires_daemon() {
            std::fs::write(
                target_path.join(".git"),
                format!("gitdir: {}\n", wt_git_dir.display()),
            )?;

            if opts.warm
                && repo.config.warm.preserve_mtime
                && let Some(ref bmeta) = baseline_meta
            {
                let changed = match base_commit.as_deref() {
                    Some(target_oid) => compute_changed_paths_between_commits(
                        &repo.repo_root,
                        &bmeta.commit_oid,
                        target_oid,
                    )
                    .unwrap_or_default(),
                    None => HashSet::new(),
                };
                let _ = apply_baseline_mtimes(&target_path, bmeta, &changed)?;
            }
        }

        let now = Utc::now();
        let state = WorkspaceState {
            schema_version: WorkspaceState::CURRENT_SCHEMA_VERSION,
            name: opts.name.clone(),
            branch: branch.clone(),
            base_branch,
            base_commit,
            base_tree: base_tree.unwrap_or_default(),
            backend: backend_kind,
            worktree_path: target_path.clone(),
            git_worktree_dir: Some(wt_git_dir.clone()),
            baseline_id,
            dirty_paths: BTreeMap::new(),
            pinned_paths: BTreeSet::new(),
            scope,
            deny,
            created_at: now,
            updated_at: now,
            last_checkpoint_oid: None,
        };
        write_json_atomic(&state_path, &state)?;

        Ok(SpawnedWorkspaceInfo {
            workspace: spawned,
            state,
            git_worktree_dir: wt_git_dir.clone(),
            baseline_source,
        })
    })();

    if spawn_result.is_err() {
        let _ = remove_dir_if_exists(&wt_git_dir);
        let _ = remove_dir_if_exists(&repo.layout.ws_meta_dir(&opts.name));
        let _ = remove_dir_if_exists(&target_path);
        let _ = std::fs::remove_file(&state_path);
        if created_branch
            && let Ok(mut reference) = gix_repo.try_find_reference(&branch_ref_name)
            && let Some(r) = reference.take()
        {
            let _ = r.delete();
        }
    }

    spawn_result
}

/// Lists all registered Ozone workspaces in `.git/ozone/state/` with live mount health.
pub fn list_workspaces(repo: &OzoneRepo) -> Result<Vec<WorkspaceSummary>> {
    let state_dir = repo.layout.state_dir();
    if !state_dir.is_dir() {
        return Ok(Vec::new());
    }

    let gix_repo = repo.open_gix().ok();
    let mut items = Vec::new();
    for entry in std::fs::read_dir(&state_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path)?;
        let state: WorkspaceState = serde_json::from_str(&raw)?;
        let mount_record = load_mount_record(&repo.layout, &state.name)?;
        let mount_health = if state.backend.requires_daemon() || mount_record.is_some() {
            probe_mount_health_os(&state.worktree_path)
        } else if state.worktree_path.is_dir() {
            MountHealth::Healthy
        } else {
            MountHealth::NotMounted
        };
        let healthy = match mount_health {
            MountHealth::Healthy | MountHealth::NotApplicable => true,
            MountHealth::NotMounted | MountHealth::DisconnectedEnotconn => false,
        };
        let branch_head_oid = gix_repo.as_ref().and_then(|r| {
            let mut rf = r
                .try_find_reference(&format!("refs/heads/{}", state.branch))
                .ok()??;
            rf.peel_to_id().ok().map(|id| id.to_hex().to_string())
        });

        items.push(WorkspaceSummary {
            state,
            mount_record,
            mount_health,
            healthy,
            branch_head_oid,
        });
    }

    items.sort_by(|a, b| a.state.name.cmp(&b.state.name));
    Ok(items)
}

/// Removes a workspace cleanly: unmounts VFS if applicable, removes `<ws>`,
/// `.git/ozone/ws/<ws>`, `.git/ozone/state/<ws>.json`, and `.git/worktrees/<ws>`.
pub fn remove_workspace(
    repo: &OzoneRepo,
    name: &str,
    opts: &RemoveWorkspaceOptions,
) -> Result<RemovedWorkspace> {
    validate_workspace_name(name)?;

    let state_path = repo.layout.workspace_state_path(name);
    let mount_path = repo.layout.ws_mount_record_path(name);
    let wt_git_dir = repo.git_common_dir.join("worktrees").join(name);

    let state: Option<WorkspaceState> = if state_path.is_file() {
        Some(serde_json::from_str(&std::fs::read_to_string(&state_path)?)?)
    } else {
        None
    };
    let mount_rec: Option<MountRecord> = if mount_path.is_file() {
        Some(serde_json::from_str(&std::fs::read_to_string(&mount_path)?)?)
    } else {
        None
    };

    if state.is_none() && mount_rec.is_none() && !wt_git_dir.exists() {
        return Err(OzError::WorkspaceNotFound {
            name: name.to_string(),
        });
    }

    if !opts.force
        && let Some(ref st) = state
        && !st.dirty_paths.is_empty()
    {
        return Err(OzError::InvalidUsage {
            message: format!(
                "workspace '{name}' has {} recorded dirty paths; pass --force to remove anyway",
                st.dirty_paths.len()
            ),
            recovery: vec![format!("oz ws rm {name} --force")],
        });
    }

    let worktree_path = state
        .as_ref()
        .map(|s| s.worktree_path.clone())
        .or_else(|| mount_rec.as_ref().map(|m| m.mount_point.clone()))
        .unwrap_or_else(|| {
            repo.config
                .render_workspace_path(&repo.repo_root, name)
                .unwrap_or_else(|_| repo.repo_root.join(name))
        });

    let backend_kind = state
        .as_ref()
        .map(|s| s.backend)
        .or_else(|| mount_rec.as_ref().map(|m| m.backend))
        .unwrap_or(BackendKind::CasHardlink);

    let branch = state
        .as_ref()
        .map(|s| s.branch.clone())
        .or_else(|| mount_rec.as_ref().map(|m| m.branch.clone()));

    if backend_kind.requires_daemon() || mount_rec.is_some() {
        #[cfg(target_os = "linux")]
        if backend_kind == BackendKind::LinuxFuse3 {
            let fuse = Fuse3VfsBackend::new(
                repo.config.daemon.allow_other,
                true,
                repo.config.daemon.auto_start,
            );
            fuse.destroy_with_context(&repo.git_common_dir, name, &worktree_path)?;
        }
        #[cfg(target_os = "windows")]
        if backend_kind == BackendKind::WindowsProjFs {
            let projfs = ProjFsVfsBackend::new(repo.config.daemon.auto_start);
            projfs.destroy_with_context(&repo.git_common_dir, name, &worktree_path)?;
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = remove_dir_if_exists(&worktree_path);
        }
    } else if let Ok(backend) = backend_for_kind(backend_kind) {
        backend.destroy(&worktree_path)?;
    }

    let _ = remove_dir_if_exists(&worktree_path);
    remove_dir_if_exists(&repo.layout.ws_meta_dir(name))?;
    if state_path.exists() {
        std::fs::remove_file(&state_path)?;
    }
    remove_dir_if_exists(&wt_git_dir)?;

    let mut branch_deleted = false;
    if opts.delete_branch
        && let Some(ref br) = branch
        && let Ok(gix_repo) = repo.open_gix()
        && let Ok(checked) = collect_checked_out_branches(&gix_repo, &repo.git_common_dir)
    {
        let full_ref = format!("refs/heads/{br}");
        if !checked.contains(&full_ref)
            && let Ok(Some(r)) = gix_repo.try_find_reference(&full_ref)
        {
            branch_deleted = r.delete().is_ok();
        }
    }

    Ok(RemovedWorkspace {
        name: name.to_string(),
        worktree_path,
        branch,
        branch_deleted,
    })
}

/// Prunes missing/orphaned workspaces and workspaces whose feature branches have merged into
/// `target_branch` (defaulting to `main` or the repository's current branch).
pub fn gc_workspaces(
    repo: &OzoneRepo,
    opts: &GcWorkspacesOptions,
) -> Result<GcWorkspacesReport> {
    let summaries = list_workspaces(repo)?;
    let mut report = GcWorkspacesReport::default();

    let gix_repo = repo.open_gix().ok();
    let target_oid = gix_repo.as_ref().and_then(|r| {
        let candidate = opts
            .target_branch
            .as_deref()
            .or(repo.branch.as_deref())
            .unwrap_or("main");
        let full = if candidate.starts_with("refs/") {
            candidate.to_string()
        } else {
            format!("refs/heads/{candidate}")
        };
        r.try_find_reference(&full)
            .ok()
            .flatten()
            .and_then(|mut rf| rf.peel_to_id().ok().map(|id| id.detach()))
    });

    for ws in summaries {
        let name = ws.state.name.clone();
        let is_missing = !ws.state.backend.requires_daemon() && !ws.state.worktree_path.is_dir();

        let is_merged = if opts.prune_merged
            && let (Some(gix_r), Some(tgt_id)) = (&gix_repo, target_oid)
            && let Some(ref br_hex) = ws.branch_head_oid
            && let Ok(br_id) = gix::ObjectId::from_hex(br_hex.as_bytes())
        {
            let has_new_commits = ws
                .state
                .base_commit
                .as_deref()
                .is_some_and(|base| base != br_hex.as_str());
            has_new_commits
                && gix_r
                    .merge_base(br_id, tgt_id)
                    .is_ok_and(|opt| opt.is_some_and(|mb| mb.detach() == br_id))
        } else {
            false
        };

        let should_prune = (opts.prune_missing && is_missing) || is_merged;
        if !should_prune {
            continue;
        }

        if !ws.state.dirty_paths.is_empty() {
            report.skipped_dirty.push(name);
            continue;
        }

        if !opts.dry_run {
            remove_workspace(
                repo,
                &name,
                &RemoveWorkspaceOptions {
                    force: false,
                    delete_branch: is_merged,
                },
            )?;
        }
        report.removed.push(name);
    }

    Ok(report)
}

fn spawn_with_backend(
    repo: &OzoneRepo,
    kind: BackendKind,
    req: &SpawnRequest,
) -> Result<SpawnedWorkspace> {
    #[cfg(target_os = "linux")]
    if kind == BackendKind::LinuxFuse3 {
        let fuse = Fuse3VfsBackend::new(
            repo.config.daemon.allow_other,
            true,
            repo.config.daemon.auto_start,
        );
        return crate::WorkspaceBackend::spawn(&fuse, req);
    }
    #[cfg(target_os = "windows")]
    if kind == BackendKind::WindowsProjFs {
        let projfs = ProjFsVfsBackend::new(repo.config.daemon.auto_start);
        return crate::WorkspaceBackend::spawn(&projfs, req);
    }
    let _ = repo;
    backend_for_kind(kind)?.spawn(req)
}

fn validate_globs(patterns: &[String], label: &str) -> Result<()> {
    for pat in patterns {
        Glob::new(pat).map_err(|err| OzError::InvalidUsage {
            message: format!("invalid {label} glob pattern '{pat}': {err}"),
            recovery: Vec::new(),
        })?;
    }
    Ok(())
}

fn validate_branch_ref_name(full_ref_name: &str) -> Result<()> {
    let _ = gix::refs::FullName::try_from(full_ref_name).map_err(|err| OzError::InvalidUsage {
        message: format!("invalid branch name '{full_ref_name}': {err}"),
        recovery: Vec::new(),
    })?;
    Ok(())
}

struct PreparedBranchTarget {
    base_commit: Option<String>,
    base_tree: Option<String>,
    base_branch: Option<String>,
    created_branch: bool,
}

fn prepare_branch_target(
    gix_repo: &gix::Repository,
    git_common_dir: &Path,
    branch_ref_name: &str,
    from_ref: Option<&str>,
) -> Result<PreparedBranchTarget> {
    let checked_out = collect_checked_out_branches(gix_repo, git_common_dir)?;
    if checked_out.contains(branch_ref_name) {
        return Err(OzError::Conflict {
            message: format!("branch '{branch_ref_name}' is already checked out in another worktree"),
            conflicts: Vec::new(),
            recovery: vec!["Pass --branch <new-branch> to spawn on a dedicated branch.".to_string()],
        });
    }

    if let Some(mut existing_ref) = gix_repo
        .try_find_reference(branch_ref_name)
        .map_err(|e| OzError::Git(e.to_string()))?
    {
        if from_ref.is_some() {
            return Err(OzError::InvalidUsage {
                message: format!(
                    "branch '{branch_ref_name}' already exists; omit --from or choose another --branch"
                ),
                recovery: Vec::new(),
            });
        }
        let commit = existing_ref
            .peel_to_commit()
            .map_err(|e| OzError::Git(e.to_string()))?;
        let commit_hex = commit.id.to_hex().to_string();
        let tree_hex = commit
            .tree_id()
            .map_err(|e| OzError::Git(e.to_string()))?
            .to_hex()
            .to_string();
        return Ok(PreparedBranchTarget {
            base_commit: Some(commit_hex),
            base_tree: Some(tree_hex),
            base_branch: None,
            created_branch: false,
        });
    }

    let base_branch = match from_ref {
        Some(spec) => Some(spec.to_string()),
        None => gix_repo
            .head_name()
            .ok()
            .flatten()
            .map(|n| String::from_utf8_lossy(n.shorten().as_ref()).into_owned()),
    };

    if from_ref.is_none()
        && gix_repo
            .head()
            .map_err(|e| OzError::Git(e.to_string()))?
            .is_unborn()
    {
        return Ok(PreparedBranchTarget {
            base_commit: None,
            base_tree: None,
            base_branch,
            created_branch: false,
        });
    }

    let rev_spec = from_ref.unwrap_or("HEAD");
    let obj_id = gix_repo
        .rev_parse_single(rev_spec)
        .map_err(|e| OzError::Git(format!("could not resolve '{rev_spec}': {e}")))?;
    let commit = obj_id
        .object()
        .map_err(|e| OzError::Git(e.to_string()))?
        .peel_to_commit()
        .map_err(|e| OzError::Git(e.to_string()))?;
    let commit_id = commit.id;
    let tree_hex = commit
        .tree_id()
        .map_err(|e| OzError::Git(e.to_string()))?
        .to_hex()
        .to_string();

    gix_repo
        .reference(
            branch_ref_name,
            commit_id,
            gix::refs::transaction::PreviousValue::MustNotExist,
            "oz ws spawn",
        )
        .map_err(|e| OzError::Git(e.to_string()))?;

    Ok(PreparedBranchTarget {
        base_commit: Some(commit_id.to_hex().to_string()),
        base_tree: Some(tree_hex),
        base_branch,
        created_branch: true,
    })
}

fn collect_checked_out_branches(
    gix_repo: &gix::Repository,
    git_common_dir: &Path,
) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    if let Ok(Some(name)) = gix_repo.head_name() {
        out.insert(String::from_utf8_lossy(name.as_bstr()).into_owned());
    }

    let worktrees_dir = git_common_dir.join("worktrees");
    if worktrees_dir.is_dir() {
        for entry in std::fs::read_dir(&worktrees_dir)? {
            let entry = entry?;
            let head_path = entry.path().join("HEAD");
            if let Ok(content) = std::fs::read_to_string(&head_path)
                && let Some(target) = content.trim().strip_prefix("ref:")
            {
                out.insert(target.trim().to_string());
            }
        }
    }
    Ok(out)
}

fn create_colocated_worktree_metadata(
    gix_repo: &gix::Repository,
    ozone_dir: &Path,
    wt_git_dir: &Path,
    target_path: &Path,
    branch_ref_name: &str,
    base_tree: Option<&str>,
) -> Result<()> {
    std::fs::create_dir_all(wt_git_dir)?;
    std::fs::write(wt_git_dir.join("HEAD"), format!("ref: {branch_ref_name}\n"))?;
    std::fs::write(wt_git_dir.join("commondir"), "../..\n")?;
    std::fs::write(
        wt_git_dir.join("gitdir"),
        format!("{}\n", target_path.join(".git").display()),
    )?;

    let index_path = wt_git_dir.join("index");
    if let Some(tree_hex) = base_tree {
        // Cache the clean tree-only Git index per tree OID so spawning N workspaces from the
        // same base commit builds the index once and copies it via fast kernel copy.
        let cache_dir = ozone_dir.join("index_cache");
        let cached_index = cache_dir.join(tree_hex);
        if cached_index.is_file() && std::fs::copy(&cached_index, &index_path).is_ok() {
            return Ok(());
        }

        let tree_id = gix::ObjectId::from_hex(tree_hex.as_bytes())
            .map_err(|e| OzError::Git(e.to_string()))?;
        let index_file = gix_repo
            .index_from_tree(&tree_id)
            .map_err(|e| OzError::Git(e.to_string()))?;

        let mut out = BufWriter::new(std::fs::File::create(&index_path)?);
        index_file
            .write_to(&mut out, gix::index::write::Options::default())
            .map_err(|e| OzError::Git(e.to_string()))?;
        drop(out);

        // Best-effort atomic populate of the single-entry tree index cache (pruning stale entries).
        if std::fs::create_dir_all(&cache_dir).is_ok() {
            static IDX_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = IDX_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp_cache = cache_dir.join(format!(".tmp_{tree_hex}_{}_{}", std::process::id(), seq));
            if std::fs::copy(&index_path, &tmp_cache).is_ok() {
                if std::fs::rename(&tmp_cache, &cached_index).is_err() {
                    let _ = std::fs::remove_file(&tmp_cache);
                } else if let Ok(rd) = std::fs::read_dir(&cache_dir) {
                    for ent in rd.flatten() {
                        let fname = ent.file_name();
                        if fname != std::ffi::OsStr::new(tree_hex)
                            && !fname.to_string_lossy().starts_with(".tmp_")
                        {
                            let _ = std::fs::remove_file(ent.path());
                        }
                    }
                }
            }
        }
        return Ok(());
    }

    let index_file = gix::index::File::from_state(
        gix::index::State::new(gix_repo.object_hash()),
        index_path.clone(),
    );
    let mut out = BufWriter::new(std::fs::File::create(&index_path)?);
    index_file
        .write_to(&mut out, gix::index::write::Options::default())
        .map_err(|e| OzError::Git(e.to_string()))?;

    Ok(())
}

fn load_mount_record(layout: &OzoneLayout, ws: &str) -> Result<Option<MountRecord>> {
    let p = layout.ws_mount_record_path(ws);
    if !p.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(p)?;
    Ok(Some(serde_json::from_str(&raw)?))
}

fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| OzError::Other(format!("path {} has no parent", path.display())))?;
    std::fs::create_dir_all(parent)?;
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".tmp_{}_{}_{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("json"),
        std::process::id(),
        seq
    ));
    let json = serde_json::to_string_pretty(value)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
