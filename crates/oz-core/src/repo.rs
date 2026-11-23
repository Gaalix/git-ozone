// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Colocated `.git/` and `.git/ozone/` repository discovery, including linked git
//! worktrees, `git-ozone` alias resolution, and `ENOTCONN`-safe `../<repo>.oz/<ws>` lookup.

use crate::config::{OzoneConfig, normalize_lexical};
use crate::error::{OzError, Result};
use crate::types::{MountRecord, WorkspaceState};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// How the Ozone CLI binary was invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CliInvocation {
    /// Invoked directly as `oz` (or `oz.exe`).
    Oz,
    /// Invoked via Git subcommand alias `git-ozone` / `git ozone`.
    GitOzone,
}

impl CliInvocation {
    /// Detects invocation mode from `argv[0]`.
    #[must_use]
    pub fn from_arg0(arg0: Option<&OsStr>) -> Self {
        let Some(raw) = arg0 else {
            return Self::Oz;
        };
        let lossy = raw.to_string_lossy();
        let file_name = lossy.rsplit(['/', '\\']).next().unwrap_or_default();
        let lower = file_name.to_ascii_lowercase();
        let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
        if stem == "git-ozone" {
            Self::GitOzone
        } else {
            Self::Oz
        }
    }

    /// Canonical binary name for display output.
    #[must_use]
    pub const fn binary_name(self) -> &'static str {
        match self {
            Self::Oz => "oz",
            Self::GitOzone => "git-ozone",
        }
    }
}

/// Searches `PATH` for the `git-ozone` binary alias used by `git ozone ...`.
#[must_use]
pub fn find_git_ozone_on_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    find_binary_in_path("git-ozone", &path_var)
}

pub(crate) fn find_binary_in_path(name: &str, path_var: &OsStr) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidate_names = [format!("{name}.exe"), name.to_string()];
    #[cfg(not(windows))]
    let candidate_names = [name.to_string()];

    for dir in std::env::split_paths(path_var) {
        for cand in &candidate_names {
            let full = dir.join(cand);
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

/// Colocated `.git/ozone/` directory layout inside a repository's shared `.git/` dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OzoneLayout {
    pub git_common_dir: PathBuf,
    pub ozone_dir: PathBuf,
}

impl OzoneLayout {
    /// Creates a layout rooted at `<git_common_dir>/ozone`.
    #[must_use]
    pub fn from_git_common_dir(git_common_dir: impl Into<PathBuf>) -> Self {
        let git_common_dir = git_common_dir.into();
        let ozone_dir = git_common_dir.join("ozone");
        Self {
            git_common_dir,
            ozone_dir,
        }
    }

    /// `.git/ozone/config.toml` (repo-local settings).
    #[must_use]
    pub fn local_config_path(&self) -> PathBuf {
        self.ozone_dir.join("config.toml")
    }

    /// `.git/ozone/oplog/` (append-only MVCC operation log).
    #[must_use]
    pub fn oplog_dir(&self) -> PathBuf {
        self.ozone_dir.join("oplog")
    }

    /// `.git/ozone/state/` (per-workspace indexless tree-delta state files).
    #[must_use]
    pub fn state_dir(&self) -> PathBuf {
        self.ozone_dir.join("state")
    }

    /// `.git/ozone/state/<ws>.json`.
    #[must_use]
    pub fn workspace_state_path(&self, ws: &str) -> PathBuf {
        self.state_dir().join(format!("{ws}.json"))
    }

    /// `.git/ozone/pinned_main.json` (virtual pinned set for the main repository).
    ///
    /// Stored outside `.git/ozone/state/` because `oz_cow::list_workspaces` parses
    /// every `*.json` file inside `.git/ozone/state/` as a `WorkspaceState`.
    #[must_use]
    pub fn main_pinned_path(&self) -> PathBuf {
        self.ozone_dir.join("pinned_main.json")
    }

    /// `.git/ozone/baselines/` (quiescent baseline trees & warm caches).
    #[must_use]
    pub fn baselines_dir(&self) -> PathBuf {
        self.ozone_dir.join("baselines")
    }

    /// `.git/ozone/ws/` (persistent VFS workspace upperdirs & mount metadata).
    #[must_use]
    pub fn ws_dir(&self) -> PathBuf {
        self.ozone_dir.join("ws")
    }

    /// `.git/ozone/ws/<ws>/`.
    #[must_use]
    pub fn ws_meta_dir(&self, ws: &str) -> PathBuf {
        self.ws_dir().join(ws)
    }

    /// `.git/ozone/ws/<ws>/mount.json`.
    #[must_use]
    pub fn ws_mount_record_path(&self, ws: &str) -> PathBuf {
        self.ws_meta_dir(ws).join("mount.json")
    }

    /// `.git/ozone/ws/<ws>/upper/`.
    #[must_use]
    pub fn ws_upper_dir(&self, ws: &str) -> PathBuf {
        self.ws_meta_dir(ws).join("upper")
    }

    /// `.git/ozone/ws/<ws>/meta/` (deletion whiteouts/tombstones).
    #[must_use]
    pub fn ws_tombstone_dir(&self, ws: &str) -> PathBuf {
        self.ws_meta_dir(ws).join("meta")
    }

    /// `.git/ozone/vfsd.sock` (local IPC socket for `oz-vfsd`), automatically falling back
    /// on Unix to a short hashed runtime path (`$XDG_RUNTIME_DIR/ozone/v-<hash>.sock` or
    /// `/tmp/oz-<uid>/v-<hash>.sock`) when the colocated path would exceed `sockaddr_un::sun_path`
    /// (`SUN_LEN` ~104-108 bytes).
    #[must_use]
    pub fn vfsd_socket_path(&self) -> PathBuf {
        let colocated = self.ozone_dir.join("vfsd.sock");
        #[cfg(unix)]
        {
            // Keep safely under macOS (104) and Linux (108) `sun_path` limits including NUL.
            const MAX_UNIX_SOCK_BYTES: usize = 100;
            if colocated.as_os_str().as_encoded_bytes().len() > MAX_UNIX_SOCK_BYTES {
                return short_unix_socket_path(&self.git_common_dir);
            }
        }
        colocated
    }

    /// Returns `true` if `.git/ozone/` already exists on disk.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.ozone_dir.is_dir()
    }

    /// Creates `.git/ozone/{oplog,state,baselines,ws}` if missing.
    pub fn ensure_initialized(&self) -> Result<()> {
        for dir in [
            &self.ozone_dir,
            &self.oplog_dir(),
            &self.state_dir(),
            &self.baselines_dir(),
            &self.ws_dir(),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

/// Reachability state of a workspace mountpoint (`../<repo>.oz/<ws>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MountHealth {
    /// Not a VFS mountpoint (main repository or standard filesystem path).
    NotApplicable,
    /// Mountpoint is present and responds to `lstat`.
    Healthy,
    /// `mount.json` exists, but the mountpoint directory is not currently mounted/present.
    NotMounted,
    /// Mountpoint returned `ENOTCONN` (`errno == 107` on Linux), indicating a dead FUSE daemon.
    DisconnectedEnotconn,
}

impl MountHealth {
    /// Stable string name for diagnostics and JSON output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::Healthy => "healthy",
            Self::NotMounted => "not_mounted",
            Self::DisconnectedEnotconn => "disconnected_enotconn",
        }
    }
}

