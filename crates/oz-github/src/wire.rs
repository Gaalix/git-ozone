// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-Rust Git Wire Protocol v2 (`clone_repo`, `fetch_remote`) and smart `git-receive-pack`
//! (`push_refs` with `--atomic`, `--force-with-lease`, and `--set-upstream`) over `file://`/local,
//! SSH, and HTTP/HTTPS (`reqwest` + `rustls`).

use crate::auth::{
    RemoteScheme, TokenDiscoveryOptions, discover_github_token_with_options, parse_git_remote,
    read_remote_url_from_gix,
};
use chrono::Utc;
use gix::bstr::{BString, ByteSlice};
use gix::hash::ObjectId;
use gix::objs::Kind as ObjectKind;
use gix::prelude::Find;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::{FullName, Target};
use gix_pack::data::output as pack_out;
use gix_packetline::PacketLineRef;
use gix_transport::client::blocking_io::{Transport, TransportV2Ext, connect};
use gix_transport::client::{Capabilities, MessageKind, TransportWithoutIO, WriteMode};
use gix_transport::{Protocol, Service};
use oz_core::{ConflictEntry, OzError, OzoneLayout, OzoneRepo, Result, normalize_lexical};
use oz_cow::materialize::materialize_tree_from_odb;
use oz_oplog::{
    ActorInfo, DEFAULT_WORKSPACE_KEY, MutationTransaction, OpKind, OpLogEntry, RefDelta,
    WsSnapshot, append_entry, ensure_oplog_dirs, new_op_id,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::io::{BufWriter, Read, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

const USER_AGENT_VALUE: &str = concat!("ozone/", env!("CARGO_PKG_VERSION"));

/// HTTP authentication header mode when connecting over `http://` or `https://`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpAuthScheme {
    /// Standard GitHub `Authorization: Basic base64("x-access-token:<token>")`.
    #[default]
    BasicToken,
    /// `Authorization: Bearer <token>`.
    Bearer,
}

/// Options controlling [`clone_repo`].
#[derive(Debug, Clone)]
pub struct CloneOptions {
    /// Remote repository URL or local filesystem path.
    pub url: String,
    /// Target directory on disk where the repository will be cloned.
    pub destination: PathBuf,
    /// Optional branch or tag name to check out (`--branch <name>`).
    pub branch: Option<String>,
    /// Optional partial-clone object filter, e.g. `Some("blob:none".to_string())` (`--filter=blob:none`).
    pub filter: Option<String>,
    /// Clone as a bare repository (`--bare`).
    pub bare: bool,
    /// Remote name to configure (defaults to `"origin"`).
    pub remote_name: Option<String>,
    /// Materialize working tree + `.git/index` after fetching (defaults to `true` when `!bare`).
    pub checkout: bool,
    /// Explicit authentication token override for HTTP/HTTPS remotes.
    pub auth_token: Option<String>,
    /// HTTP authentication header scheme (`BasicToken` or `Bearer`).
    pub auth_scheme: HttpAuthScheme,
}

impl CloneOptions {
    /// Creates default clone options for `url` into `destination`.
    #[must_use]
    pub fn new(url: impl Into<String>, destination: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            destination: destination.into(),
            branch: None,
            filter: None,
            bare: false,
            remote_name: None,
            checkout: true,
            auth_token: None,
            auth_scheme: HttpAuthScheme::BasicToken,
        }
    }
}

/// Summary returned by [`clone_repo`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneOutcome {
    pub destination: PathBuf,
    pub git_dir: PathBuf,
    pub remote_name: String,
    pub remote_url: String,
    pub protocol_version: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    pub partial_clone: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    pub filter_negotiated: bool,
    pub checked_out_files: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Options controlling [`fetch_remote`].
#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    /// Remote name or URL (defaults to `"origin"`).
    pub remote: Option<String>,
    /// Optional explicit refspecs (e.g. `main`, `refs/heads/feat:refs/remotes/origin/feat`).
    pub refspecs: Vec<String>,
    /// Prune remote-tracking refs under `refs/remotes/<remote>/*` that no longer exist on the remote (`--prune`).
    pub prune: bool,
    /// Optional partial-clone object filter (`--filter=blob:none`).
    pub filter: Option<String>,
    /// Fetch remote tags (`refs/tags/*`).
    pub tags: bool,
    /// Explicit authentication token override for HTTP/HTTPS remotes.
    pub auth_token: Option<String>,
    /// HTTP authentication header scheme (`BasicToken` or `Bearer`).
    pub auth_scheme: HttpAuthScheme,
}

/// Single reference delta applied during [`fetch_remote`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedRefUpdate {
    pub remote_ref: String,
    pub local_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_oid: Option<String>,
    pub new_oid: String,
    pub forced: bool,
}

/// Summary returned by [`fetch_remote`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchOutcome {
    pub remote: String,
    pub remote_url: String,
    pub protocol_version: u8,
    pub pack_received: bool,
    pub filter_negotiated: bool,
    pub updated_refs: Vec<FetchedRefUpdate>,
    pub pruned_refs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Explicit `--force-with-lease[=<ref>[:<expected_oid>]]` specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForceLeaseSpec {
    /// Optional target ref (`main` or `refs/heads/main`); when `None`, applies to all pushed refs.
    pub ref_name: Option<String>,
    /// Explicit expected remote OID (`""` or `"0000..."` expects ref to not exist on remote);
    /// when `None`, compares against local `refs/remotes/<remote>/<branch>`.
    pub expected_oid: Option<String>,
}

impl ForceLeaseSpec {
    /// Parses `--force-with-lease` argument values:
    /// - `""` -> default lease against remote-tracking refs
    /// - `"<ref>"` -> lease for `<ref>` against its remote-tracking ref
    /// - `"<ref>:<expected_oid>"` -> explicit expected OID for `<ref>`
    #[must_use]
    pub fn parse(spec: &str) -> Self {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return Self {
                ref_name: None,
                expected_oid: None,
            };
        }
        if let Some((r, expected)) = trimmed.split_once(':') {
            Self {
                ref_name: (!r.trim().is_empty()).then(|| r.trim().to_string()),
                expected_oid: Some(expected.trim().to_string()),
            }
        } else {
            Self {
                ref_name: Some(trimmed.to_string()),
                expected_oid: None,
            }
        }
    }
}

/// Options controlling [`push_refs`].
#[derive(Debug, Clone, Default)]
pub struct PushOptions {
    /// Remote name or URL (defaults to `"origin"`).
    pub remote: Option<String>,
    /// Refspecs to push (if empty, pushes current checked-out branch `HEAD -> refs/heads/<branch>`).
    pub refspecs: Vec<String>,
    /// Negotiate `atomic` capability and abort all updates if any ref fails (`--atomic`).
    pub atomic: bool,
    /// Force non-fast-forward updates unconditionally (`--force`).
    pub force: bool,
    /// Compare remote advertised tips against local remote-tracking refs (`--force-with-lease`).
    pub force_with_lease: bool,
    /// Explicit `--force-with-lease=<ref>[:<oid>]` expectations.
    pub lease_specs: Vec<ForceLeaseSpec>,
    /// Delete listed branch/ref names on the remote (`--delete`).
    pub delete: bool,
    /// Configure `branch.<name>.remote` and `branch.<name>.merge` (`-u` / `--set-upstream`).
    pub set_upstream: bool,
    /// Explicit authentication token override for HTTP/HTTPS remotes.
    pub auth_token: Option<String>,
    /// HTTP authentication header scheme (`BasicToken` or `Bearer`).
    pub auth_scheme: HttpAuthScheme,
}

/// Status of a single pushed reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushedRefUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_ref: Option<String>,
    pub remote_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_oid: Option<String>,
    pub deleted: bool,
    pub forced: bool,
}

/// Summary returned by [`push_refs`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushOutcome {
    pub remote: String,
    pub remote_url: String,
    pub atomic: bool,
    pub pack_objects_sent: usize,
    pub pack_bytes_sent: usize,
    pub pushed_refs: Vec<PushedRefUpdate>,
    pub upstream_configured: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op_id: Option<String>,
}

#[derive(Debug, Clone)]
enum AdvertisedRef {
    Unborn {
        name: String,
        target: String,
    },
    Symbolic {
        name: String,
        target: String,
        oid: ObjectId,
    },
    Peeled {
        name: String,
        tag_oid: ObjectId,
        peeled_oid: ObjectId,
    },
    Direct {
        name: String,
        oid: ObjectId,
    },
}

impl AdvertisedRef {
    fn name(&self) -> &str {
        match self {
            Self::Unborn { name, .. }
            | Self::Symbolic { name, .. }
            | Self::Peeled { name, .. }
            | Self::Direct { name, .. } => name,
        }
    }

    fn oid(&self) -> Option<ObjectId> {
        match self {
            Self::Unborn { .. } => None,
            Self::Symbolic { oid, .. } | Self::Direct { oid, .. } => Some(*oid),
            Self::Peeled { tag_oid, .. } => Some(*tag_oid),
        }
    }

    fn symref_target(&self) -> Option<&str> {
        match self {
            Self::Unborn { target, .. } | Self::Symbolic { target, .. } => Some(target),
            Self::Peeled { .. } | Self::Direct { .. } => None,
        }
    }
}

/// Executes blocking `gix-transport` / `reqwest::blocking` operations safely even when invoked
/// from inside an active `tokio` runtime (preventing nested-runtime drop panics).
pub(crate) fn run_blocking_safe<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::Builder::new()
            .name("oz-github-blocking".to_string())
            .spawn(f)
            .map_err(OzError::Io)?
            .join()
            .map_err(|_| OzError::Other("blocking worker thread panicked".to_string()))?
    } else {
        f()
    }
}

/// Clones a remote Git repository over `file://`/local path, SSH, or HTTP/HTTPS using pure-Rust
/// `gix` Wire Protocol v2, supporting `--filter=blob:none` partial clones, `--branch`, checkout
/// materialization, colocated `.git/ozone/` initialization, and initial OpLog recording.
pub fn clone_repo(opts: &CloneOptions, actor: ActorInfo) -> Result<CloneOutcome> {
    let opts_owned = opts.clone();
    run_blocking_safe(move || clone_repo_blocking(&opts_owned, actor))
}