/// Discovered colocated Git + Ozone repository context.
#[derive(Debug, Clone)]
pub struct OzoneRepo {
    /// Root of the main Git repository on real host disk.
    pub repo_root: PathBuf,
    /// Active Git metadata directory (`.git` or `.git/worktrees/<ws>`).
    pub git_dir: PathBuf,
    /// Shared `.git` directory across all worktrees.
    pub git_common_dir: PathBuf,
    /// Active working directory (main repo root, linked worktree, or `../<repo>.oz/<ws>`).
    pub workdir_path: PathBuf,
    /// Active Ozone workspace name, if inside a spawned/linked workspace.
    pub workspace: Option<String>,
    /// Whether the discovered context is a linked Git worktree.
    pub is_linked_worktree: bool,
    /// Colocated `.git/ozone/` directory paths.
    pub layout: OzoneLayout,
    /// Loaded `mount.json` if inside a VFS workspace.
    pub mount_record: Option<MountRecord>,
    /// Loaded `state/<ws>.json` if present.
    pub workspace_state: Option<WorkspaceState>,
    /// Health of the workspace mountpoint (`DisconnectedEnotconn` if FUSE returned `ENOTCONN`).
    pub mount_health: MountHealth,
    /// Checked-out branch name (`None` if detached HEAD).
    pub branch: Option<String>,
    /// Resolved `HEAD` commit hex OID (`None` on unborn branch).
    pub head_oid: Option<String>,
    /// Layered `.ozone.toml` + `.git/ozone/config.toml` configuration.
    pub config: OzoneConfig,
}