fn clone_repo_blocking(opts: &CloneOptions, actor: ActorInfo) -> Result<CloneOutcome> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let dst = if opts.destination.is_absolute() {
        normalize_lexical(&opts.destination)
    } else {
        normalize_lexical(&cwd.join(&opts.destination))
    };

    if dst.exists() {
        let is_non_empty = std::fs::read_dir(&dst)?.next().is_some();
        if is_non_empty {
            return Err(OzError::InvalidUsage {
                message: format!(
                    "destination path '{}' already exists and is not an empty directory",
                    dst.display()
                ),
                recovery: vec!["Choose an empty target directory for `oz clone`.".to_string()],
            });
        }
    } else if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let remote_name = opts
        .remote_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("origin")
        .to_string();

    if let Some(ref filter_spec) = opts.filter {
        validate_filter_spec(filter_spec)?;
    }

    let created_dst = !dst.exists();
    let result = (|| -> Result<CloneOutcome> {
        let create_kind = if opts.bare {
            gix::create::Kind::Bare
        } else {
            gix::create::Kind::WithWorktree
        };
        let init_repo = gix::ThreadSafeRepository::init_opts(
            &dst,
            create_kind,
            gix::create::Options {
                destination_must_be_empty: Some(true),
                ..Default::default()
            },
            gix::open::Options::isolated(),
        )
        .map_err(|err| OzError::Git(format!("failed to initialize destination repository: {err}")))?
        .to_thread_local();

        let git_dir = init_repo.git_dir().to_path_buf();
        let (gix_url, persisted_url, token_opt, auth_scheme) = resolve_transport_url_and_auth(
            &opts.url,
            &cwd,
            opts.auth_token.as_deref(),
            opts.auth_scheme,
        )?;

        let mut warnings = Vec::new();
        let (protocol, adv_refs, pack_written, filter_negotiated) = fetch_pack_session(
            &init_repo,
            gix_url.clone(),
            token_opt.as_deref(),
            auth_scheme,
            opts.filter.as_deref(),
            true,
            None,
            &[],
            &mut warnings,
        )?;
        let _ = pack_written;

        // Determine selected branch & commit OID.
        let (selected_branch, target_oid, default_remote_branch) =
            select_clone_target(&adv_refs, opts.branch.as_deref())?;

        // Write `.git/config` remote, partial-clone promisor settings, and branch upstream tracking.
        write_clone_git_config(
            &git_dir,
            &remote_name,
            &persisted_url,
            opts.bare,
            opts.filter.as_deref(),
            selected_branch.as_deref(),
        )?;

        // Re-open repo so ODB sees newly written packfiles and config.
        let gix_repo = open_gix_unrestricted(&dst)?;

        // Build atomic ref edits for remote tracking branches, tags, and local branch.
        let mut ref_edits = Vec::new();
        let mut ref_deltas = BTreeMap::new();
        for adv in &adv_refs {
            let Some(oid) = adv.oid() else {
                continue;
            };
            let name = adv.name();
            if let Some(short_branch) = name.strip_prefix("refs/heads/") {
                let tracking_ref = if opts.bare {
                    format!("refs/heads/{short_branch}")
                } else {
                    format!("refs/remotes/{remote_name}/{short_branch}")
                };
                if let Ok(full) = FullName::try_from(tracking_ref.as_str()) {
                    ref_edits.push(RefEdit::update(
                        full,
                        oid,
                        PreviousValue::Any,
                        format!("oz clone: from {persisted_url}"),
                    ));
                    ref_deltas.insert(
                        tracking_ref,
                        RefDelta {
                            old: None,
                            new: Some(oid.to_hex().to_string()),
                        },
                    );
                }
            } else if name.starts_with("refs/tags/")
                && let Ok(full) = FullName::try_from(name)
            {
                ref_edits.push(RefEdit::update(
                    full,
                    oid,
                    PreviousValue::Any,
                    format!("oz clone: from {persisted_url}"),
                ));
                ref_deltas.insert(
                    name.to_string(),
                    RefDelta {
                        old: None,
                        new: Some(oid.to_hex().to_string()),
                    },
                );
            }
        }

        if !opts.bare
            && let Some(ref branch_name) = selected_branch
            && let Some(oid) = target_oid
        {
            let local_ref = format!("refs/heads/{branch_name}");
            let full = FullName::try_from(local_ref.as_str())
                .map_err(|err| OzError::Git(format!("invalid branch ref '{local_ref}': {err}")))?;
            ref_edits.push(RefEdit::update(
                full,
                oid,
                PreviousValue::Any,
                format!("oz clone: from {persisted_url}"),
            ));
            ref_deltas.insert(
                local_ref,
                RefDelta {
                    old: None,
                    new: Some(oid.to_hex().to_string()),
                },
            );
        }

        if !ref_edits.is_empty() {
            gix_repo
                .edit_references(ref_edits)
                .map_err(|err| OzError::Git(format!("failed to write cloned refs: {err}")))?;
        }

        // Set symbolic `refs/remotes/<remote>/HEAD` and `HEAD`.
        if !opts.bare
            && let Some(ref def_br) = default_remote_branch
        {
            let remote_head_ref = format!("refs/remotes/{remote_name}/HEAD");
            let remote_target_ref = format!("refs/remotes/{remote_name}/{def_br}");
            let _ = set_symbolic_ref(&gix_repo, &remote_head_ref, &remote_target_ref, "oz clone");
        }

        if let Some(ref branch_name) = selected_branch {
            let local_ref = format!("refs/heads/{branch_name}");
            set_symbolic_ref(&gix_repo, "HEAD", &local_ref, "oz clone")?;
        } else if let Some(oid) = target_oid {
            set_detached_head(&gix_repo, oid, "oz clone")?;
        }

        // Materialize working tree & `.git/index` (fetching missing blobs on-demand if partial clone!).
        let mut checked_out_files = 0usize;
        let mut base_tree_hex = gix_repo.empty_tree().id().to_hex().to_string();
        if let Some(oid) = target_oid {
            let gix_repo = open_gix_unrestricted(&dst)?;
            let commit = gix_repo
                .find_commit(oid)
                .map_err(|err| OzError::Git(format!("missing cloned HEAD commit {oid}: {err}")))?;
            let tree_id = commit
                .tree_id()
                .map_err(|err| OzError::Git(err.to_string()))?
                .detach();
            base_tree_hex = tree_id.to_hex().to_string();

            if !opts.bare && opts.checkout {
                let missing_blobs = collect_missing_blobs_in_tree(&gix_repo, tree_id)?;
                if !missing_blobs.is_empty() {
                    fetch_specific_objects(
                        &gix_repo,
                        gix_url,
                        token_opt.as_deref(),
                        auth_scheme,
                        &missing_blobs,
                        opts.filter.is_some(),
                    )?;
                }
                let gix_repo = open_gix_unrestricted(&dst)?;
                materialize_tree_from_odb(&gix_repo, tree_id, &dst)?;
                checked_out_files = write_index_from_tree(&gix_repo, tree_id)?;
            }
        }

        // Initialize colocated `.git/ozone/` and record initial OpLog entry.
        let mut op_id = None;
        if !opts.bare {
            let layout = OzoneLayout::from_git_common_dir(&git_dir);
            layout.ensure_initialized()?;
            ensure_oplog_dirs(&layout)?;

            let head_hex = target_oid.map(|id| id.to_hex().to_string());
            let mut ws_snapshots = BTreeMap::new();
            ws_snapshots.insert(
                DEFAULT_WORKSPACE_KEY.to_string(),
                WsSnapshot {
                    base_tree: base_tree_hex.clone(),
                    dirty_tree: Some(base_tree_hex),
                    dirty_files: Vec::new(),
                    head_commit: head_hex.clone(),
                    branch: selected_branch.clone(),
                    pre_base_tree: None,
                    pre_dirty_tree: None,
                    pre_dirty_files: Vec::new(),
                    pre_head_commit: None,
                    pre_branch: None,
                    pre_detached: false,
                    post_detached: selected_branch.is_none() && target_oid.is_some(),
                },
            );

            let entry = OpLogEntry {
                op_id: new_op_id(),
                parents: Vec::new(),
                timestamp: Utc::now(),
                actor,
                kind: OpKind::Mutate,
                command: format!(
                    "oz clone {}{} {}",
                    opts.filter
                        .as_deref()
                        .map(|f| format!("--filter={f} "))
                        .unwrap_or_default(),
                    persisted_url,
                    dst.display()
                ),
                message: Some(format!("cloned from {persisted_url}")),
                inverse_of: None,
                undone_ops: Vec::new(),
                rewound_uncheckpointed: false,
                ref_deltas,
                ws_snapshots,
            };
            let stored = append_entry(&layout, entry)?;
            op_id = Some(stored.op_id);
        }

        Ok(CloneOutcome {
            destination: dst.clone(),
            git_dir,
            remote_name,
            remote_url: persisted_url,
            protocol_version: protocol_number(protocol),
            branch: selected_branch,
            head_oid: target_oid.map(|id| id.to_hex().to_string()),
            partial_clone: opts.filter.is_some(),
            filter: opts.filter.clone(),
            filter_negotiated,
            checked_out_files,
            op_id,
            warnings,
        })
    })();

    if result.is_err() {
        if created_dst {
            let _ = std::fs::remove_dir_all(&dst);
        } else {
            let _ = std::fs::remove_dir_all(dst.join(".git"));
        }
    }

    result
}

/// Fetches from `<remote>` (default `"origin"`) over pure-Rust `gix` Wire Protocol v2, supporting
/// optional refspecs, `--prune`, and `--filter=blob:none`, atomically updating
/// `refs/remotes/<remote>/*` and recording OpLog `ref_deltas`.
pub fn fetch_remote(
    repo: &OzoneRepo,
    opts: &FetchOptions,
    actor: ActorInfo,
) -> Result<FetchOutcome> {
    let repo_cloned = repo.clone();
    let opts_cloned = opts.clone();
    run_blocking_safe(move || fetch_remote_blocking(&repo_cloned, &opts_cloned, actor))
}

fn fetch_remote_blocking(
    repo: &OzoneRepo,
    opts: &FetchOptions,
    actor: ActorInfo,
) -> Result<FetchOutcome> {
    repo.layout.ensure_initialized()?;
    let gix_repo = open_gix_unrestricted(&repo.repo_root)?;

    let remote_arg = opts
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("origin");

    let configured_url = read_remote_url_from_gix(&gix_repo, remote_arg, false)?;
    let (remote_label, raw_url, is_named_remote) = match configured_url {
        Some(url) => (remote_arg.to_string(), url, true),
        None if looks_like_url_or_path(remote_arg) => {
            (remote_arg.to_string(), remote_arg.to_string(), false)
        }
        None => {
            return Err(OzError::InvalidUsage {
                message: format!("remote '{remote_arg}' is not configured"),
                recovery: vec![format!("git remote add {remote_arg} <url>")],
            });
        }
    };

    let configured_filter = if is_named_remote {
        gix_repo
            .config_snapshot()
            .string(&format!("remote.{remote_label}.partialclonefilter"))
            .map(|v| v.to_str_lossy().trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };
    let effective_filter = opts.filter.clone().or(configured_filter);
    if let Some(ref f) = effective_filter {
        validate_filter_spec(f)?;
    }

    let refspec_strings = if !opts.refspecs.is_empty() {
        normalize_fetch_refspecs(&opts.refspecs, &remote_label, is_named_remote)
    } else if is_named_remote {
        let key = format!("remote.{remote_label}.fetch");
        let cfg = gix_repo.config_snapshot();
        let vals: Vec<String> = cfg
            .strings(&key)
            .map(|list| {
                list.into_iter()
                    .map(|v| v.to_str_lossy().trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if vals.is_empty() {
            vec![format!("+refs/heads/*:refs/remotes/{remote_label}/*")]
        } else {
            vals
        }
    } else {
        vec!["+refs/heads/*:refs/remotes/origin/*".to_string()]
    };

    let parsed_specs = parse_fetch_refspecs(&refspec_strings)?;
    let (gix_url, persisted_url, token_opt, auth_scheme) = resolve_transport_url_and_auth(
        &raw_url,
        &repo.repo_root,
        opts.auth_token.as_deref(),
        opts.auth_scheme,
    )?;

    let local_have_oids = collect_local_have_oids(&gix_repo)?;
    let cmd_str = format!(
        "oz fetch {}{}{}",
        if opts.prune { "--prune " } else { "" },
        effective_filter
            .as_deref()
            .map(|f| format!("--filter={f} "))
            .unwrap_or_default(),
        remote_label
    );
    let tx = MutationTransaction::begin_watching_all_refs(repo, cmd_str, actor, None)?;

    let mut warnings = Vec::new();
    let (protocol, adv_refs, pack_received, filter_negotiated) = fetch_pack_session(
        &gix_repo,
        gix_url,
        token_opt.as_deref(),
        auth_scheme,
        effective_filter.as_deref(),
        opts.tags,
        Some(&parsed_specs),
        &local_have_oids,
        &mut warnings,
    )?;

    // Reopen ODB so any newly written packfile is immediately visible before updating refs.
    let gix_repo = open_gix_unrestricted(&repo.repo_root)?;
    let mut ref_edits = Vec::new();
    let mut updated_refs = Vec::new();
    let mut matched_destinations = BTreeSet::new();

    for adv in &adv_refs {
        let Some(new_oid) = adv.oid() else {
            continue;
        };
        let remote_ref_name = adv.name();
        for spec in &parsed_specs {
            if let Some(dst_ref) = match_fetch_refspec(spec, remote_ref_name) {
                matched_destinations.insert(dst_ref.clone());
                let old_oid = read_ref_oid(&gix_repo, &dst_ref)?;
                if old_oid == Some(new_oid) {
                    continue;
                }
                if !spec.allow_non_fast_forward
                    && let Some(old_id) = old_oid
                    && !is_ancestor_commit(&gix_repo, old_id, new_oid)?
                {
                    return Err(OzError::Conflict {
                        message: format!(
                            "non-fast-forward fetch rejected for '{dst_ref}' (remote '{remote_ref_name}' is at {new_oid}, local is at {old_id})"
                        ),
                        conflicts: Vec::new(),
                        recovery: vec![format!("oz fetch {remote_label} +{remote_ref_name}:{dst_ref}")],
                    });
                }

                let full = FullName::try_from(dst_ref.as_str())
                    .map_err(|err| OzError::Git(format!("invalid ref '{dst_ref}': {err}")))?;
                let forced = spec.allow_non_fast_forward
                    && old_oid.is_some_and(|old_id| {
                        !is_ancestor_commit(&gix_repo, old_id, new_oid).unwrap_or(false)
                    });
                ref_edits.push(RefEdit::update(
                    full,
                    new_oid,
                    PreviousValue::Any,
                    format!("oz fetch {remote_label}"),
                ));
                updated_refs.push(FetchedRefUpdate {
                    remote_ref: remote_ref_name.to_string(),
                    local_ref: dst_ref,
                    old_oid: old_oid.map(|id| id.to_hex().to_string()),
                    new_oid: new_oid.to_hex().to_string(),
                    forced,
                });
            }
        }

        if opts.tags && remote_ref_name.starts_with("refs/tags/") {
            let old_oid = read_ref_oid(&gix_repo, remote_ref_name)?;
            if old_oid.is_none()
                && let Ok(full) = FullName::try_from(remote_ref_name)
            {
                ref_edits.push(RefEdit::update(
                    full,
                    new_oid,
                    PreviousValue::Any,
                    format!("oz fetch {remote_label}"),
                ));
                updated_refs.push(FetchedRefUpdate {
                    remote_ref: remote_ref_name.to_string(),
                    local_ref: remote_ref_name.to_string(),
                    old_oid: None,
                    new_oid: new_oid.to_hex().to_string(),
                    forced: false,
                });
            }
        }
    }

    let mut pruned_refs = Vec::new();
    if opts.prune && is_named_remote && opts.refspecs.is_empty() {
        let prefix = format!("refs/remotes/{remote_label}/");
        let remote_head = format!("refs/remotes/{remote_label}/HEAD");
        let platform = gix_repo
            .references()
            .map_err(|err| OzError::Git(err.to_string()))?;
        let remotes = platform
            .remote_branches()
            .map_err(|err| OzError::Git(err.to_string()))?;

        for item in remotes {
            let reference = item.map_err(|err| OzError::Git(err.to_string()))?;
            let full_str = reference.name().as_bstr().to_str_lossy().into_owned();
            if !full_str.starts_with(&prefix) || full_str == remote_head {
                continue;
            }
            if matches!(reference.target(), gix::refs::TargetRef::Symbolic(_)) {
                continue;
            }
            if !matched_destinations.contains(&full_str) {
                let full = FullName::try_from(full_str.as_str())
                    .map_err(|err| OzError::Git(err.to_string()))?;
                ref_edits.push(RefEdit {
                    change: Change::Delete {
                        expected: PreviousValue::Any,
                        log: RefLog::AndReference,
                    },
                    name: full,
                    deref: false,
                });
                pruned_refs.push(full_str);
            }
        }
    }

    if !ref_edits.is_empty() {
        gix_repo
            .edit_references(ref_edits)
            .map_err(|err| OzError::Git(format!("atomic fetch ref update failed: {err}")))?;
    }

    let op_entry = tx.finish_if_changed(repo, None)?;

    Ok(FetchOutcome {
        remote: remote_label,
        remote_url: persisted_url,
        protocol_version: protocol_number(protocol),
        pack_received,
        filter_negotiated,
        updated_refs,
        pruned_refs,
        op_id: op_entry.map(|e| e.op_id),
        warnings,
    })
}

/// Pushes local branches/refs over pure-Rust smart `git-receive-pack` (`file://`/local, SSH, or
/// HTTP/HTTPS), supporting `--atomic`, `--force-with-lease[=<ref>:<oid>]`, `--delete`, and
/// `--set-upstream`, plus updating `refs/remotes/<remote>/*` and recording an OpLog transaction.
pub fn push_refs(repo: &OzoneRepo, opts: &PushOptions, actor: ActorInfo) -> Result<PushOutcome> {
    let repo_cloned = repo.clone();
    let opts_cloned = opts.clone();
    run_blocking_safe(move || push_refs_blocking(&repo_cloned, &opts_cloned, actor))
}

#[derive(Debug, Clone)]
struct ResolvedPushSpec {
    local_ref: Option<String>,
    local_branch_short: Option<String>,
    remote_ref: String,
    new_oid: ObjectId,
    force_prefix: bool,
    is_delete: bool,
}

fn push_refs_blocking(
    repo: &OzoneRepo,
    opts: &PushOptions,
    actor: ActorInfo,
) -> Result<PushOutcome> {
    repo.layout.ensure_initialized()?;
    let gix_repo = open_gix_unrestricted(&repo.repo_root)?;

    let remote_arg = opts
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("origin");

    let configured_url = read_remote_url_from_gix(&gix_repo, remote_arg, true)?;
    let (remote_label, raw_url, is_named_remote) = match configured_url {
        Some(url) => (remote_arg.to_string(), url, true),
        None if looks_like_url_or_path(remote_arg) => {
            (remote_arg.to_string(), remote_arg.to_string(), false)
        }
        None => {
            return Err(OzError::InvalidUsage {
                message: format!("remote '{remote_arg}' is not configured"),
                recovery: vec![format!("git remote add {remote_arg} <url>")],
            });
        }
    };

    let push_specs = resolve_push_specs(repo, &gix_repo, opts)?;
    if push_specs.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "nothing specified to push (HEAD is detached or unborn and no refspecs were provided)"
                .to_string(),
            recovery: vec![format!("oz push {remote_label} <branch>")],
        });
    }

    let (gix_url, persisted_url, token_opt, auth_scheme) = resolve_transport_url_and_auth(
        &raw_url,
        &repo.repo_root,
        opts.auth_token.as_deref(),
        opts.auth_scheme,
    )?;

    let mut transport = connect_transport(
        gix_url,
        Protocol::V1,
        token_opt.as_deref(),
        auth_scheme,
        &gix_repo,
    )?;

    let mut advertised_tips: BTreeMap<String, ObjectId> = BTreeMap::new();
    let capabilities = {
        let hs = transport
            .handshake(Service::ReceivePack, &[])
            .map_err(|err| map_transport_error("git-receive-pack handshake", err))?;

        if let Some(mut refs_reader) = hs.refs {
            let mut line = String::new();
            while refs_reader
                .readline_str(&mut line)
                .map_err(|err| OzError::Git(format!("failed reading receive-pack advertised refs: {err}")))?
                != 0
            {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if let Some((oid_str, ref_name)) = trimmed.split_once(' ')
                    && ref_name != "capabilities^{}"
                    && !ref_name.ends_with("^{}")
                    && let Ok(oid) = ObjectId::from_hex(oid_str.as_bytes())
                    && !oid.is_null()
                {
                    advertised_tips.insert(ref_name.to_string(), oid);
                }
                line.clear();
            }
        }
        hs.capabilities.clone()
    };

    // 1. Validate `--atomic` capability before sending any commands/packs.
    if opts.atomic && !capabilities.contains("atomic") {
        return Err(OzError::CheckFailed {
            message: format!(
                "remote '{remote_label}' does not advertise the 'atomic' push capability required by --atomic"
            ),
            recovery: vec!["Retry without --atomic or upgrade the remote Git server.".to_string()],
        });
    }
    if push_specs.iter().any(|s| s.is_delete) && !capabilities.contains("delete-refs") {
        return Err(OzError::CheckFailed {
            message: format!("remote '{remote_label}' does not allow deleting references ('delete-refs' missing)"),
            recovery: Vec::new(),
        });
    }

    // 2. Validate `--force-with-lease` expectations atomically BEFORE transmitting anything.
    let has_any_lease = opts.force_with_lease || !opts.lease_specs.is_empty();
    let mut lease_validated_refs: HashSet<String> = HashSet::new();
    if has_any_lease {
        validate_push_leases(
            &gix_repo,
            &remote_label,
            is_named_remote,
            &push_specs,
            &advertised_tips,
            opts.force_with_lease,
            &opts.lease_specs,
            &mut lease_validated_refs,
        )?;
    }

    // 3. Validate fast-forward rules for refs not forced via `--force`, `+`, or a verified lease.
    let null_oid = ObjectId::null(gix_repo.object_hash());
    for spec in &push_specs {
        let old_oid = advertised_tips.get(&spec.remote_ref).copied();
        if !spec.is_delete
            && let Some(old_id) = old_oid
            && old_id != spec.new_oid
        {
            let allowed_non_ff = opts.force
                || spec.force_prefix
                || lease_validated_refs.contains(&spec.remote_ref);
            if !allowed_non_ff && !is_ancestor_commit(&gix_repo, old_id, spec.new_oid)? {
                return Err(OzError::Conflict {
                    message: format!(
                        "push to '{}' rejected (non-fast-forward: remote is at {}, local is at {})",
                        spec.remote_ref, old_id, spec.new_oid
                    ),
                    conflicts: vec![ConflictEntry {
                        id: "push-non-ff".to_string(),
                        path: spec.remote_ref.clone(),
                        message: "non-fast-forward push rejected".to_string(),
                        base: None,
                        ours: Some(spec.new_oid.to_hex().to_string()),
                        theirs: Some(old_id.to_hex().to_string()),
                        suggested_commands: vec![
                            format!("oz fetch {remote_label}"),
                            format!("oz push {remote_label} --force-with-lease"),
                        ],
                    }],
                    recovery: vec![
                        "oz sync".to_string(),
                        format!("oz push {remote_label} --force-with-lease"),
                    ],
                });
            }
        }
    }

    // 4. Generate minimal packfile walking missing objects between `advertised_tips` and `new_oid`s.
    let any_non_delete = push_specs.iter().any(|s| !s.is_delete);
    let (pack_bytes, pack_objects_sent) = if any_non_delete {
        let wanted: Vec<ObjectId> = push_specs
            .iter()
            .filter(|s| !s.is_delete && !s.new_oid.is_null())
            .map(|s| s.new_oid)
            .collect();
        let remote_known: Vec<ObjectId> = advertised_tips.values().copied().collect();
        build_push_packfile(&gix_repo, &wanted, &remote_known)?
    } else {
        (Vec::new(), 0)
    };
    let pack_bytes_sent = pack_bytes.len();

    // 5. Transmit pkt-line commands + flush + packfile and parse `report-status`.
    let use_sideband = capabilities.contains("side-band-64k");
    let mut client_caps = vec!["report-status".to_string()];
    if use_sideband {
        client_caps.push("side-band-64k".to_string());
    }
    if capabilities.contains("ofs-delta") {
        client_caps.push("ofs-delta".to_string());
    }
    if opts.atomic {
        client_caps.push("atomic".to_string());
    }
    client_caps.push(format!("agent={USER_AGENT_VALUE}"));

    let tx_cmd = format!(
        "oz push {}{}{}",
        if opts.atomic { "--atomic " } else { "" },
        if has_any_lease {
            "--force-with-lease "
        } else {
            ""
        },
        remote_label
    );
    let tx = MutationTransaction::begin_watching_all_refs(repo, tx_cmd, actor, None)?;

    let req_writer = transport
        .request(WriteMode::Binary, MessageKind::Flush, false)
        .map_err(|err| map_transport_error("git-receive-pack request", err))?;
    let (mut raw_writer, mut response_reader) = req_writer.into_parts();

    for (idx, spec) in push_specs.iter().enumerate() {
        let old_oid = advertised_tips
            .get(&spec.remote_ref)
            .copied()
            .unwrap_or(null_oid);
        let line = if idx == 0 {
            format!(
                "{} {} {}\0{}\n",
                old_oid.to_hex(),
                spec.new_oid.to_hex(),
                spec.remote_ref,
                client_caps.join(" ")
            )
        } else {
            format!(
                "{} {} {}\n",
                old_oid.to_hex(),
                spec.new_oid.to_hex(),
                spec.remote_ref
            )
        };
        gix_packetline::blocking_io::encode::data_to_write(line.as_bytes(), &mut raw_writer)
            .map_err(|err| OzError::Git(format!("failed encoding push command pkt-line: {err}")))?;
    }
    gix_packetline::blocking_io::encode::flush_to_write(&mut raw_writer)
        .map_err(|err| OzError::Git(format!("failed encoding push flush pkt-line: {err}")))?;

    if any_non_delete {
        raw_writer
            .write_all(&pack_bytes)
            .map_err(|err| OzError::Git(format!("failed streaming packfile to remote: {err}")))?;
    }
    raw_writer
        .flush()
        .map_err(|err| OzError::Git(format!("failed flushing push request: {err}")))?;
    drop(raw_writer);

    let status_report = parse_receive_pack_report(&mut response_reader, use_sideband)?;
    drop(response_reader);

    if status_report.unpack_status != "ok" {
        return Err(OzError::CheckFailed {
            message: format!(
                "remote '{}' failed to unpack pushed objects: {}",
                remote_label, status_report.unpack_status
            ),
            recovery: Vec::new(),
        });
    }

    let mut rejected_messages = Vec::new();
    for (ref_name, reason) in &status_report.rejected_refs {
        rejected_messages.push(format!("{ref_name}: {reason}"));
    }
    if !rejected_messages.is_empty() {
        return Err(OzError::Conflict {
            message: format!(
                "remote '{}' rejected push ({})",
                remote_label,
                rejected_messages.join("; ")
            ),
            conflicts: status_report
                .rejected_refs
                .iter()
                .enumerate()
                .map(|(i, (r, reason))| ConflictEntry {
                    id: format!("push-ng-{}", i + 1),
                    path: r.clone(),
                    message: reason.clone(),
                    base: None,
                    ours: None,
                    theirs: Some(reason.clone()),
                    suggested_commands: vec![format!("oz fetch {remote_label}"), "oz sync".to_string()],
                })
                .collect(),
            recovery: vec![format!("oz fetch {remote_label}"), "oz sync".to_string()],
        });
    }

    // 6. Update `refs/remotes/<remote>/<branch>`, `--set-upstream` config, and finish OpLog!
    let mut tracking_edits = Vec::new();
    let mut pushed_refs = Vec::with_capacity(push_specs.len());
    let mut upstream_configured = Vec::new();

    for spec in &push_specs {
        let old_oid = advertised_tips.get(&spec.remote_ref).copied();
        let forced = !spec.is_delete
            && old_oid.is_some_and(|old_id| {
                old_id != spec.new_oid
                    && !is_ancestor_commit(&gix_repo, old_id, spec.new_oid).unwrap_or(false)
            });

        if is_named_remote
            && let Some(short_remote_branch) = spec.remote_ref.strip_prefix("refs/heads/")
        {
            let tracking_ref = format!("refs/remotes/{remote_label}/{short_remote_branch}");
            if let Ok(full) = FullName::try_from(tracking_ref.as_str()) {
                if spec.is_delete {
                    tracking_edits.push(RefEdit {
                        change: Change::Delete {
                            expected: PreviousValue::Any,
                            log: RefLog::AndReference,
                        },
                        name: full,
                        deref: false,
                    });
                } else {
                    tracking_edits.push(RefEdit::update(
                        full,
                        spec.new_oid,
                        PreviousValue::Any,
                        format!("oz push {remote_label}"),
                    ));
                }
            }

            if opts.set_upstream
                && !spec.is_delete
                && let Some(ref local_short) = spec.local_branch_short
            {
                write_branch_upstream_config(
                    &repo.git_common_dir,
                    local_short,
                    &remote_label,
                    &spec.remote_ref,
                )?;
                upstream_configured.push(local_short.clone());
            }
        }

        pushed_refs.push(PushedRefUpdate {
            local_ref: spec.local_ref.clone(),
            remote_ref: spec.remote_ref.clone(),
            old_oid: old_oid.map(|id| id.to_hex().to_string()),
            new_oid: (!spec.is_delete).then(|| spec.new_oid.to_hex().to_string()),
            deleted: spec.is_delete,
            forced,
        });
    }

    if !tracking_edits.is_empty() {
        gix_repo
            .edit_references(tracking_edits)
            .map_err(|err| OzError::Git(format!("failed updating local remote-tracking refs: {err}")))?;
    }

    let op_entry = tx.finish(repo, None)?;

    Ok(PushOutcome {
        remote: remote_label,
        remote_url: persisted_url,
        atomic: opts.atomic,
        pack_objects_sent,
        pack_bytes_sent,
        pushed_refs,
        upstream_configured,
        op_id: Some(op_entry.op_id),
    })
}

#[expect(clippy::too_many_arguments)]
fn validate_push_leases(
    gix_repo: &gix::Repository,
    remote_label: &str,
    is_named_remote: bool,
    push_specs: &[ResolvedPushSpec],
    advertised_tips: &BTreeMap<String, ObjectId>,
    use_default_lease: bool,
    lease_specs: &[ForceLeaseSpec],
    validated_out: &mut HashSet<String>,
) -> Result<()> {
    let mut explicit_by_ref: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut wildcard_default = use_default_lease;

    for lease in lease_specs {
        match &lease.ref_name {
            Some(raw_ref) => {
                let full_ref = normalize_dest_ref(raw_ref);
                explicit_by_ref.insert(full_ref, lease.expected_oid.clone());
            }
            None => {
                wildcard_default = true;
            }
        }
    }

    for spec in push_specs {
        let check_entry = if let Some(exp_opt) = explicit_by_ref.get(&spec.remote_ref) {
            Some(exp_opt.clone())
        } else if wildcard_default {
            Some(None)
        } else {
            None
        };

        let Some(explicit_expected_oid) = check_entry else {
            continue;
        };

        let expected_oid: Option<ObjectId> = match explicit_expected_oid {
            Some(hex) => {
                let trimmed = hex.trim();
                if trimmed.is_empty() || trimmed.chars().all(|c| c == '0') {
                    None
                } else {
                    Some(ObjectId::from_hex(trimmed.as_bytes()).map_err(|err| {
                        OzError::InvalidUsage {
                            message: format!(
                                "invalid expected OID '{trimmed}' in --force-with-lease for '{}': {err}",
                                spec.remote_ref
                            ),
                            recovery: Vec::new(),
                        }
                    })?)
                }
            }
            None => {
                if is_named_remote
                    && let Some(short) = spec.remote_ref.strip_prefix("refs/heads/")
                {
                    let tracking = format!("refs/remotes/{remote_label}/{short}");
                    read_ref_oid(gix_repo, &tracking)?
                } else {
                    None
                }
            }
        };

        let actual_remote_oid = advertised_tips.get(&spec.remote_ref).copied();
        if actual_remote_oid != expected_oid {
            let expected_desc = expected_oid
                .map(|id| id.to_hex().to_string())
                .unwrap_or_else(|| "<absent>".to_string());
            let actual_desc = actual_remote_oid
                .map(|id| id.to_hex().to_string())
                .unwrap_or_else(|| "<absent>".to_string());

            return Err(OzError::Conflict {
                message: format!(
                    "stale info: --force-with-lease rejected update to '{}' (expected remote at {}, but remote is at {})",
                    spec.remote_ref, expected_desc, actual_desc
                ),
                conflicts: vec![ConflictEntry {
                    id: "stale-lease".to_string(),
                    path: spec.remote_ref.clone(),
                    message: format!(
                        "stale lease: expected {expected_desc}, remote at {actual_desc}"
                    ),
                    base: Some(expected_desc),
                    ours: (!spec.is_delete).then(|| spec.new_oid.to_hex().to_string()),
                    theirs: Some(actual_desc),
                    suggested_commands: vec![
                        format!("oz fetch {remote_label}"),
                        "oz sync".to_string(),
                    ],
                }],
                recovery: vec![
                    format!("oz fetch {remote_label}"),
                    "oz sync".to_string(),
                ],
            });
        }

        validated_out.insert(spec.remote_ref.clone());
    }

    // Also validate any explicit `--force-with-lease=<ref>:<oid>` targeting a precondition ref not in `push_specs`.
    for (full_ref, exp_opt) in &explicit_by_ref {
        if validated_out.contains(full_ref) {
            continue;
        }
        let expected_oid = match exp_opt {
            Some(hex) => {
                let trimmed = hex.trim();
                if trimmed.is_empty() || trimmed.chars().all(|c| c == '0') {
                    None
                } else {
                    Some(ObjectId::from_hex(trimmed.as_bytes()).map_err(|err| {
                        OzError::InvalidUsage {
                            message: format!(
                                "invalid expected OID '{trimmed}' in --force-with-lease for '{full_ref}': {err}"
                            ),
                            recovery: Vec::new(),
                        }
                    })?)
                }
            }
            None => {
                if is_named_remote && let Some(short) = full_ref.strip_prefix("refs/heads/") {
                    let tracking = format!("refs/remotes/{remote_label}/{short}");
                    read_ref_oid(gix_repo, &tracking)?
                } else {
                    None
                }
            }
        };
        let actual_remote_oid = advertised_tips.get(full_ref).copied();
        if actual_remote_oid != expected_oid {
            return Err(OzError::Conflict {
                message: format!(
                    "stale info: --force-with-lease precondition failed on '{full_ref}'"
                ),
                conflicts: Vec::new(),
                recovery: vec![format!("oz fetch {remote_label}")],
            });
        }
    }

    Ok(())
}

#[derive(Debug)]
struct ReceivePackStatus {
    unpack_status: String,
    #[allow(dead_code)]
    accepted_refs: Vec<String>,
    rejected_refs: Vec<(String, String)>,
}

fn parse_receive_pack_report<'a>(
    reader: &mut Box<dyn gix_transport::client::blocking_io::ExtendedBufRead<'a> + Unpin + 'a>,
    use_sideband: bool,
) -> Result<ReceivePackStatus> {
    let remote_errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let lines: Vec<String> = if use_sideband {
        let mut raw_status_stream = Vec::new();
        {
            let errs_ref = Arc::clone(&remote_errors);
            reader.set_progress_handler(Some(Box::new(move |is_err, msg| {
                if is_err {
                    errs_ref
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(String::from_utf8_lossy(msg).trim().to_string());
                }
                ControlFlow::Continue(())
            })));
            reader
                .read_to_end(&mut raw_status_stream)
                .map_err(|err| {
                    let errs = remote_errors.lock().unwrap_or_else(|p| p.into_inner());
                    if !errs.is_empty() {
                        OzError::Git(format!(
                            "remote receive-pack error: {}",
                            errs.join("; ")
                        ))
                    } else {
                        OzError::Git(format!("failed reading receive-pack status stream: {err}"))
                    }
                })?;
        }
        let mut out_lines = Vec::new();
        let mut pkt_iter = gix_packetline::blocking_io::StreamingPeekableIter::new(
            &raw_status_stream[..],
            &[PacketLineRef::Flush],
            false,
        );
        while let Some(pkt_res) = pkt_iter.read_line() {
            let pkt = pkt_res
                .map_err(OzError::Io)?
                .map_err(|err| OzError::Git(format!("corrupt status packetline: {err}")))?;
            if let PacketLineRef::Data(bytes) = pkt {
                let s = String::from_utf8_lossy(bytes)
                    .trim_end_matches(['\r', '\n'])
                    .to_string();
                if !s.is_empty() {
                    out_lines.push(s);
                }
            }
        }
        out_lines
    } else {
        let mut out_lines = Vec::new();
        let mut buf = String::new();
        while reader
            .readline_str(&mut buf)
            .map_err(|err| OzError::Git(format!("failed reading receive-pack status: {err}")))?
            != 0
        {
            let s = buf.trim_end_matches(['\r', '\n']).to_string();
            if !s.is_empty() {
                out_lines.push(s);
            }
            buf.clear();
        }
        out_lines
    };

    let recorded_errs = remote_errors.lock().unwrap_or_else(|p| p.into_inner()).clone();
    if !recorded_errs.is_empty() && lines.is_empty() {
        return Err(OzError::Git(format!(
            "remote receive-pack error: {}",
            recorded_errs.join("; ")
        )));
    }

    let mut unpack_status = "ok".to_string();
    let mut accepted_refs = Vec::new();
    let mut rejected_refs = Vec::new();

    for line in lines {
        let cleaned = line.trim_end_matches('\0');
        if let Some(rest) = cleaned.strip_prefix("unpack ") {
            unpack_status = rest.trim().to_string();
        } else if let Some(rest) = cleaned.strip_prefix("ok ") {
            accepted_refs.push(rest.trim().to_string());
        } else if let Some(rest) = cleaned.strip_prefix("ng ") {
            let (ref_name, reason) = rest
                .split_once(' ')
                .map(|(r, msg)| (r.trim().to_string(), msg.trim().to_string()))
                .unwrap_or_else(|| (rest.trim().to_string(), "rejected".to_string()));
            rejected_refs.push((ref_name, reason));
        }
    }

    Ok(ReceivePackStatus {
        unpack_status,
        accepted_refs,
        rejected_refs,
    })
}