impl OzoneRepo {
    /// Discovers the repository from `start_dir` (e.g., `-C <dir>`) or the current directory.
    ///
    /// Handles `ENOTCONN` (`errno == 107`) on dead FUSE mounts at `../<repo>.oz/<ws>` by
    /// inspecting `PWD` when `current_dir()` fails and resolving `../<repo>/.git/ozone/ws/<ws>/mount.json`
    /// directly on real host disk before performing any stat calls inside the workspace mount.
    pub fn discover(start_dir: Option<&Path>) -> Result<Self> {
        let cwd_res = std::env::current_dir();
        let pwd_env = std::env::var_os("PWD").map(PathBuf::from);
        let git_dir_env = if start_dir.is_none() {
            std::env::var_os("GIT_DIR").map(PathBuf::from)
        } else {
            None
        };
        Self::discover_with_env(
            start_dir,
            cwd_res,
            pwd_env.as_deref(),
            git_dir_env.as_deref(),
            probe_mount_health_os,
        )
    }

    /// Opens the underlying repository on real host disk using `gix`.
    pub fn open_gix(&self) -> Result<gix::Repository> {
        let mut gix_repo =
            gix::open(&self.repo_root).map_err(|err| OzError::Git(err.to_string()))?;
        let _ = gix_repo.committer_or_set_fallback("Ozone", "oz@ozone.dev");
        Ok(gix_repo)
    }

    /// Loads layered Git configuration (`system`, `global`, `local`, `worktree`,
    /// `include` / `includeIf`, env vars, and CLI `-c` overrides) for this repository.
    pub fn load_git_config(
        &self,
        overrides: &[(String, String)],
    ) -> Result<crate::git_config::GitConfig> {
        crate::git_config::GitConfig::load_for_repo(self, overrides)
    }

    /// Deterministic discovery core allowing unit tests to simulate `current_dir()` `ENOTCONN`
    /// failures, `PWD` fallbacks, and mount probes without changing global process state.
    pub fn discover_with_env<F>(
        start_dir: Option<&Path>,
        current_dir_result: std::io::Result<PathBuf>,
        pwd_env: Option<&Path>,
        git_dir_env: Option<&Path>,
        probe_mount: F,
    ) -> Result<Self>
    where
        F: Fn(&Path) -> MountHealth,
    {
        let candidate_path = resolve_candidate_path(start_dir, current_dir_result, pwd_env)?;

        // CRITICAL: Check lexical `../<repo>.oz/<ws>` structure BEFORE touching `candidate_path`
        // on disk, so a dead FUSE mount returning ENOTCONN never blocks discovery.
        if let Some(oz_match) = parse_oz_workspace_path(&candidate_path)
            && let Some(discovered) = try_discover_from_sibling_oz(&oz_match, &probe_mount)?
        {
            return Ok(discovered);
        }

        // Standard ancestor walk for `.git` directory or linked worktree `.git` file.
        if let Some(discovered) = try_discover_ancestors(&candidate_path, &probe_mount)? {
            return Ok(discovered);
        }

        // Fallback when invoked via `git --git-dir=... ozone ...` with GIT_DIR in environment.
        if let Some(env_git_dir) = git_dir_env {
            let abs_git_dir = if env_git_dir.is_absolute() {
                normalize_lexical(env_git_dir)
            } else {
                normalize_lexical(&candidate_path.join(env_git_dir))
            };
            if abs_git_dir.exists() {
                return open_from_git_dir(&abs_git_dir, &candidate_path, None, &probe_mount);
            }
        }

        Err(OzError::NotARepository {
            path: candidate_path,
        })
    }
}

/// Parsed lexical `../<repo>.oz/<ws>` path match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiblingOzPath {
    pub repo_name: String,
    pub workspace_name: String,
    pub sibling_repo_root: PathBuf,
    pub workspace_root: PathBuf,
}

/// Lexically detects `<parent>/<repo>.oz/<ws>[/...]` without performing any filesystem I/O.
#[must_use]
pub fn parse_oz_workspace_path(path: &Path) -> Option<SiblingOzPath> {
    let normalized = normalize_lexical(path);
    let comps: Vec<Component<'_>> = normalized.components().collect();

    for (idx, comp) in comps.iter().enumerate() {
        let Component::Normal(os_name) = comp else {
            continue;
        };
        let Some(name_str) = os_name.to_str() else {
            continue;
        };
        let Some(repo_name) = name_str.strip_suffix(".oz") else {
            continue;
        };
        if repo_name.is_empty() {
            continue;
        }
        let Some(Component::Normal(ws_os)) = comps.get(idx + 1) else {
            continue;
        };
        let Some(ws_name) = ws_os.to_str() else {
            continue;
        };
        if ws_name.is_empty() {
            continue;
        }

        let mut parent_dir = PathBuf::new();
        for c in &comps[..idx] {
            parent_dir.push(c.as_os_str());
        }
        let sibling_repo_root = parent_dir.join(repo_name);
        let workspace_root = parent_dir.join(name_str).join(ws_name);

        return Some(SiblingOzPath {
            repo_name: repo_name.to_string(),
            workspace_name: ws_name.to_string(),
            sibling_repo_root,
            workspace_root,
        });
    }
    None
}