/// Walks the local ODB between `wanted_tips` and `remote_known_tips` and encodes a pure-Rust
/// Git v2 packfile (`PACK` header + deflated object entries + SHA-1 checksum).
fn build_push_packfile(
    gix_repo: &gix::Repository,
    wanted_tips: &[ObjectId],
    remote_known_tips: &[ObjectId],
) -> Result<(Vec<u8>, usize)> {
    let excluded = collect_known_object_closure(gix_repo, remote_known_tips);
    let missing_oids = collect_missing_push_objects(gix_repo, wanted_tips, &excluded)?;
    let num_entries = missing_oids.len();

    let mut entries = Vec::with_capacity(num_entries);
    let mut buf = Vec::new();
    for oid in &missing_oids {
        let obj = gix_repo
            .objects
            .try_find(oid, &mut buf)
            .map_err(|err| OzError::Git(format!("failed reading local object {oid} for push: {err}")))?
            .ok_or_else(|| OzError::Git(format!("missing required local object {oid} during push")))?;
        let count = pack_out::Count::from_data(*oid, None);
        let entry = pack_out::Entry::from_data(&count, &obj, Default::default())
            .map_err(|err| OzError::Git(format!("failed compressing pack entry {oid}: {err}")))?;
        entries.push(entry);
    }

    let mut pack_bytes = Vec::new();
    let chunk_iter: Box<dyn Iterator<Item = std::result::Result<Vec<pack_out::Entry>, gix::error::Error>>> =
        if entries.is_empty() {
            Box::new(std::iter::empty())
        } else {
            Box::new(std::iter::once(Ok(entries)))
        };

    let pack_writer = pack_out::bytes::FromEntriesIter::new(
        chunk_iter,
        &mut pack_bytes,
        u32::try_from(num_entries).map_err(|_| OzError::Other("packfile exceeds u32 object limit".to_string()))?,
        gix_pack::data::Version::V2,
        gix_repo.object_hash(),
    );

    for step in pack_writer {
        step.map_err(|err| OzError::Git(format!("packfile encoding failed: {err}")))?;
    }

    Ok((pack_bytes, num_entries))
}

/// Collects all commit, tag, tree, and blob OIDs reachable from `remote_known_tips` without
/// reading blob payloads (so partial-clone `blob:none` repos work without missing-blob errors!).
fn collect_known_object_closure(
    gix_repo: &gix::Repository,
    remote_known_tips: &[ObjectId],
) -> HashSet<ObjectId> {
    let mut known = HashSet::new();
    let mut commit_queue = VecDeque::new();
    let mut tree_queue = VecDeque::new();
    let mut buf = Vec::new();

    for &tip in remote_known_tips {
        if tip.is_null() {
            continue;
        }
        let Ok(Some(obj)) = gix_repo.objects.try_find(&tip, &mut buf) else {
            continue;
        };
        match obj.kind {
            ObjectKind::Commit => {
                if known.insert(tip) {
                    commit_queue.push_back(tip);
                }
            }
            ObjectKind::Tag => {
                known.insert(tip);
                if let Some(tag_ref) = obj.try_into_tag_iter()
                    && let Some(target_id) = tag_ref.into_iter().find_map(|tok| match tok {
                        Ok(gix::objs::tag::ref_iter::Token::Target { id }) => Some(id),
                        _ => None,
                    })
                    && known.insert(target_id)
                {
                    commit_queue.push_back(target_id);
                }
            }
            ObjectKind::Tree => {
                if known.insert(tip) {
                    tree_queue.push_back(tip);
                }
            }
            ObjectKind::Blob => {
                known.insert(tip);
            }
        }
    }

    // Bound ancestor walk generously so huge histories stay fast while covering full branch bases.
    let mut walked_commits = 0usize;
    while let Some(cid) = commit_queue.pop_front() {
        if walked_commits >= 4096 {
            break;
        }
        walked_commits += 1;
        let Ok(commit) = gix_repo.find_commit(cid) else {
            continue;
        };
        if let Ok(tid) = commit.tree_id() {
            let tid = tid.detach();
            if known.insert(tid) {
                tree_queue.push_back(tid);
            }
        }
        for pid in commit.parent_ids() {
            let pid = pid.detach();
            if known.insert(pid) {
                commit_queue.push_back(pid);
            }
        }
    }

    while let Some(tid) = tree_queue.pop_front() {
        let Ok(tree) = gix_repo.find_tree(tid) else {
            continue;
        };
        for entry_res in tree.iter() {
            let Ok(entry) = entry_res else {
                continue;
            };
            let entry_oid = entry.oid().to_owned();
            if entry.mode().is_commit() {
                continue;
            }
            if entry.mode().is_tree() {
                if known.insert(entry_oid) {
                    tree_queue.push_back(entry_oid);
                }
            } else {
                // Record blob OID as known on remote WITHOUT reading the blob from local ODB!
                known.insert(entry_oid);
            }
        }
    }

    known
}

fn collect_missing_push_objects(
    gix_repo: &gix::Repository,
    wanted_tips: &[ObjectId],
    excluded: &HashSet<ObjectId>,
) -> Result<Vec<ObjectId>> {
    let mut visited = excluded.clone();
    let mut out = Vec::new();
    let mut commit_queue = VecDeque::new();
    let mut tree_queue = VecDeque::new();
    let mut buf = Vec::new();

    for &tip in wanted_tips {
        if tip.is_null() || !visited.insert(tip) {
            continue;
        }
        let obj = gix_repo
            .objects
            .try_find(&tip, &mut buf)
            .map_err(|err| OzError::Git(err.to_string()))?
            .ok_or_else(|| OzError::Git(format!("pushed target object {tip} not found locally")))?;

        match obj.kind {
            ObjectKind::Tag => {
                out.push(tip);
                if let Some(tag_ref) = obj.try_into_tag_iter() {
                    for tok in tag_ref.flatten() {
                        if let gix::objs::tag::ref_iter::Token::Target { id } = tok
                            && visited.insert(id)
                        {
                            commit_queue.push_back(id);
                        }
                    }
                }
            }
            ObjectKind::Commit => {
                commit_queue.push_back(tip);
            }
            ObjectKind::Tree => {
                tree_queue.push_back(tip);
            }
            ObjectKind::Blob => {
                out.push(tip);
            }
        }
    }

    while let Some(cid) = commit_queue.pop_front() {
        out.push(cid);
        let commit = gix_repo
            .find_commit(cid)
            .map_err(|err| OzError::Git(format!("missing commit {cid}: {err}")))?;
        let tid = commit
            .tree_id()
            .map_err(|err| OzError::Git(err.to_string()))?
            .detach();
        if visited.insert(tid) {
            tree_queue.push_back(tid);
        }
        for pid in commit.parent_ids() {
            let pid = pid.detach();
            if visited.insert(pid) {
                // Verify parent commit exists locally; if local repo is shallow, stop cleanly if absent.
                if gix_repo.find_commit(pid).is_ok() {
                    commit_queue.push_back(pid);
                }
            }
        }
    }

    while let Some(tid) = tree_queue.pop_front() {
        out.push(tid);
        let tree = gix_repo
            .find_tree(tid)
            .map_err(|err| OzError::Git(format!("missing tree {tid}: {err}")))?;
        for entry_res in tree.iter() {
            let entry = entry_res.map_err(|err| OzError::Git(err.to_string()))?;
            if entry.mode().is_commit() {
                continue;
            }
            let entry_oid = entry.oid().to_owned();
            if visited.insert(entry_oid) {
                if entry.mode().is_tree() {
                    tree_queue.push_back(entry_oid);
                } else {
                    out.push(entry_oid);
                }
            }
        }
    }

    Ok(out)
}

#[derive(Debug, Clone)]
struct ParsedFetchSpec {
    source: String,
    destination: Option<String>,
    allow_non_fast_forward: bool,
    is_wildcard: bool,
}

fn parse_fetch_refspecs(specs: &[String]) -> Result<Vec<ParsedFetchSpec>> {
    let mut out = Vec::with_capacity(specs.len());
    for raw in specs {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (allow_non_fast_forward, body) = match trimmed.strip_prefix('+') {
            Some(rest) => (true, rest),
            None => (false, trimmed),
        };
        let (src, dst) = match body.split_once(':') {
            Some((s, d)) => (
                s.trim().to_string(),
                (!d.trim().is_empty()).then(|| d.trim().to_string()),
            ),
            None => (body.to_string(), None),
        };
        let is_wildcard = src.contains('*');
        if is_wildcard
            && let Some(ref d) = dst
            && !d.contains('*')
        {
            return Err(OzError::InvalidUsage {
                message: format!("invalid wildcard fetch refspec '{raw}'"),
                recovery: Vec::new(),
            });
        }
        out.push(ParsedFetchSpec {
            source: src,
            destination: dst,
            allow_non_fast_forward,
            is_wildcard,
        });
    }
    Ok(out)
}

fn normalize_fetch_refspecs(
    raw_specs: &[String],
    remote_label: &str,
    is_named_remote: bool,
) -> Vec<String> {
    raw_specs
        .iter()
        .map(|s| {
            let trimmed = s.trim();
            let (plus, rest) = match trimmed.strip_prefix('+') {
                Some(r) => ("+", r),
                None => ("", trimmed),
            };
            if let Some((src, dst)) = rest.split_once(':') {
                let full_src = normalize_source_ref_pattern(src.trim());
                let full_dst = if dst.trim().is_empty() {
                    String::new()
                } else if dst.trim().starts_with("refs/") {
                    dst.trim().to_string()
                } else {
                    format!("refs/heads/{}", dst.trim())
                };
                if full_dst.is_empty() {
                    format!("{plus}{full_src}")
                } else {
                    format!("{plus}{full_src}:{full_dst}")
                }
            } else {
                let short = rest.trim().strip_prefix("refs/heads/").unwrap_or(rest.trim());
                let full_src = normalize_source_ref_pattern(rest.trim());
                if is_named_remote && !full_src.starts_with("refs/tags/") && rest.trim() != "HEAD" {
                    format!("{plus}{full_src}:refs/remotes/{remote_label}/{short}")
                } else {
                    format!("{plus}{full_src}")
                }
            }
        })
        .collect()
}

fn normalize_source_ref_pattern(src: &str) -> String {
    if src == "HEAD" || src.starts_with("refs/") {
        src.to_string()
    } else {
        format!("refs/heads/{src}")
    }
}

fn match_fetch_refspec(spec: &ParsedFetchSpec, remote_ref: &str) -> Option<String> {
    if spec.is_wildcard {
        let (src_prefix, src_suffix) = spec.source.split_once('*')?;
        if !remote_ref.starts_with(src_prefix) || !remote_ref.ends_with(src_suffix) {
            return None;
        }
        let middle_end = remote_ref.len().checked_sub(src_suffix.len())?;
        if middle_end < src_prefix.len() {
            return None;
        }
        let captured = &remote_ref[src_prefix.len()..middle_end];
        let dst_pat = spec.destination.as_deref()?;
        let (dst_prefix, dst_suffix) = dst_pat.split_once('*')?;
        Some(format!("{dst_prefix}{captured}{dst_suffix}"))
    } else if spec.source == remote_ref {
        spec.destination.clone()
    } else {
        None
    }
}

/// Performs a single pure-Rust `git-upload-pack` session (`ls-refs` + `fetch`) over Protocol v2
/// (with automatic fallback to Protocol v1 if downgraded by the remote).
#[expect(clippy::too_many_arguments)]
fn fetch_pack_session(
    gix_repo: &gix::Repository,
    gix_url: gix::url::Url,
    auth_token: Option<&str>,
    auth_scheme: HttpAuthScheme,
    filter_spec: Option<&str>,
    include_tags: bool,
    fetch_specs: Option<&[ParsedFetchSpec]>,
    local_have_oids: &[ObjectId],
    warnings: &mut Vec<String>,
) -> Result<(Protocol, Vec<AdvertisedRef>, bool, bool)> {
    let mut transport = connect_transport(
        gix_url,
        Protocol::V2,
        auth_token,
        auth_scheme,
        gix_repo,
    )?;

    let (protocol, capabilities, v1_refs) = {
        let mut hs = transport
            .handshake(Service::UploadPack, &[])
            .map_err(|err| map_transport_error("git-upload-pack handshake", err))?;
        let protocol = hs.actual_protocol;
        let capabilities = hs.capabilities.clone();
        let v1_refs = if matches!(protocol, Protocol::V0 | Protocol::V1) {
            Some(parse_v1_handshake_refs(&mut hs)?)
        } else {
            None
        };
        (protocol, capabilities, v1_refs)
    };

    let advertised_refs = match protocol {
        Protocol::V2 => invoke_v2_ls_refs(&mut transport, &capabilities)?,
        Protocol::V0 | Protocol::V1 => v1_refs.unwrap_or_default(),
    };

    // Determine which remote OIDs we actually want to fetch.
    let mut wanted_set = BTreeSet::new();
    for adv in &advertised_refs {
        let Some(oid) = adv.oid() else {
            continue;
        };
        let rname = adv.name();
        let matches_interest = match fetch_specs {
            Some(specs) => {
                specs.iter().any(|sp| {
                    sp.source == rname
                        || (sp.is_wildcard && match_fetch_refspec(sp, rname).is_some())
                }) || (include_tags && rname.starts_with("refs/tags/"))
            }
            None => {
                rname == "HEAD"
                    || rname.starts_with("refs/heads/")
                    || (include_tags && rname.starts_with("refs/tags/"))
            }
        };
        if !matches_interest {
            continue;
        }
        if !has_commit_locally(gix_repo, oid) {
            wanted_set.insert(oid);
        }
    }

    if wanted_set.is_empty() {
        if protocol == Protocol::V2 && transport.connection_persists_across_multiple_requests() {
            send_graceful_flush(&mut transport);
        }
        return Ok((protocol, advertised_refs, false, false));
    }

    let wanted_oids: Vec<ObjectId> = wanted_set.into_iter().collect();
    let (pack_written, filter_negotiated) = match protocol {
        Protocol::V2 => invoke_v2_fetch_pack(
            gix_repo,
            &mut transport,
            &capabilities,
            &wanted_oids,
            local_have_oids,
            filter_spec,
            include_tags,
            warnings,
        )?,
        Protocol::V0 | Protocol::V1 => {
            if filter_spec.is_some() {
                warnings.push(
                    "remote downgraded to protocol v1; --filter partial clone was not negotiated on wire"
                        .to_string(),
                );
            }
            invoke_v1_fetch_pack(
                gix_repo,
                &mut transport,
                &capabilities,
                &wanted_oids,
                local_have_oids,
            )?;
            (true, false)
        }
    };

    if protocol == Protocol::V2 && transport.connection_persists_across_multiple_requests() {
        send_graceful_flush(&mut transport);
    }

    Ok((protocol, advertised_refs, pack_written, filter_negotiated))
}