fn resolve_candidate_path(
    start_dir: Option<&Path>,
    current_dir_result: std::io::Result<PathBuf>,
    pwd_env: Option<&Path>,
) -> Result<PathBuf> {
    let base_cwd = || -> Result<PathBuf> {
        match current_dir_result {
            Ok(dir) => Ok(dir),
            Err(err) => {
                if let Some(pwd) = pwd_env
                    && pwd.is_absolute()
                {
                    return Ok(normalize_lexical(pwd));
                }
                Err(OzError::Io(err))
            }
        }
    };

    match start_dir {
        Some(path) if path.is_absolute() => Ok(normalize_lexical(path)),
        Some(rel) => Ok(normalize_lexical(&base_cwd()?.join(rel))),
        None => Ok(normalize_lexical(&base_cwd()?)),
    }
}

fn try_discover_from_sibling_oz<F>(
    oz_match: &SiblingOzPath,
    probe_mount: &F,
) -> Result<Option<OzoneRepo>>
where
    F: Fn(&Path) -> MountHealth,
{
    let host_repo = &oz_match.sibling_repo_root;
    let host_dot_git = host_repo.join(".git");
    let host_mount_json = host_dot_git
        .join("ozone")
        .join("ws")
        .join(&oz_match.workspace_name)
        .join("mount.json");

    // First priority: `../<repo>/.git/ozone/ws/<ws>/mount.json` on real host disk.
    if host_mount_json.is_file() {
        let raw = std::fs::read_to_string(&host_mount_json)?;
        let mount_record: MountRecord = serde_json::from_str(&raw)?;
        let repo_root = if mount_record.repo_root.is_dir() {
            mount_record.repo_root.clone()
        } else {
            host_repo.clone()
        };
        let git_common_dir = if mount_record.git_dir.exists() {
            mount_record.git_dir.clone()
        } else {
            repo_root.join(".git")
        };
        let worktree_git_dir = git_common_dir
            .join("worktrees")
            .join(&oz_match.workspace_name);
        let (git_dir, is_linked_worktree) = if worktree_git_dir.is_dir() {
            (worktree_git_dir, true)
        } else {
            (git_common_dir.clone(), false)
        };

        let layout = OzoneLayout::from_git_common_dir(&git_common_dir);
        let workspace_state = read_workspace_state(&layout, &oz_match.workspace_name)?;
        let config = OzoneConfig::load_for_repo(&repo_root, &layout.ozone_dir)?;

        let (branch, head_oid) = if is_linked_worktree {
            read_head_info_from_git_dir(&git_dir, &repo_root)
        } else {
            let br = Some(mount_record.branch.clone());
            let oid = resolve_branch_head_oid(&repo_root, br.as_deref())
                .or_else(|| mount_record.base_commit.clone());
            (br, oid)
        };

        let mount_health = probe_mount(&oz_match.workspace_root);

        return Ok(Some(OzoneRepo {
            repo_root,
            git_dir,
            git_common_dir,
            workdir_path: oz_match.workspace_root.clone(),
            workspace: Some(oz_match.workspace_name.clone()),
            is_linked_worktree,
            layout,
            mount_record: Some(mount_record),
            workspace_state,
            mount_health,
            branch,
            head_oid,
            config,
        }));
    }

    // Second priority: `../<repo>/.git` exists on real host disk (e.g., native CoW reflink or
    // CAS hardlink workspace, or FUSE mount before mount.json was created).
    if let Ok(meta) = std::fs::symlink_metadata(&host_dot_git) {
        let git_common_dir = if meta.is_dir() {
            host_dot_git
        } else if meta.is_file() {
            resolve_gitdir_file(&host_dot_git, host_repo)?
        } else {
            return Ok(None);
        };

        let layout = OzoneLayout::from_git_common_dir(&git_common_dir);
        let workspace_state = read_workspace_state(&layout, &oz_match.workspace_name)?;
        let worktree_git_dir = git_common_dir
            .join("worktrees")
            .join(&oz_match.workspace_name);
        let (git_dir, is_linked_worktree) = if worktree_git_dir.is_dir() {
            (worktree_git_dir, true)
        } else {
            (git_common_dir.clone(), false)
        };

        let config = OzoneConfig::load_for_repo(host_repo, &layout.ozone_dir)?;
        let (branch, head_oid) = if is_linked_worktree {
            read_head_info_from_git_dir(&git_dir, host_repo)
        } else if let Some(ref st) = workspace_state {
            let oid = resolve_branch_head_oid(host_repo, Some(st.branch.as_str()))
                .or_else(|| st.base_commit.clone());
            (Some(st.branch.clone()), oid)
        } else {
            read_head_info_from_repo(host_repo)
        };

        let mount_health = probe_mount(&oz_match.workspace_root);

        return Ok(Some(OzoneRepo {
            repo_root: host_repo.clone(),
            git_dir,
            git_common_dir,
            workdir_path: oz_match.workspace_root.clone(),
            workspace: Some(oz_match.workspace_name.clone()),
            is_linked_worktree,
            layout,
            mount_record: None,
            workspace_state,
            mount_health,
            branch,
            head_oid,
            config,
        }));
    }

    Ok(None)
}