fn fetch_specific_objects(
    gix_repo: &gix::Repository,
    gix_url: gix::url::Url,
    auth_token: Option<&str>,
    auth_scheme: HttpAuthScheme,
    wanted_oids: &[ObjectId],
    mark_promisor: bool,
) -> Result<()> {
    if wanted_oids.is_empty() {
        return Ok(());
    }
    let mut transport = connect_transport(
        gix_url,
        Protocol::V2,
        auth_token,
        auth_scheme,
        gix_repo,
    )?;
    let (protocol, capabilities) = {
        let mut hs = transport
            .handshake(Service::UploadPack, &[])
            .map_err(|err| map_transport_error("on-demand object fetch handshake", err))?;
        let protocol = hs.actual_protocol;
        let capabilities = hs.capabilities.clone();
        if matches!(protocol, Protocol::V0 | Protocol::V1) {
            let _ = parse_v1_handshake_refs(&mut hs)?;
        }
        (protocol, capabilities)
    };

    match protocol {
        Protocol::V2 => {
            let mut warnings = Vec::new();
            invoke_v2_fetch_pack_inner(
                gix_repo,
                &mut transport,
                &capabilities,
                wanted_oids,
                &[],
                None,
                false,
                mark_promisor,
                &mut warnings,
            )?;
            if transport.connection_persists_across_multiple_requests() {
                send_graceful_flush(&mut transport);
            }
        }
        Protocol::V0 | Protocol::V1 => {
            invoke_v1_fetch_pack(gix_repo, &mut transport, &capabilities, wanted_oids, &[])?;
        }
    }
    Ok(())
}

fn invoke_v2_ls_refs(
    transport: &mut Box<dyn Transport + Send>,
    capabilities: &Capabilities,
) -> Result<Vec<AdvertisedRef>> {
    let mut caps: Vec<(&str, Option<&str>)> = Vec::new();
    if capabilities.contains("agent") {
        caps.push(("agent", Some(USER_AGENT_VALUE)));
    }

    let mut args: Vec<BString> = vec!["peel".into(), "symrefs".into()];
    if capabilities
        .capability("ls-refs")
        .and_then(|c| c.supports("unborn"))
        .unwrap_or(false)
    {
        args.push("unborn".into());
    }

    let mut reader = transport
        .invoke("ls-refs", caps.into_iter(), Some(args.into_iter()), false)
        .map_err(|err| map_transport_error("protocol v2 ls-refs", err))?;

    let mut out = Vec::new();
    let mut line = String::new();
    while reader
        .readline_str(&mut line)
        .map_err(|err| OzError::Git(format!("failed reading v2 ls-refs response: {err}")))?
        != 0
    {
        if let Some(parsed) = parse_v2_ref_line(line.trim_end_matches(['\r', '\n']))? {
            out.push(parsed);
        }
        line.clear();
    }
    Ok(out)
}

fn parse_v2_ref_line(line: &str) -> Result<Option<AdvertisedRef>> {
    if line.is_empty() {
        return Ok(None);
    }
    let mut parts = line.split(' ');
    let first = parts.next().unwrap_or_default();
    let Some(ref_name) = parts.next() else {
        return Ok(None);
    };

    let mut symref_target: Option<String> = None;
    let mut peeled_oid: Option<ObjectId> = None;
    for attr in parts {
        if let Some(target) = attr.strip_prefix("symref-target:") {
            symref_target = Some(target.to_string());
        } else if let Some(peeled) = attr.strip_prefix("peeled:")
            && let Ok(id) = ObjectId::from_hex(peeled.as_bytes())
        {
            peeled_oid = Some(id);
        }
    }

    if first == "unborn" {
        return Ok(Some(AdvertisedRef::Unborn {
            name: ref_name.to_string(),
            target: symref_target.unwrap_or_else(|| "refs/heads/main".to_string()),
        }));
    }

    let oid = ObjectId::from_hex(first.as_bytes())
        .map_err(|err| OzError::Git(format!("invalid OID '{first}' in ls-refs line '{line}': {err}")))?;

    if let Some(target) = symref_target {
        Ok(Some(AdvertisedRef::Symbolic {
            name: ref_name.to_string(),
            target,
            oid,
        }))
    } else if let Some(p_id) = peeled_oid {
        Ok(Some(AdvertisedRef::Peeled {
            name: ref_name.to_string(),
            tag_oid: oid,
            peeled_oid: p_id,
        }))
    } else {
        Ok(Some(AdvertisedRef::Direct {
            name: ref_name.to_string(),
            oid,
        }))
    }
}

fn parse_v1_handshake_refs(
    hs: &mut gix_transport::client::blocking_io::SetServiceResponse<'_>,
) -> Result<Vec<AdvertisedRef>> {
    let mut symrefs: BTreeMap<String, String> = BTreeMap::new();
    for cap in hs.capabilities.iter() {
        if cap.name() == "symref"
            && let Some(val) = cap.value()
            && let val_str = val.to_str_lossy()
            && let Some((from_ref, to_ref)) = val_str.split_once(':')
        {
            symrefs.insert(from_ref.to_string(), to_ref.to_string());
        }
    }

    let mut out: Vec<AdvertisedRef> = Vec::new();
    if let Some(mut reader) = hs.refs.take() {
        let mut line = String::new();
        while reader
            .readline_str(&mut line)
            .map_err(|err| OzError::Git(format!("failed reading v1 handshake refs: {err}")))?
            != 0
        {
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if let Some((oid_str, rname)) = trimmed.split_once(' ') {
                if rname == "capabilities^{}" {
                    line.clear();
                    continue;
                }
                if let Some(tag_ref_name) = rname.strip_suffix("^{}") {
                    if let Ok(peeled_id) = ObjectId::from_hex(oid_str.as_bytes())
                        && let Some(last) = out.last_mut()
                        && last.name() == tag_ref_name
                        && let Some(tag_id) = last.oid()
                    {
                        *last = AdvertisedRef::Peeled {
                            name: tag_ref_name.to_string(),
                            tag_oid: tag_id,
                            peeled_oid: peeled_id,
                        };
                    }
                    line.clear();
                    continue;
                }
                if let Ok(oid) = ObjectId::from_hex(oid_str.as_bytes()) {
                    if let Some(target) = symrefs.remove(rname) {
                        out.push(AdvertisedRef::Symbolic {
                            name: rname.to_string(),
                            target,
                            oid,
                        });
                    } else {
                        out.push(AdvertisedRef::Direct {
                            name: rname.to_string(),
                            oid,
                        });
                    }
                }
            }
            line.clear();
        }
    }

    for (from_ref, to_ref) in symrefs {
        out.insert(
            0,
            AdvertisedRef::Unborn {
                name: from_ref,
                target: to_ref,
            },
        );
    }

    Ok(out)
}

#[expect(clippy::too_many_arguments)]
fn invoke_v2_fetch_pack(
    gix_repo: &gix::Repository,
    transport: &mut Box<dyn Transport + Send>,
    capabilities: &Capabilities,
    wanted_oids: &[ObjectId],
    local_have_oids: &[ObjectId],
    filter_spec: Option<&str>,
    include_tags: bool,
    warnings: &mut Vec<String>,
) -> Result<(bool, bool)> {
    invoke_v2_fetch_pack_inner(
        gix_repo,
        transport,
        capabilities,
        wanted_oids,
        local_have_oids,
        filter_spec,
        include_tags,
        filter_spec.is_some(),
        warnings,
    )
}

#[expect(clippy::too_many_arguments)]
fn invoke_v2_fetch_pack_inner(
    gix_repo: &gix::Repository,
    transport: &mut Box<dyn Transport + Send>,
    capabilities: &Capabilities,
    wanted_oids: &[ObjectId],
    local_have_oids: &[ObjectId],
    filter_spec: Option<&str>,
    include_tags: bool,
    mark_promisor: bool,
    warnings: &mut Vec<String>,
) -> Result<(bool, bool)> {
    let mut caps: Vec<(&str, Option<&str>)> = Vec::new();
    if capabilities.contains("agent") {
        caps.push(("agent", Some(USER_AGENT_VALUE)));
    }

    let server_supports_filter = capabilities
        .capability("fetch")
        .and_then(|c| c.supports("filter"))
        .unwrap_or(false);

    let mut args: Vec<BString> = vec![
        "thin-pack".into(),
        "ofs-delta".into(),
        "no-progress".into(),
    ];
    if include_tags {
        args.push("include-tag".into());
    }

    let mut filter_negotiated = false;
    if let Some(spec) = filter_spec {
        if server_supports_filter {
            args.push(format!("filter {spec}").into());
            filter_negotiated = true;
        } else {
            warnings.push(format!(
                "remote server does not advertise 'filter' in protocol v2 fetch; '{spec}' saved in promisor config without wire filtering"
            ));
        }
    }

    for want in wanted_oids {
        args.push(format!("want {}", want.to_hex()).into());
    }
    for have in local_have_oids {
        args.push(format!("have {}", have.to_hex()).into());
    }
    args.push("done".into());

    let mut reader = transport
        .invoke("fetch", caps.into_iter(), Some(args.into_iter()), false)
        .map_err(|err| map_transport_error("protocol v2 fetch", err))?;

    reader.reset(Protocol::V2);
    let mut line = String::new();
    let mut saw_packfile = false;
    loop {
        line.clear();
        let n = reader
            .readline_str(&mut line)
            .map_err(|err| OzError::Git(format!("failed reading v2 fetch section header: {err}")))?;
        if n == 0 {
            break;
        }
        let header = line.trim_end_matches(['\r', '\n']).to_string();
        if header == "packfile" {
            saw_packfile = true;
            break;
        }
        // Consume non-packfile section (`acknowledgments`, `shallow-info`, `wanted-refs`).
        loop {
            line.clear();
            let item_len = reader
                .readline_str(&mut line)
                .map_err(|err| OzError::Git(format!("failed reading v2 fetch section '{header}': {err}")))?;
            if item_len == 0 {
                break;
            }
        }
        if reader.stopped_at() == Some(MessageKind::Delimiter) {
            reader.reset(Protocol::V2);
        } else {
            break;
        }
    }

    if !saw_packfile {
        return Ok((false, filter_negotiated));
    }

    let remote_err_msgs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let write_res = {
        let errs_ref = Arc::clone(&remote_err_msgs);
        reader.set_progress_handler(Some(Box::new(move |is_err, data| {
            if is_err {
                errs_ref
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(String::from_utf8_lossy(data).trim().to_string());
            }
            ControlFlow::Continue(())
        })));
        write_received_pack_to_odb(gix_repo, &mut reader, mark_promisor)
    };
    write_res.map_err(|err| {
        let errs = remote_err_msgs.lock().unwrap_or_else(|p| p.into_inner());
        if !errs.is_empty() {
            OzError::Git(format!(
                "remote fetch error: {} ({err})",
                errs.join("; ")
            ))
        } else {
            err
        }
    })?;

    if reader.stopped_at().is_none() {
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    }

    Ok((true, filter_negotiated))
}

fn invoke_v1_fetch_pack(
    gix_repo: &gix::Repository,
    transport: &mut Box<dyn Transport + Send>,
    capabilities: &Capabilities,
    wanted_oids: &[ObjectId],
    local_have_oids: &[ObjectId],
) -> Result<()> {
    let use_sideband = capabilities.contains("side-band-64k");
    let mut v1_caps = vec!["ofs-delta", "thin-pack", "no-progress"];
    if use_sideband {
        v1_caps.push("side-band-64k");
    }

    let mut req = transport
        .request(
            WriteMode::OneLfTerminatedLinePerWriteCall,
            MessageKind::Text(b"done"),
            false,
        )
        .map_err(|err| map_transport_error("protocol v1 fetch request", err))?;

    for (idx, want) in wanted_oids.iter().enumerate() {
        let line = if idx == 0 {
            format!("want {} {}", want.to_hex(), v1_caps.join(" "))
        } else {
            format!("want {}", want.to_hex())
        };
        req.write_all(line.as_bytes())
            .map_err(|err| OzError::Git(format!("failed sending v1 want: {err}")))?;
    }
    req.write_message(MessageKind::Flush)
        .map_err(|err| OzError::Git(format!("failed sending v1 flush: {err}")))?;
    for have in local_have_oids {
        let line = format!("have {}", have.to_hex());
        req.write_all(line.as_bytes())
            .map_err(|err| OzError::Git(format!("failed sending v1 have: {err}")))?;
    }

    let mut reader = req
        .into_read()
        .map_err(|err| OzError::Git(format!("failed reading v1 fetch reply: {err}")))?;

    // Drain ACK / NAK lines before pack stream.
    let mut line = String::new();
    loop {
        line.clear();
        let (is_ack_or_nak, is_nak) = {
            let Some(peek) = reader.peek_data_line() else {
                break;
            };
            let peek_bytes = peek
                .map_err(OzError::Io)?
                .map_err(|err| map_transport_error("v1 fetch response", err))?;
            let text = String::from_utf8_lossy(peek_bytes);
            let is_nak = text.starts_with("NAK");
            (is_nak || text.starts_with("ACK "), is_nak)
        };
        if is_ack_or_nak {
            let _ = reader.readline_str(&mut line).map_err(OzError::Io)?;
            if is_nak {
                break;
            }
        } else {
            break;
        }
    }

    if use_sideband {
        reader.set_progress_handler(Some(Box::new(|_is_err, _data| ControlFlow::Continue(()))));
    }
    write_received_pack_to_odb(gix_repo, &mut reader, false)?;
    let _ = std::io::copy(&mut reader, &mut std::io::sink());
    Ok(())
}

fn write_received_pack_to_odb(
    gix_repo: &gix::Repository,
    reader: &mut dyn std::io::BufRead,
    mark_promisor: bool,
) -> Result<()> {
    let pack_dir = gix_repo.objects.store_ref().path().join("pack");
    std::fs::create_dir_all(&pack_dir)?;

    let should_interrupt = AtomicBool::new(false);
    let mut progress = gix::progress::Discard;
    let write_opts = gix_pack::bundle::write::Options {
        thread_limit: Some(1),
        index_version: gix_pack::index::Version::V2,
        iteration_mode: gix_pack::data::input::Mode::Verify,
        ..Default::default()
    };

    let mut outcome = gix_pack::Bundle::write_to_directory(
        reader,
        Some(&pack_dir),
        &mut progress,
        &should_interrupt,
        Some(Box::new(gix_repo.objects.clone())),
        gix_repo.object_hash(),
        write_opts,
    )
    .map_err(|err| OzError::Git(format!("failed writing received packfile to ODB: {err}")))?;

    if mark_promisor && let Some(ref data_path) = outcome.data_path {
        let promisor_path = data_path.with_extension("promisor");
        let _ = std::fs::write(promisor_path, b"");
    }

    if let Some(keep_path) = outcome.keep_path.take() {
        let _ = std::fs::remove_file(keep_path);
    }

    Ok(())
}

fn send_graceful_flush(transport: &mut Box<dyn Transport + Send>) {
    if let Ok(req) = transport.request(WriteMode::Binary, MessageKind::Flush, false) {
        let _ = req.into_read();
    }
}

fn connect_transport(
    gix_url: gix::url::Url,
    desired_version: Protocol,
    auth_token: Option<&str>,
    auth_scheme: HttpAuthScheme,
    gix_repo: &gix::Repository,
) -> Result<Box<dyn Transport + Send>> {
    let is_http = matches!(
        gix_url.scheme,
        gix::url::Scheme::Http | gix::url::Scheme::Https
    );
    let mut transport = connect::connect(
        gix_url,
        connect::Options {
            version: desired_version,
            ssh: Default::default(),
            trace: false,
        },
    )
    .map_err(|err| OzError::Git(format!("failed connecting to remote: {err}")))?;

    if is_http {
        let _ = gix_repo;
        let mut http_opts = gix_transport::client::blocking_io::http::Options {
            user_agent: Some(format!("git/{USER_AGENT_VALUE}")),
            ..Default::default()
        };

        if let Some(raw_tok) = auth_token.map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(bearer_tok) = raw_tok.strip_prefix("Bearer ") {
                http_opts
                    .extra_headers
                    .push(format!("Authorization: Bearer {}", bearer_tok.trim()));
            } else if auth_scheme == HttpAuthScheme::Bearer {
                http_opts
                    .extra_headers
                    .push(format!("Authorization: Bearer {raw_tok}"));
            } else {
                transport
                    .set_identity(gix::sec::identity::Account {
                        username: "x-access-token".to_string(),
                        password: raw_tok.to_string(),
                        oauth_refresh_token: None,
                    })
                    .map_err(|err| map_transport_error("setting HTTP basic credentials", err))?;
            }
        }

        transport
            .configure(&http_opts)
            .map_err(|err| OzError::Git(format!("failed configuring HTTP transport: {err}")))?;
    }

    Ok(transport)
}

fn resolve_transport_url_and_auth(
    raw_url: &str,
    base_dir: &Path,
    explicit_token: Option<&str>,
    default_scheme: HttpAuthScheme,
) -> Result<(gix::url::Url, String, Option<String>, HttpAuthScheme)> {
    let parsed = parse_git_remote(raw_url, None)?;

    if parsed.scheme == RemoteScheme::File {
        let local_path_str = raw_url
            .trim()
            .strip_prefix("file://")
            .unwrap_or_else(|| raw_url.trim());
        let p = Path::new(local_path_str);
        let abs = if p.is_absolute() {
            normalize_lexical(p)
        } else {
            normalize_lexical(&base_dir.join(p))
        };
        let git_dir = if abs.join(".git").is_dir() {
            abs.join(".git")
        } else {
            abs.clone()
        };
        if !git_dir.exists() {
            return Err(OzError::InvalidUsage {
                message: format!("local remote repository '{}' does not exist", abs.display()),
                recovery: Vec::new(),
            });
        }
        let path_bstr = gix::path::into_bstr(git_dir.clone())
            .map_err(|err| OzError::Git(err.to_string()))?;
        let gix_url = gix::url::Url::from_parts(
            gix::url::Scheme::File,
            None,
            None,
            None,
            None,
            path_bstr.into_owned(),
            false,
        )
        .map_err(|err| OzError::Git(format!("invalid file URL for {}: {err}", git_dir.display())))?;

        let persisted = if raw_url.trim().starts_with("file://") {
            raw_url.trim().to_string()
        } else {
            abs.to_string_lossy().into_owned()
        };
        return Ok((gix_url, persisted, None, default_scheme));
    }

    let gix_url = gix::url::parse(parsed.sanitized_url.as_str())
        .map_err(|err| OzError::InvalidUsage {
            message: format!("invalid remote URL '{}': {err}", parsed.sanitized_url),
            recovery: Vec::new(),
        })?;

    let inline_password = url::Url::parse(&parsed.raw_url)
        .ok()
        .and_then(|u| u.password().map(str::to_string));

    let token = if let Some(tok) = explicit_token.map(str::trim).filter(|s| !s.is_empty()) {
        Some(tok.to_string())
    } else if let Some(inline) = inline_password {
        Some(inline)
    } else if parsed.is_http() {
        let protocol = match parsed.scheme {
            RemoteScheme::Http => "http",
            _ => "https",
        };
        discover_github_token_with_options(&TokenDiscoveryOptions {
            host: parsed
                .host
                .clone()
                .unwrap_or_else(|| "github.com".to_string()),
            protocol: protocol.to_string(),
            credential_workdir: Some(base_dir.to_path_buf()),
            ..Default::default()
        })
        .map(|d| d.token)
    } else {
        None
    };

    Ok((gix_url, parsed.sanitized_url, token, default_scheme))
}

fn select_clone_target(
    adv_refs: &[AdvertisedRef],
    requested_branch: Option<&str>,
) -> Result<(Option<String>, Option<ObjectId>, Option<String>)> {
    let default_remote_branch = adv_refs
        .iter()
        .find(|r| r.name() == "HEAD")
        .and_then(|r| {
            r.symref_target()
                .and_then(|t| t.strip_prefix("refs/heads/").map(str::to_string))
                .or_else(|| {
                    let head_oid = r.oid()?;
                    adv_refs.iter().find_map(|cand| {
                        let br = cand.name().strip_prefix("refs/heads/")?;
                        (cand.oid() == Some(head_oid)).then(|| br.to_string())
                    })
                })
        });

    if let Some(req) = requested_branch.map(str::trim).filter(|s| !s.is_empty()) {
        let short = req
            .strip_prefix("refs/heads/")
            .or_else(|| req.strip_prefix("refs/tags/"))
            .unwrap_or(req);
        let branch_full = format!("refs/heads/{short}");
        if let Some(found) = adv_refs.iter().find(|r| r.name() == branch_full) {
            return Ok((
                Some(short.to_string()),
                found.oid(),
                default_remote_branch.or_else(|| Some(short.to_string())),
            ));
        }
        let tag_full = format!("refs/tags/{short}");
        if let Some(found) = adv_refs.iter().find(|r| r.name() == tag_full) {
            let oid = match found {
                AdvertisedRef::Peeled { peeled_oid, .. } => Some(*peeled_oid),
                other => other.oid(),
            };
            return Ok((None, oid, default_remote_branch));
        }
        return Err(OzError::InvalidUsage {
            message: format!("remote branch or tag '{req}' was not found on origin"),
            recovery: vec!["Omit `--branch` to clone the remote default branch.".to_string()],
        });
    }

    if let Some(ref def_br) = default_remote_branch {
        let full = format!("refs/heads/{def_br}");
        let oid = adv_refs
            .iter()
            .find(|r| r.name() == full)
            .and_then(AdvertisedRef::oid)
            .or_else(|| {
                adv_refs
                    .iter()
                    .find(|r| r.name() == "HEAD")
                    .and_then(AdvertisedRef::oid)
            });
        return Ok((Some(def_br.clone()), oid, Some(def_br.clone())));
    }

    if let Some(first_branch) = adv_refs
        .iter()
        .find_map(|r| r.name().strip_prefix("refs/heads/").map(|b| (b.to_string(), r.oid())))
    {
        return Ok((
            Some(first_branch.0.clone()),
            first_branch.1,
            Some(first_branch.0),
        ));
    }

    // Empty remote repository -> unborn `main`.
    Ok((Some("main".to_string()), None, Some("main".to_string())))
}

fn write_clone_git_config(
    git_dir: &Path,
    remote_name: &str,
    remote_url: &str,
    bare: bool,
    filter_spec: Option<&str>,
    branch: Option<&str>,
) -> Result<()> {
    let config_path = git_dir.join("config");
    let mut existing = if config_path.is_file() {
        std::fs::read_to_string(&config_path)?
    } else {
        String::new()
    };

    if filter_spec.is_some() {
        existing = replace_repository_format_version(&existing, 1);
    }

    if !existing.ends_with('\n') && !existing.is_empty() {
        existing.push('\n');
    }

    existing.push_str(&format!("[remote \"{remote_name}\"]\n"));
    existing.push_str(&format!("\turl = {remote_url}\n"));
    if !bare {
        existing.push_str(&format!(
            "\tfetch = +refs/heads/*:refs/remotes/{remote_name}/*\n"
        ));
    }
    if let Some(spec) = filter_spec {
        existing.push_str("\tpromisor = true\n");
        existing.push_str(&format!("\tpartialclonefilter = {spec}\n"));
    }

    if !bare && let Some(br) = branch {
        existing.push_str(&format!("[branch \"{br}\"]\n"));
        existing.push_str(&format!("\tremote = {remote_name}\n"));
        existing.push_str(&format!("\tmerge = refs/heads/{br}\n"));
    }

    std::fs::write(&config_path, existing)?;
    Ok(())
}

fn replace_repository_format_version(config: &str, version: u8) -> String {
    let mut out = String::with_capacity(config.len() + 32);
    let mut in_core = false;
    let mut replaced = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_core = trimmed.eq_ignore_ascii_case("[core]");
        }
        if in_core
            && let Some((k, _)) = trimmed.split_once('=')
            && k.trim().eq_ignore_ascii_case("repositoryformatversion")
        {
            out.push_str(&format!("\trepositoryformatversion = {version}\n"));
            replaced = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !replaced {
        out.push_str(&format!("[core]\n\trepositoryformatversion = {version}\n"));
    }
    out
}