fn try_discover_ancestors<F>(candidate_path: &Path, probe_mount: &F) -> Result<Option<OzoneRepo>>
where
    F: Fn(&Path) -> MountHealth,
{
    for dir in candidate_path.ancestors() {
        let dot_git = dir.join(".git");
        let meta = match std::fs::symlink_metadata(&dot_git) {
            Ok(m) => m,
            Err(err) if is_enotconn(&err) => continue,
            Err(_) => continue,
        };

        if meta.is_dir() {
            return Ok(Some(open_from_git_dir(&dot_git, dir, None, probe_mount)?));
        }

        if meta.is_file() {
            let worktree_git_dir = resolve_gitdir_file(&dot_git, dir)?;
            return Ok(Some(open_from_git_dir(
                &worktree_git_dir,
                dir,
                worktree_git_dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(str::to_string),
                probe_mount,
            )?));
        }
    }
    Ok(None)
}

fn open_from_git_dir<F>(
    git_dir: &Path,
    workdir_hint: &Path,
    workspace_hint: Option<String>,
    probe_mount: &F,
) -> Result<OzoneRepo>
where
    F: Fn(&Path) -> MountHealth,
{
    let commondir_file = git_dir.join("commondir");
    let (git_common_dir, is_linked_worktree) = if commondir_file.is_file() {
        let rel = std::fs::read_to_string(&commondir_file)?;
        let common = normalize_lexical(&git_dir.join(rel.trim()));
        (common, true)
    } else {
        (git_dir.to_path_buf(), false)
    };

    let repo_root = if git_common_dir.file_name() == Some(OsStr::new(".git")) {
        git_common_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| workdir_hint.to_path_buf())
    } else {
        workdir_hint.to_path_buf()
    };

    let layout = OzoneLayout::from_git_common_dir(&git_common_dir);
    let workspace = if is_linked_worktree {
        workspace_hint.or_else(|| {
            git_dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
        })
    } else {
        None
    };

    let mount_record = if let Some(ref ws) = workspace {
        read_mount_record(&layout, ws)?
    } else {
        None
    };
    let workspace_state = if let Some(ref ws) = workspace {
        read_workspace_state(&layout, ws)?
    } else {
        None
    };

    let (branch, head_oid) = read_head_info_from_git_dir(git_dir, &repo_root);
    let config = OzoneConfig::load_for_repo(&repo_root, &layout.ozone_dir)?;

    let mount_health = if mount_record.is_some() {
        probe_mount(workdir_hint)
    } else {
        MountHealth::NotApplicable
    };

    Ok(OzoneRepo {
        repo_root,
        git_dir: git_dir.to_path_buf(),
        git_common_dir,
        workdir_path: workdir_hint.to_path_buf(),
        workspace,
        is_linked_worktree,
        layout,
        mount_record,
        workspace_state,
        mount_health,
        branch,
        head_oid,
        config,
    })
}

fn resolve_gitdir_file(dot_git_file: &Path, workdir: &Path) -> Result<PathBuf> {
    let raw = std::fs::read_to_string(dot_git_file)?;
    let target = raw
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))
        .map(str::trim)
        .ok_or_else(|| {
            OzError::Git(format!("malformed .git file at {}", dot_git_file.display()))
        })?;
    let p = Path::new(target);
    if p.is_absolute() {
        Ok(normalize_lexical(p))
    } else {
        Ok(normalize_lexical(&workdir.join(p)))
    }
}

fn read_mount_record(layout: &OzoneLayout, ws: &str) -> Result<Option<MountRecord>> {
    let p = layout.ws_mount_record_path(ws);
    if !p.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(p)?;
    Ok(Some(serde_json::from_str(&raw)?))
}

fn read_workspace_state(layout: &OzoneLayout, ws: &str) -> Result<Option<WorkspaceState>> {
    let p = layout.workspace_state_path(ws);
    if !p.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(p)?;
    Ok(Some(serde_json::from_str(&raw)?))
}

fn read_head_info_from_repo(repo_root: &Path) -> (Option<String>, Option<String>) {
    let Ok(repo) = gix::open(repo_root) else {
        return (None, None);
    };
    extract_head_from_gix(&repo)
}