fn write_branch_upstream_config(
    git_common_dir: &Path,
    local_branch: &str,
    remote_name: &str,
    merge_ref: &str,
) -> Result<()> {
    let config_path = git_common_dir.join("config");
    let mut existing = if config_path.is_file() {
        std::fs::read_to_string(&config_path)?
    } else {
        String::new()
    };
    let section_header = format!("[branch \"{local_branch}\"]");
    if existing.contains(&section_header) {
        // Rewrite existing branch section cleanly so repeated `-u` pushes stay idempotent.
        let mut rewritten = String::with_capacity(existing.len() + 64);
        let mut inside_target_branch = false;
        for line in existing.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                if inside_target_branch {
                    inside_target_branch = false;
                }
                if trimmed == section_header {
                    inside_target_branch = true;
                    rewritten.push_str(line);
                    rewritten.push('\n');
                    rewritten.push_str(&format!("\tremote = {remote_name}\n"));
                    rewritten.push_str(&format!("\tmerge = {merge_ref}\n"));
                    continue;
                }
            }
            if inside_target_branch
                && let Some((k, _)) = trimmed.split_once('=')
                && matches!(k.trim(), "remote" | "merge")
            {
                continue;
            }
            rewritten.push_str(line);
            rewritten.push('\n');
        }
        existing = rewritten;
    } else {
        if !existing.ends_with('\n') && !existing.is_empty() {
            existing.push('\n');
        }
        existing.push_str(&format!(
            "{section_header}\n\tremote = {remote_name}\n\tmerge = {merge_ref}\n"
        ));
    }
    std::fs::write(&config_path, existing)?;
    Ok(())
}

fn collect_missing_blobs_in_tree(
    gix_repo: &gix::Repository,
    tree_id: ObjectId,
) -> Result<Vec<ObjectId>> {
    let tree = gix_repo
        .find_tree(tree_id)
        .map_err(|err| OzError::Git(err.to_string()))?;
    let entries = tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|err| OzError::Git(err.to_string()))?;

    let mut missing = BTreeSet::new();
    let mut buf = Vec::new();
    for entry in entries {
        if entry.mode.is_tree() || entry.mode.is_commit() {
            continue;
        }
        let exists = gix_repo
            .objects
            .try_find(&entry.oid, &mut buf)
            .map_err(|err| OzError::Git(err.to_string()))?
            .is_some();
        if !exists {
            missing.insert(entry.oid);
        }
    }
    Ok(missing.into_iter().collect())
}

fn write_index_from_tree(gix_repo: &gix::Repository, tree_id: ObjectId) -> Result<usize> {
    let index_path = gix_repo.index_path();
    let index_file = gix_repo
        .index_from_tree(&tree_id)
        .map_err(|err| OzError::Git(format!("failed creating index from tree {tree_id}: {err}")))?;
    let count = index_file.entries().len();
    let mut out = BufWriter::new(std::fs::File::create(&index_path)?);
    index_file
        .write_to(&mut out, gix::index::write::Options::default())
        .map_err(|err| OzError::Git(format!("failed writing .git/index: {err}")))?;
    out.flush()?;
    Ok(count)
}

fn resolve_push_specs(
    repo: &OzoneRepo,
    gix_repo: &gix::Repository,
    opts: &PushOptions,
) -> Result<Vec<ResolvedPushSpec>> {
    let null_oid = ObjectId::null(gix_repo.object_hash());

    if opts.refspecs.is_empty() {
        let Some(ref branch) = repo.branch else {
            return Ok(Vec::new());
        };
        let local_ref = format!("refs/heads/{branch}");
        if opts.delete {
            return Ok(vec![ResolvedPushSpec {
                local_ref: None,
                local_branch_short: Some(branch.clone()),
                remote_ref: local_ref,
                new_oid: null_oid,
                force_prefix: false,
                is_delete: true,
            }]);
        }
        let oid = read_ref_oid(gix_repo, &local_ref)?.ok_or_else(|| OzError::InvalidUsage {
            message: format!("current branch '{branch}' has no commits yet"),
            recovery: vec!["Create an initial commit with `oz commit -m \"...\"` first.".to_string()],
        })?;
        return Ok(vec![ResolvedPushSpec {
            local_ref: Some(local_ref.clone()),
            local_branch_short: Some(branch.clone()),
            remote_ref: local_ref,
            new_oid: oid,
            force_prefix: false,
            is_delete: false,
        }]);
    }

    let mut out = Vec::with_capacity(opts.refspecs.len());
    for raw in &opts.refspecs {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (force_prefix, body) = match trimmed.strip_prefix('+') {
            Some(rest) => (true, rest),
            None => (false, trimmed),
        };

        if opts.delete && !body.contains(':') {
            let remote_ref = normalize_dest_ref(body);
            out.push(ResolvedPushSpec {
                local_ref: None,
                local_branch_short: remote_ref.strip_prefix("refs/heads/").map(str::to_string),
                remote_ref,
                new_oid: null_oid,
                force_prefix,
                is_delete: true,
            });
            continue;
        }

        if let Some((src_raw, dst_raw)) = body.split_once(':') {
            let src = src_raw.trim();
            let dst = dst_raw.trim();
            if dst.is_empty() {
                return Err(OzError::InvalidUsage {
                    message: format!("push refspec '{raw}' has an empty destination"),
                    recovery: Vec::new(),
                });
            }
            let remote_ref = normalize_dest_ref(dst);
            if src.is_empty() {
                out.push(ResolvedPushSpec {
                    local_ref: None,
                    local_branch_short: None,
                    remote_ref,
                    new_oid: null_oid,
                    force_prefix,
                    is_delete: true,
                });
            } else {
                let (local_ref, local_short, oid) = resolve_local_push_source(gix_repo, src)?;
                out.push(ResolvedPushSpec {
                    local_ref,
                    local_branch_short: local_short,
                    remote_ref,
                    new_oid: oid,
                    force_prefix,
                    is_delete: false,
                });
            }
        } else {
            let (local_ref, local_short, oid) = resolve_local_push_source(gix_repo, body)?;
            let remote_ref = local_ref
                .clone()
                .unwrap_or_else(|| normalize_dest_ref(body));
            out.push(ResolvedPushSpec {
                local_ref,
                local_branch_short: local_short,
                remote_ref,
                new_oid: oid,
                force_prefix,
                is_delete: false,
            });
        }
    }

    Ok(out)
}

fn resolve_local_push_source(
    gix_repo: &gix::Repository,
    src: &str,
) -> Result<(Option<String>, Option<String>, ObjectId)> {
    if src == "HEAD" {
        let head = gix_repo
            .head()
            .map_err(|err| OzError::Git(err.to_string()))?;
        let branch_short = head
            .referent_name()
            .map(|n| String::from_utf8_lossy(n.shorten().as_ref()).into_owned());
        let local_ref = branch_short.as_ref().map(|b| format!("refs/heads/{b}"));
        let oid = head
            .into_peeled_id()
            .map_err(|err| OzError::Git(format!("HEAD does not point to a valid commit: {err}")))?
            .detach();
        return Ok((local_ref, branch_short, oid));
    }

    let candidate_branch = if src.starts_with("refs/") {
        src.to_string()
    } else {
        format!("refs/heads/{src}")
    };
    if let Some(oid) = read_ref_oid(gix_repo, &candidate_branch)? {
        let short = candidate_branch
            .strip_prefix("refs/heads/")
            .map(str::to_string);
        return Ok((Some(candidate_branch), short, oid));
    }

    let candidate_tag = format!("refs/tags/{src}");
    if !src.starts_with("refs/")
        && let Some(oid) = read_ref_oid(gix_repo, &candidate_tag)?
    {
        return Ok((Some(candidate_tag), None, oid));
    }

    let parsed = gix_repo
        .rev_parse_single(src)
        .map_err(|err| OzError::InvalidUsage {
            message: format!("local ref or revision '{src}' does not exist: {err}"),
            recovery: vec!["Check available branches with `oz status` or `oz stack status`.".to_string()],
        })?
        .detach();
    Ok((None, None, parsed))
}

fn normalize_dest_ref(dst: &str) -> String {
    if dst.starts_with("refs/") {
        dst.to_string()
    } else {
        format!("refs/heads/{dst}")
    }
}

fn validate_filter_spec(spec: &str) -> Result<()> {
    let trimmed = spec.trim();
    if trimmed.is_empty()
        || !(trimmed == "blob:none"
            || trimmed.starts_with("blob:limit=")
            || trimmed.starts_with("tree:"))
    {
        return Err(OzError::InvalidUsage {
            message: format!(
                "unsupported partial clone filter '{spec}' (expected `blob:none`, `blob:limit=<n>`, or `tree:<depth>`)"
            ),
            recovery: vec!["Pass `--filter=blob:none`.".to_string()],
        });
    }
    Ok(())
}

pub(crate) fn open_gix_unrestricted(path: &Path) -> Result<gix::Repository> {
    let mut repo = gix::open(path)
        .or_else(|_| {
            let mut opts = gix::open::Options::isolated();
            opts.permissions.config.git_binary = true;
            gix::ThreadSafeRepository::discover_with_environment_overrides_opts(
                path,
                Default::default(),
                gix::sec::trust::Mapping {
                    full: opts.clone(),
                    reduced: opts,
                },
            )
            .map(|r| r.to_thread_local())
        })
        .map_err(|err| OzError::Git(err.to_string()))?;
    let _ = repo.committer_or_set_generic_fallback();
    Ok(repo)
}

fn collect_local_have_oids(gix_repo: &gix::Repository) -> Result<Vec<ObjectId>> {
    let mut seen = BTreeSet::new();
    let platform = gix_repo
        .references()
        .map_err(|err| OzError::Git(err.to_string()))?;
    let all_refs = platform
        .all()
        .map_err(|err| OzError::Git(err.to_string()))?;
    for item in all_refs.flatten() {
        let mut r = item;
        if matches!(r.target(), gix::refs::TargetRef::Symbolic(_)) {
            continue;
        }
        if let Ok(id) = r.peel_to_id() {
            let oid = id.detach();
            if has_commit_locally(gix_repo, oid) {
                seen.insert(oid);
            }
        }
    }
    Ok(seen.into_iter().take(128).collect())
}

fn has_commit_locally(gix_repo: &gix::Repository, oid: ObjectId) -> bool {
    let mut buf = Vec::new();
    gix_repo
        .objects
        .try_find(&oid, &mut buf)
        .ok()
        .flatten()
        .is_some()
}

pub(crate) fn read_ref_oid(gix_repo: &gix::Repository, full_ref: &str) -> Result<Option<ObjectId>> {
    let Some(mut r) = gix_repo
        .try_find_reference(full_ref)
        .map_err(|err| OzError::Git(err.to_string()))?
    else {
        return Ok(None);
    };
    if let gix::refs::TargetRef::Object(id) = r.target() {
        return Ok(Some(id.to_owned()));
    }
    Ok(r.peel_to_id().ok().map(|id| id.detach()))
}

pub(crate) fn is_ancestor_commit(
    gix_repo: &gix::Repository,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    if !has_commit_locally(gix_repo, ancestor) || !has_commit_locally(gix_repo, descendant) {
        return Ok(false);
    }
    match gix_repo.merge_base(ancestor, descendant) {
        Ok(Some(mb)) => Ok(mb.detach() == ancestor),
        Ok(None) | Err(_) => Ok(false),
    }
}

fn set_symbolic_ref(
    gix_repo: &gix::Repository,
    ref_name: &str,
    target_ref: &str,
    log_msg: &str,
) -> Result<()> {
    let name = FullName::try_from(ref_name)
        .map_err(|err| OzError::Git(format!("invalid ref name '{ref_name}': {err}")))?;
    let target = FullName::try_from(target_ref)
        .map_err(|err| OzError::Git(format!("invalid target ref '{target_ref}': {err}")))?;
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: log_msg.into(),
            },
            expected: PreviousValue::Any,
            new: Target::Symbolic(target),
        },
        name,
        deref: false,
    };
    gix_repo
        .edit_reference(edit)
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(())
}

fn set_detached_head(gix_repo: &gix::Repository, oid: ObjectId, log_msg: &str) -> Result<()> {
    let head_name =
        FullName::try_from("HEAD").map_err(|err| OzError::Git(format!("invalid HEAD: {err}")))?;
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: log_msg.into(),
            },
            expected: PreviousValue::Any,
            new: Target::Object(oid),
        },
        name: head_name,
        deref: false,
    };
    gix_repo
        .edit_reference(edit)
        .map_err(|err| OzError::Git(err.to_string()))?;
    Ok(())
}

fn looks_like_url_or_path(s: &str) -> bool {
    s.contains("://")
        || s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || s.contains('@')
        || Path::new(s).exists()
}

const fn protocol_number(p: Protocol) -> u8 {
    match p {
        Protocol::V0 => 0,
        Protocol::V1 => 1,
        Protocol::V2 => 2,
    }
}

fn map_transport_error(stage: &str, err: gix_transport::client::Error) -> OzError {
    let msg = err.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("401")
        || lower.contains("403")
        || lower.contains("authentication")
        || lower.contains("unauthorized")
    {
        OzError::CheckFailed {
            message: format!("authentication failed during {stage}: {msg}"),
            recovery: vec![
                "export GITHUB_TOKEN=<personal-access-token>".to_string(),
                "gh auth login".to_string(),
            ],
        }
    } else {
        OzError::Git(format!("{stage} failed: {msg}"))
    }
}