fn read_head_info_from_git_dir(
    git_dir: &Path,
    fallback_repo_root: &Path,
) -> (Option<String>, Option<String>) {
    if let Ok(repo) = gix::open(git_dir) {
        return extract_head_from_gix(&repo);
    }
    if let Some(parent) = git_dir.parent()
        && let Ok(repo) = gix::open(parent)
    {
        return extract_head_from_gix(&repo);
    }
    read_head_info_from_repo(fallback_repo_root)
}

fn extract_head_from_gix(repo: &gix::Repository) -> (Option<String>, Option<String>) {
    let Ok(head) = repo.head() else {
        return (None, None);
    };
    let branch = head
        .referent_name()
        .map(|name| String::from_utf8_lossy(name.shorten().as_ref()).into_owned());
    let head_oid = if head.is_unborn() {
        None
    } else {
        head.try_into_peeled_id()
            .ok()
            .flatten()
            .map(|id| id.to_hex().to_string())
    };
    (branch, head_oid)
}

fn resolve_branch_head_oid(repo_root: &Path, branch: Option<&str>) -> Option<String> {
    let branch = branch?;
    let repo = gix::open(repo_root).ok()?;
    let ref_name = format!("refs/heads/{branch}");
    let mut reference = repo.try_find_reference(&ref_name).ok()??;
    reference
        .peel_to_id()
        .ok()
        .map(|id| id.to_hex().to_string())
}

/// Probes a workspace mountpoint path to see whether it is healthy, missing, or `ENOTCONN`.
#[must_use]
pub fn probe_mount_health_os(path: &Path) -> MountHealth {
    match std::fs::symlink_metadata(path) {
        Ok(_) => MountHealth::Healthy,
        Err(err) if is_enotconn(&err) => MountHealth::DisconnectedEnotconn,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => MountHealth::NotMounted,
        Err(_) => MountHealth::NotMounted,
    }
}

/// Returns `true` if `err` represents `ENOTCONN` (`errno == 107` on Linux).
#[must_use]
pub fn is_enotconn(err: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        err.raw_os_error() == Some(libc::ENOTCONN)
    }
    #[cfg(not(unix))]
    {
        err.raw_os_error() == Some(107) || err.raw_os_error() == Some(10057)
    }
}

#[cfg(unix)]
fn short_unix_socket_path(git_common_dir: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(git_common_dir)
        .unwrap_or_else(|_| normalize_lexical(git_common_dir));
    let digest = blake3::hash(canonical.as_os_str().as_encoded_bytes());
    let hex = digest.to_hex();
    let short_hash = &hex.as_str()[..16];
    let file_name = format!("v-{short_hash}.sock");

    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR")
        && !xdg.is_empty()
    {
        let cand = PathBuf::from(xdg).join("ozone").join(&file_name);
        if cand.as_os_str().as_encoded_bytes().len() <= 100 {
            return cand;
        }
    }

    let uid = unsafe { libc::geteuid() };
    let tmp_dir = PathBuf::from(format!("/tmp/oz-{uid}"));
    let cand = tmp_dir.join(&file_name);
    if cand.as_os_str().as_encoded_bytes().len() <= 100 {
        return cand;
    }

    std::env::temp_dir()
        .join(format!("oz-{uid}"))
        .join(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BackendKind;
    use chrono::Utc;
    use std::process::Command;
    use tempfile::TempDir;

    fn init_git_repo(path: &Path, make_commit: bool) -> Option<String> {
        std::fs::create_dir_all(path).expect("mkdir repo");
        let init_out = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(path)
            .output()
            .expect("git init");
        assert!(init_out.status.success());

        if !make_commit {
            return None;
        }

        Command::new("git")
            .args(["config", "user.name", "Ozone Test"])
            .current_dir(path)
            .output()
            .expect("git config name");
        Command::new("git")
            .args(["config", "user.email", "test@ozone.dev"])
            .current_dir(path)
            .output()
            .expect("git config email");

        std::fs::write(path.join("README.md"), "# test\n").expect("write readme");
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(path)
            .output()
            .expect("git add");
        let commit_out = Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(path)
            .output()
            .expect("git commit");
        assert!(commit_out.status.success());

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(path)
            .output()
            .expect("rev-parse");
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    #[test]
    fn discovers_main_repo_and_nested_subdir_including_unborn() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().join("my-repo");
        init_git_repo(&repo_root, false);
        let nested = repo_root.join("crates/oz-core");
        std::fs::create_dir_all(&nested).expect("mkdir nested");

        let found = OzoneRepo::discover(Some(&nested)).expect("discover unborn repo");
        assert_eq!(found.repo_root, repo_root);
        assert_eq!(found.git_common_dir, repo_root.join(".git"));
        assert_eq!(found.branch.as_deref(), Some("main"));
        assert_eq!(found.head_oid, None);
        assert!(!found.is_linked_worktree);
        assert_eq!(found.workspace, None);
        assert!(!found.layout.is_initialized());

        found.layout.ensure_initialized().expect("init ozone dir");
        assert!(found.layout.oplog_dir().is_dir());
        assert!(found.layout.state_dir().is_dir());
        assert!(found.layout.baselines_dir().is_dir());
        assert!(found.layout.ws_dir().is_dir());
    }

    #[test]
    fn discovers_linked_git_worktree() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().join("app");
        let oid = init_git_repo(&repo_root, true).expect("commit oid");

        let wt_path = dir.path().join("app-wt-auth");
        let wt_out = Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "feat/auth",
                wt_path.to_str().expect("utf8"),
            ])
            .current_dir(&repo_root)
            .output()
            .expect("git worktree add");
        assert!(wt_out.status.success());

        let found = OzoneRepo::discover(Some(&wt_path)).expect("discover linked worktree");
        let canon_root = std::fs::canonicalize(&repo_root).expect("canon repo_root");
        let canon_common = std::fs::canonicalize(repo_root.join(".git")).expect("canon git_common_dir");
        assert_eq!(found.repo_root, canon_root);
        assert_eq!(found.git_common_dir, canon_common);
        assert!(found.is_linked_worktree);
        assert_eq!(found.workspace.as_deref(), Some("app-wt-auth"));
        assert_eq!(found.branch.as_deref(), Some("feat/auth"));
        assert_eq!(found.head_oid.as_deref(), Some(oid.as_str()));
    }

    #[test]
    fn discovers_sibling_oz_workspace_even_when_fuse_returns_enotconn_and_cwd_fails() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().join("my-repo");
        let oid = init_git_repo(&repo_root, true).expect("commit oid");

        // Create feature branch so head_oid resolves from real disk ODB.
        let br_status = Command::new("git")
            .args(["branch", "feat/pkce"])
            .current_dir(&repo_root)
            .status()
            .expect("git branch");
        assert!(br_status.success());

        let layout = OzoneLayout::from_git_common_dir(repo_root.join(".git"));
        layout.ensure_initialized().expect("init ozone");

        // Intentionally DO NOT create `../my-repo.oz/agent-auth` on disk, and simulate
        // both `current_dir()` failing with ENOTCONN (errno 107) and the mount probe
        // returning ENOTCONN — discovery must resolve solely from real host disk
        // `../my-repo/.git/ozone/ws/agent-auth/mount.json`.
        let dead_ws_path = dir.path().join("my-repo.oz/agent-auth/crates/oz-core");
        let mount_point = dir.path().join("my-repo.oz/agent-auth");
        let record = MountRecord {
            schema_version: MountRecord::CURRENT_SCHEMA_VERSION,
            workspace: "agent-auth".to_string(),
            branch: "feat/pkce".to_string(),
            backend: BackendKind::LinuxFuse3,
            repo_root: repo_root.clone(),
            git_dir: repo_root.join(".git"),
            mount_point: mount_point.clone(),
            upper_dir: Some(layout.ws_upper_dir("agent-auth")),
            meta_dir: Some(layout.ws_tombstone_dir("agent-auth")),
            base_tree: None,
            base_commit: Some(oid.clone()),
            socket_path: Some(layout.vfsd_socket_path()),
            daemon_pid: Some(99999),
            mounted: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let ws_meta = layout.ws_meta_dir("agent-auth");
        std::fs::create_dir_all(&ws_meta).expect("mkdir ws meta");
        std::fs::write(
            layout.ws_mount_record_path("agent-auth"),
            serde_json::to_string_pretty(&record).expect("json"),
        )
        .expect("write mount.json");

        #[cfg(unix)]
        let cwd_err = std::io::Error::from_raw_os_error(libc::ENOTCONN);
        #[cfg(not(unix))]
        let cwd_err = std::io::Error::from_raw_os_error(107);

        let found =
            OzoneRepo::discover_with_env(None, Err(cwd_err), Some(&dead_ws_path), None, |_| {
                MountHealth::DisconnectedEnotconn
            })
            .expect("discover across dead FUSE mount via PWD and mount.json");

        assert_eq!(found.repo_root, repo_root);
        assert_eq!(found.git_common_dir, repo_root.join(".git"));
        assert_eq!(found.workdir_path, mount_point);
        assert_eq!(found.workspace.as_deref(), Some("agent-auth"));
        assert_eq!(found.branch.as_deref(), Some("feat/pkce"));
        assert_eq!(found.head_oid.as_deref(), Some(oid.as_str()));
        assert_eq!(found.mount_health, MountHealth::DisconnectedEnotconn);
        assert!(found.mount_record.is_some());

        // Also test explicit `-C <dead_ws_path>` invocation directly.
        let found_via_c = OzoneRepo::discover_with_env(
            Some(&dead_ws_path),
            Ok(dir.path().to_path_buf()),
            None,
            None,
            |_| MountHealth::DisconnectedEnotconn,
        )
        .expect("discover via -C inside dead FUSE mount");
        assert_eq!(found_via_c.repo_root, repo_root);
        assert_eq!(found_via_c.workspace.as_deref(), Some("agent-auth"));
        assert_eq!(found_via_c.branch.as_deref(), Some("feat/pkce"));
    }

    #[test]
    fn discovers_sibling_oz_workspace_from_state_without_mount_json() {
        let dir = TempDir::new().expect("tempdir");
        let repo_root = dir.path().join("web");
        let oid = init_git_repo(&repo_root, true).expect("commit oid");
        let br_out = Command::new("git")
            .args(["branch", "feat/ui"])
            .current_dir(&repo_root)
            .output()
            .expect("git branch");
        assert!(br_out.status.success());

        let layout = OzoneLayout::from_git_common_dir(repo_root.join(".git"));
        layout.ensure_initialized().expect("init ozone");

        let ws_path = dir.path().join("web.oz/agent-ui");
        std::fs::create_dir_all(&ws_path).expect("mkdir ws");
        let state = WorkspaceState {
            schema_version: WorkspaceState::CURRENT_SCHEMA_VERSION,
            name: "agent-ui".to_string(),
            branch: "feat/ui".to_string(),
            base_branch: Some("main".to_string()),
            base_commit: Some(oid.clone()),
            base_tree: oid.clone(),
            backend: BackendKind::CasHardlink,
            worktree_path: ws_path.clone(),
            git_worktree_dir: None,
            baseline_id: None,
            dirty_paths: Default::default(),
            pinned_paths: Default::default(),
            scope: Vec::new(),
            deny: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_checkpoint_oid: None,
        };
        std::fs::write(
            layout.workspace_state_path("agent-ui"),
            serde_json::to_string_pretty(&state).expect("json"),
        )
        .expect("write state");

        let found = OzoneRepo::discover(Some(&ws_path)).expect("discover from state");
        assert_eq!(found.repo_root, repo_root);
        assert_eq!(found.workspace.as_deref(), Some("agent-ui"));
        assert_eq!(found.branch.as_deref(), Some("feat/ui"));
        assert_eq!(found.head_oid.as_deref(), Some(oid.as_str()));
        assert_eq!(found.mount_health, MountHealth::Healthy);
        assert!(found.workspace_state.is_some());
    }

    #[test]
    fn detects_git_ozone_invocation_alias() {
        assert_eq!(
            CliInvocation::from_arg0(Some(OsStr::new("/usr/local/bin/git-ozone"))),
            CliInvocation::GitOzone
        );
        assert_eq!(
            CliInvocation::from_arg0(Some(OsStr::new("C:\\bin\\git-ozone.exe"))),
            CliInvocation::GitOzone
        );
        assert_eq!(
            CliInvocation::from_arg0(Some(OsStr::new("oz"))),
            CliInvocation::Oz
        );
    }

    #[cfg(unix)]
    #[test]
    fn vfsd_socket_path_hashes_overlong_unix_paths_below_sun_len() {
        use std::os::unix::net::UnixListener;

        let dir = TempDir::new().expect("tempdir");
        let short_git = dir.path().join("r/.git");
        let short_layout = OzoneLayout::from_git_common_dir(&short_git);
        assert_eq!(short_layout.vfsd_socket_path(), short_git.join("ozone/vfsd.sock"));

        // Construct a deeply nested path whose `.git/ozone/vfsd.sock` exceeds Unix SUN_LEN (~108B).
        let deep_git = dir
            .path()
            .join("very-long-workspace-parent-segment-0123456789")
            .join("another-very-long-worktree-uuid-segment-9b09e381-0f74-40cc-a460-95d7d1beabd7")
            .join("repo/.git");
        std::fs::create_dir_all(&deep_git).expect("mkdir deep git");
        let deep_layout = OzoneLayout::from_git_common_dir(&deep_git);
        let sock = deep_layout.vfsd_socket_path();
        assert!(
            sock.as_os_str().as_encoded_bytes().len() <= 100,
            "hashed socket path '{}' exceeds 100 bytes",
            sock.display()
        );
        assert_eq!(sock, deep_layout.vfsd_socket_path(), "hash must be deterministic");

        // Verify a real UnixListener can bind to the shortened socket path without EINVAL/SUN_LEN.
        if let Some(parent) = sock.parent() {
            std::fs::create_dir_all(parent).expect("mkdir sock parent");
        }
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).expect("bind hashed unix socket");
        drop(listener);
        let _ = std::fs::remove_file(&sock);
    }
}
