// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Non-interactive GitHub token discovery, remote URL parsing, and `oz doctor` auth diagnostics.

use crate::GitTransportKind;
use crate::ssh_config::resolve_ssh_host;
use gix::bstr::ByteSlice;
use oz_core::{GitConfig, GitConfigContext, OzError, OzoneRepo, Result};
use oz_cow::materialize::resolve_git_common_dir;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Default public GitHub REST API base URL.
pub const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";

/// Underlying URL scheme of a Git remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteScheme {
    Https,
    Http,
    Ssh,
    File,
}

/// Parsed Git remote metadata supporting GitHub.com, GitHub Enterprise, local HTTP mocks, and local paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedGitRemote {
    pub raw_url: String,
    pub sanitized_url: String,
    pub scheme: RemoteScheme,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<GitTransportKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub api_base_url: String,
}

impl ParsedGitRemote {
    /// Returns `(owner, repo)` or an actionable [`OzError::InvalidUsage`] if this remote
    /// does not have a `<owner>/<repo>` path structure.
    pub fn require_owner_repo(&self) -> Result<(&str, &str)> {
        match (self.owner.as_deref(), self.repo.as_deref()) {
            (Some(owner), Some(repo)) if !owner.is_empty() && !repo.is_empty() => Ok((owner, repo)),
            _ => Err(OzError::InvalidUsage {
                message: format!(
                    "remote URL '{}' does not look like a '<owner>/<repo>' GitHub remote",
                    self.sanitized_url
                ),
                recovery: vec![
                    "Configure a GitHub remote, e.g. `git remote add origin https://github.com/<owner>/<repo>.git`"
                        .to_string(),
                ],
            }),
        }
    }

    /// Returns `true` when the remote points to an HTTP or HTTPS endpoint.
    #[must_use]
    pub const fn is_http(&self) -> bool {
        matches!(self.scheme, RemoteScheme::Https | RemoteScheme::Http)
    }
}

/// Source where a GitHub authentication token was discovered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TokenSource {
    Explicit,
    OzGithubTokenEnv,
    GithubTokenEnv,
    GhTokenEnv,
    GhHostsYml { path: PathBuf },
    GitCredentialHelper,
}

impl TokenSource {
    /// Short machine-readable label for diagnostics.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::OzGithubTokenEnv => "OZ_GITHUB_TOKEN",
            Self::GithubTokenEnv => "GITHUB_TOKEN",
            Self::GhTokenEnv => "GH_TOKEN",
            Self::GhHostsYml { .. } => "gh_hosts_yml",
            Self::GitCredentialHelper => "git_credential_fill",
        }
    }
}

/// Non-interactively discovered GitHub token and its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredToken {
    pub token: String,
    pub source: TokenSource,
    pub host: String,
}

impl DiscoveredToken {
    /// Redacts the token value for safe display in `oz doctor` or debug logs.
    #[must_use]
    pub fn masked(&self) -> String {
        mask_secret(&self.token)
    }
}

/// Configurable inputs for deterministic, thread-safe token discovery without mutating global env.
#[derive(Debug, Clone)]
pub struct TokenDiscoveryOptions {
    /// Target host (defaults to `"github.com"`).
    pub host: String,
    /// Target URL protocol (`"https"` or `"http"`) for `git credential fill`.
    pub protocol: String,
    /// Highest-priority explicit token override (e.g. CLI flag or programmatic caller).
    pub explicit_token: Option<String>,
    /// Optional isolated environment variable map (uses `std::env::var` when `None`).
    pub env_overrides: Option<BTreeMap<String, String>>,
    /// Optional explicit path to `hosts.yml` (overrides `$GH_CONFIG_DIR/hosts.yml` lookup when `Some`).
    pub gh_hosts_file: Option<PathBuf>,
    /// Whether to invoke `git credential fill` with `GIT_TERMINAL_PROMPT=0` as final fallback.
    pub use_git_credential_helper: bool,
    /// Optional working directory when invoking `git credential fill`.
    pub credential_workdir: Option<PathBuf>,
    /// Maximum timeout for `git credential fill`.
    pub credential_timeout: Duration,
}

impl Default for TokenDiscoveryOptions {
    fn default() -> Self {
        Self {
            host: "github.com".to_string(),
            protocol: "https".to_string(),
            explicit_token: None,
            env_overrides: None,
            gh_hosts_file: None,
            use_git_credential_helper: true,
            credential_workdir: None,
            credential_timeout: Duration::from_secs(2),
        }
    }
}

/// Diagnostic report returned by [`probe_github_auth`] for `oz doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubAuthProbe {
    pub remote_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<GitTransportKind>,
    pub api_base_url: String,
    pub authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_source: Option<TokenSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub masked_token: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<String>,
}

/// Parses a Git remote URL (`https://`, `http://`, `git@host:owner/repo.git`, `ssh://`, `file://`,
/// or local path) and resolves the corresponding GitHub API base URL.
pub fn parse_git_remote(raw_url: &str, api_url_override: Option<&str>) -> Result<ParsedGitRemote> {
    let env_api = std::env::var("OZ_GITHUB_API_URL").ok();
    parse_git_remote_with_env(raw_url, api_url_override, env_api.as_deref())
}

/// Deterministic variant of [`parse_git_remote`] accepting an explicit `OZ_GITHUB_API_URL` value.
pub fn parse_git_remote_with_env(
    raw_url: &str,
    api_url_override: Option<&str>,
    env_api_url: Option<&str>,
) -> Result<ParsedGitRemote> {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "remote URL must not be empty".to_string(),
            recovery: Vec::new(),
        });
    }

    // 1. Standard URL schemes: https://, http://, ssh://, file://
    if trimmed.starts_with("https://")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("ssh://")
        || trimmed.starts_with("file://")
    {
        let parsed = url::Url::parse(trimmed).map_err(|err| OzError::InvalidUsage {
            message: format!("invalid remote URL '{trimmed}': {err}"),
            recovery: Vec::new(),
        })?;

        let scheme = match parsed.scheme() {
            "https" => RemoteScheme::Https,
            "http" => RemoteScheme::Http,
            "ssh" => RemoteScheme::Ssh,
            "file" => RemoteScheme::File,
            other => {
                return Err(OzError::InvalidUsage {
                    message: format!("unsupported remote URL scheme '{other}'"),
                    recovery: Vec::new(),
                });
            }
        };

        let transport = match scheme {
            RemoteScheme::Https | RemoteScheme::Http => Some(GitTransportKind::Https),
            RemoteScheme::Ssh => Some(GitTransportKind::Ssh),
            RemoteScheme::File => None,
        };

        let host = parsed.host_str().map(|h| match parsed.port() {
            Some(port) if matches!(scheme, RemoteScheme::Https | RemoteScheme::Http) => {
                format!("{h}:{port}")
            }
            _ => h.to_string(),
        });

        let (owner, repo) = if scheme == RemoteScheme::File {
            if Path::new(parsed.path())
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("git"))
            {
                extract_owner_repo_from_path(parsed.path())
            } else {
                (None, None)
            }
        } else {
            extract_owner_repo_from_path(parsed.path())
        };

        let mut sanitized = parsed.clone();
        if matches!(scheme, RemoteScheme::Https | RemoteScheme::Http) {
            let _ = sanitized.set_username("");
            let _ = sanitized.set_password(None);
        }
        let sanitized_url = if scheme == RemoteScheme::File {
            trimmed.to_string()
        } else {
            sanitized.to_string()
        };

        let api_base_url =
            compute_api_base_url(scheme, host.as_deref(), api_url_override, env_api_url);

        return Ok(ParsedGitRemote {
            raw_url: trimmed.to_string(),
            sanitized_url,
            scheme,
            transport,
            host,
            owner,
            repo,
            api_base_url,
        });
    }

    // 2. SCP-like SSH syntax: `[user@]host:owner/repo(.git)` (excluding Windows drive letters `C:\...` or `C:/...`).
    if let Some((host_part, path_part)) = split_scp_like_ssh(trimmed) {
        let host = host_part
            .rsplit_once('@')
            .map_or(host_part, |(_, h)| h)
            .to_string();
        let (owner, repo) = extract_owner_repo_from_path(path_part);
        let api_base_url = compute_api_base_url(
            RemoteScheme::Ssh,
            Some(host.as_str()),
            api_url_override,
            env_api_url,
        );

        return Ok(ParsedGitRemote {
            raw_url: trimmed.to_string(),
            sanitized_url: trimmed.to_string(),
            scheme: RemoteScheme::Ssh,
            transport: Some(GitTransportKind::Ssh),
            host: Some(host),
            owner,
            repo,
            api_base_url,
        });
    }

    // 3. Local filesystem path (`/path/to/repo`, `./repo`, `../repo.git`, `C:\repo`).
    let (owner, repo) = if Path::new(trimmed)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("git"))
    {
        extract_owner_repo_from_path(trimmed)
    } else {
        (None, None)
    };
    let api_base_url =
        compute_api_base_url(RemoteScheme::File, None, api_url_override, env_api_url);
    Ok(ParsedGitRemote {
        raw_url: trimmed.to_string(),
        sanitized_url: trimmed.to_string(),
        scheme: RemoteScheme::File,
        transport: None,
        host: None,
        owner,
        repo,
        api_base_url,
    })
}

fn split_scp_like_ssh(input: &str) -> Option<(&str, &str)> {
    if input.starts_with('/') || input.starts_with("./") || input.starts_with("../") {
        return None;
    }
    // Exclude Windows drive paths like `C:\foo` or `C:/foo`.
    let bytes = input.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
    {
        return None;
    }

    let (left, right) = input.split_once(':')?;
    if left.is_empty() || right.is_empty() || left.contains('/') || left.contains('\\') {
        return None;
    }
    Some((left, right))
}

fn extract_owner_repo_from_path(path: &str) -> (Option<String>, Option<String>) {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return (None, None);
    }
    let segments: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return (None, None);
    }
    let owner = segments[segments.len() - 2].to_string();
    let raw_repo = segments[segments.len() - 1];
    let repo = raw_repo
        .strip_suffix(".git")
        .unwrap_or(raw_repo)
        .to_string();
    if owner.is_empty() || repo.is_empty() {
        (None, None)
    } else {
        (Some(owner), Some(repo))
    }
}

fn compute_api_base_url(
    scheme: RemoteScheme,
    host: Option<&str>,
    api_url_override: Option<&str>,
    env_api_url: Option<&str>,
) -> String {
    if let Some(explicit) = api_url_override.map(str::trim).filter(|s| !s.is_empty()) {
        return explicit.trim_end_matches('/').to_string();
    }
    if let Some(env_val) = env_api_url.map(str::trim).filter(|s| !s.is_empty()) {
        return env_val.trim_end_matches('/').to_string();
    }
    let Some(host) = host else {
        return DEFAULT_GITHUB_API_URL.to_string();
    };
    let host_no_port = strip_host_port(host);
    if host_no_port.eq_ignore_ascii_case("github.com") {
        return DEFAULT_GITHUB_API_URL.to_string();
    }
    let http_scheme = match scheme {
        RemoteScheme::Http => "http",
        _ => "https",
    };
    format!("{http_scheme}://{host}/api/v3")
}

/// Strips an optional `:port` suffix from a `host[:port]` authority string.
#[must_use]
pub(crate) fn strip_host_port(host: &str) -> &str {
    if let Some((h, port)) = host.rsplit_once(':')
        && !port.is_empty()
        && port.chars().all(|c| c.is_ascii_digit())
        && (!h.contains(':') || (h.starts_with('[') && h.ends_with(']')))
    {
        return h;
    }
    host
}

/// Returns `true` when `host` (with optional `:port`) refers to a local loopback interface.
#[must_use]
pub(crate) fn is_loopback_host(host: &str) -> bool {
    let bare = strip_host_port(host.trim()).to_ascii_lowercase();
    matches!(bare.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Reads the configured URL for `remote_name` (default `"origin"`) from `repo`,
/// resolving `includeIf`, `insteadOf` / `pushInsteadOf`, and SSH `HostName` aliases.
pub(crate) fn resolve_repo_remote(
    repo: &OzoneRepo,
    remote_name: Option<&str>,
    api_url_override: Option<&str>,
) -> Result<ParsedGitRemote> {
    let remote = remote_name.unwrap_or("origin");
    let raw_url =
        read_remote_url_from_repo(repo, remote, false)?.ok_or_else(|| OzError::InvalidUsage {
            message: format!(
                "remote '{remote}' is not configured in {}",
                repo.repo_root.display()
            ),
            recovery: vec![format!("git remote add {remote} <url>")],
        })?;
    let mut parsed = parse_git_remote(&raw_url, api_url_override)?;
    if parsed.scheme == RemoteScheme::Ssh
        && let Some(ref alias_host) = parsed.host
    {
        let resolved_ssh = resolve_ssh_host(alias_host);
        if let Some(real_host) = resolved_ssh.hostname
            && !real_host.is_empty()
        {
            parsed.host = Some(real_host.clone());
            if api_url_override.is_none() {
                let env_api = std::env::var("OZ_GITHUB_API_URL")
                    .ok()
                    .filter(|s| !s.trim().is_empty());
                parsed.api_base_url =
                    compute_api_base_url(parsed.scheme, Some(&real_host), None, env_api.as_deref());
            }
        }
    }
    Ok(parsed)
}

/// Loads layered [`GitConfig`] (`includeIf`, `insteadOf`, etc.) for an open [`gix::Repository`].
pub(crate) fn load_git_config_from_gix_repo(
    gix_repo: &gix::Repository,
    overrides: &[(String, String)],
) -> Result<GitConfig> {
    let git_dir = gix_repo.git_dir().to_path_buf();
    let git_common_dir = resolve_git_common_dir(&git_dir)?;
    let branch = gix_repo
        .head_name()
        .ok()
        .flatten()
        .map(|n| n.shorten().to_string());
    let ctx = GitConfigContext {
        git_dir: Some(git_dir),
        git_common_dir: Some(git_common_dir),
        workdir_path: gix_repo.workdir().map(Path::to_path_buf),
        branch,
        overrides: overrides.to_vec(),
        ..Default::default()
    };
    GitConfig::load_with_context(&ctx)
}

/// Reads `remote.<name>.pushurl` (when `for_push` is `true`) or `remote.<name>.url`
/// from layered Git config, applying `includeIf` and `insteadOf` / `pushInsteadOf`.
pub(crate) fn read_remote_url_from_repo(
    repo: &OzoneRepo,
    remote_name: &str,
    for_push: bool,
) -> Result<Option<String>> {
    let cfg = repo.load_git_config(&[])?;
    let resolved = cfg.resolve_remote_urls(remote_name);
    let url = if for_push {
        resolved.push_url
    } else {
        resolved.fetch_url
    };
    if url.is_some() {
        return Ok(url);
    }
    let gix_repo = gix::open(&repo.git_common_dir).or_else(|_| repo.open_gix())?;
    Ok(read_remote_url_from_gix(&gix_repo, remote_name, for_push))
}

/// Reads `remote.<name>.pushurl` or `remote.<name>.url` using layered Git config
/// (`includeIf`, `insteadOf`, `pushInsteadOf`) for `gix_repo`.
pub(crate) fn read_remote_url_from_gix(
    gix_repo: &gix::Repository,
    remote_name: &str,
    for_push: bool,
) -> Option<String> {
    if let Ok(layered) = load_git_config_from_gix_repo(gix_repo, &[]) {
        let resolved = layered.resolve_remote_urls(remote_name);
        let url = if for_push {
            resolved.push_url
        } else {
            resolved.fetch_url
        };
        if url.is_some() {
            return url;
        }
    }

    let cfg = gix_repo.config_snapshot();
    if for_push {
        let push_key = format!("remote.{remote_name}.pushurl");
        if let Some(val) = cfg.string(&push_key) {
            let s = val.to_str_lossy().trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    let url_key = format!("remote.{remote_name}.url");
    if let Some(val) = cfg.string(&url_key) {
        let s = val.to_str_lossy().trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Discovers a GitHub token strictly non-interactively in priority order:
/// 1. `explicit_token`
/// 2. `OZ_GITHUB_TOKEN`
/// 3. `GITHUB_TOKEN`
/// 4. `GH_TOKEN`
/// 5. `gh` CLI `hosts.yml` (`$GH_CONFIG_DIR/hosts.yml`, `~/.config/gh/hosts.yml`, `%APPDATA%\GitHub CLI\hosts.yml`)
/// 6. `git credential fill` with `GIT_TERMINAL_PROMPT=0` and closed stdin
#[must_use]
pub fn discover_github_token(
    host: Option<&str>,
    explicit_token: Option<&str>,
) -> Option<DiscoveredToken> {
    let opts = TokenDiscoveryOptions {
        host: host.unwrap_or("github.com").to_string(),
        explicit_token: explicit_token.map(str::to_string),
        ..TokenDiscoveryOptions::default()
    };
    discover_github_token_with_options(&opts)
}

/// Deterministic, configurable token discovery entrypoint.
#[must_use]
pub fn discover_github_token_with_options(opts: &TokenDiscoveryOptions) -> Option<DiscoveredToken> {
    let target_host = if opts.host.trim().is_empty() {
        "github.com".to_string()
    } else {
        opts.host.trim().to_string()
    };
    let bare_host = strip_host_port(&target_host).to_ascii_lowercase();
    let is_loopback = matches!(
        bare_host.as_str(),
        "localhost" | "127.0.0.1" | "::1" | "[::1]"
    );

    // 1. Explicit override
    if let Some(tok) = opts
        .explicit_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(DiscoveredToken {
            token: tok.to_string(),
            source: TokenSource::Explicit,
            host: target_host,
        });
    }

    // Never auto-discover tokens over unencrypted HTTP unless targeting a loopback host.
    if opts.protocol.trim().eq_ignore_ascii_case("http") && !is_loopback {
        return None;
    }

    // 2-4. Environment variables in strict priority order, scoped strictly to github.com,
    // loopback hosts, or an explicitly configured GH_HOST / OZ_GITHUB_API_URL host.
    if env_tokens_allowed_for_host(opts, &bare_host, is_loopback) {
        for (env_name, source) in [
            ("OZ_GITHUB_TOKEN", TokenSource::OzGithubTokenEnv),
            ("GITHUB_TOKEN", TokenSource::GithubTokenEnv),
            ("GH_TOKEN", TokenSource::GhTokenEnv),
        ] {
            if let Some(val) = lookup_env(opts, env_name)
                && let trimmed = val.trim()
                && !trimmed.is_empty()
            {
                return Some(DiscoveredToken {
                    token: trimmed.to_string(),
                    source,
                    host: target_host,
                });
            }
        }
    }

    // 5. GitHub CLI `hosts.yml` (host-matched)
    for path in candidate_gh_hosts_paths(opts) {
        if path.is_file()
            && let Ok(contents) = std::fs::read_to_string(&path)
            && let Some(token) = extract_token_from_gh_hosts_yml(&contents, &target_host)
        {
            return Some(DiscoveredToken {
                token,
                source: TokenSource::GhHostsYml { path },
                host: target_host,
            });
        }
    }

    // 6. `git credential fill` with `GIT_TERMINAL_PROMPT=0` and closed stdin
    if opts.use_git_credential_helper
        && let Some(token) = query_git_credential_fill(
            &opts.protocol,
            &target_host,
            opts.credential_workdir.as_deref(),
            opts.credential_timeout,
        )
    {
        return Some(DiscoveredToken {
            token,
            source: TokenSource::GitCredentialHelper,
            host: target_host,
        });
    }

    None
}

fn env_tokens_allowed_for_host(
    opts: &TokenDiscoveryOptions,
    bare_host: &str,
    is_loopback: bool,
) -> bool {
    if bare_host == "github.com" || is_loopback {
        return true;
    }
    if let Some(gh_host_raw) = lookup_env(opts, "GH_HOST") {
        let trimmed = gh_host_raw.trim();
        if !trimmed.is_empty() {
            let configured_host = if trimmed.contains("://") {
                url::Url::parse(trimmed)
                    .ok()
                    .and_then(|u| u.host_str().map(|h| strip_host_port(h).to_ascii_lowercase()))
            } else {
                Some(
                    strip_host_port(trimmed.trim_end_matches('/'))
                        .to_ascii_lowercase(),
                )
            };
            if configured_host.as_deref() == Some(bare_host) {
                return true;
            }
        }
    }
    if let Some(api_url_raw) = lookup_env(opts, "OZ_GITHUB_API_URL") {
        let trimmed = api_url_raw.trim();
        if !trimmed.is_empty()
            && let Ok(parsed) = url::Url::parse(trimmed)
            && let Some(api_host) = parsed.host_str()
            && strip_host_port(api_host).eq_ignore_ascii_case(bare_host)
        {
            return true;
        }
    }
    false
}

/// Requires a GitHub token or returns an actionable non-interactive [`OzError`].
pub fn require_github_token(
    host: Option<&str>,
    explicit_token: Option<&str>,
) -> Result<DiscoveredToken> {
    let target_host = host.unwrap_or("github.com");
    discover_github_token(Some(target_host), explicit_token).ok_or_else(|| OzError::InvalidUsage {
        message: format!(
            "no GitHub authentication token found for '{target_host}' (checked OZ_GITHUB_TOKEN, GITHUB_TOKEN, GH_TOKEN, gh hosts.yml, and non-interactive git credential helper)"
        ),
        recovery: vec![
            "export GITHUB_TOKEN=<personal-access-token>".to_string(),
            "gh auth login".to_string(),
        ],
    })
}

/// Diagnostic helper inspecting repository remote + non-interactive GitHub authentication state for `oz doctor`.
#[must_use]
pub fn probe_github_auth(
    repo: Option<&OzoneRepo>,
    remote_name: Option<&str>,
    explicit_token: Option<&str>,
) -> GitHubAuthProbe {
    let remote = remote_name.unwrap_or("origin").to_string();
    let parsed_opt = repo.and_then(|r| resolve_repo_remote(r, Some(&remote), None).ok());

    let host = parsed_opt
        .as_ref()
        .and_then(|p| p.host.clone())
        .unwrap_or_else(|| "github.com".to_string());
    let protocol = match parsed_opt.as_ref().map(|p| p.scheme) {
        Some(RemoteScheme::Http) => "http".to_string(),
        _ => "https".to_string(),
    };
    let api_base_url = parsed_opt.as_ref().map_or_else(
        || {
            std::env::var("OZ_GITHUB_API_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map_or_else(
                    || DEFAULT_GITHUB_API_URL.to_string(),
                    |s| s.trim().trim_end_matches('/').to_string(),
                )
        },
        |p| p.api_base_url.clone(),
    );

    let opts = TokenDiscoveryOptions {
        host: host.clone(),
        protocol,
        explicit_token: explicit_token.map(str::to_string),
        credential_workdir: repo.map(|r| r.repo_root.clone()),
        ..TokenDiscoveryOptions::default()
    };
    let discovered = discover_github_token_with_options(&opts);

    let mut hints = Vec::new();
    if discovered.is_none() {
        hints.push(format!(
            "Set GITHUB_TOKEN or OZ_GITHUB_TOKEN, or run `gh auth login --hostname {}`.",
            strip_host_port(&host)
        ));
    }

    GitHubAuthProbe {
        remote_name: remote,
        remote_url: parsed_opt.as_ref().map(|p| p.sanitized_url.clone()),
        host,
        owner: parsed_opt.as_ref().and_then(|p| p.owner.clone()),
        repo: parsed_opt.as_ref().and_then(|p| p.repo.clone()),
        transport: parsed_opt.as_ref().and_then(|p| p.transport),
        api_base_url,
        authenticated: discovered.is_some(),
        token_source: discovered.as_ref().map(|d| d.source.clone()),
        masked_token: discovered.as_ref().map(DiscoveredToken::masked),
        hints,
    }
}

fn lookup_env(opts: &TokenDiscoveryOptions, key: &str) -> Option<String> {
    if let Some(ref map) = opts.env_overrides {
        map.get(key).cloned()
    } else {
        std::env::var(key).ok()
    }
}

fn candidate_gh_hosts_paths(opts: &TokenDiscoveryOptions) -> Vec<PathBuf> {
    if let Some(ref explicit_file) = opts.gh_hosts_file {
        return vec![explicit_file.clone()];
    }

    let mut paths = Vec::new();
    if let Some(gh_dir) = lookup_env(opts, "GH_CONFIG_DIR")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        paths.push(PathBuf::from(gh_dir).join("hosts.yml"));
        return paths;
    }

    if let Some(xdg) = lookup_env(opts, "XDG_CONFIG_HOME")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        paths.push(PathBuf::from(xdg).join("gh").join("hosts.yml"));
    }

    if let Some(home) = lookup_env(opts, "HOME")
        .or_else(|| lookup_env(opts, "USERPROFILE"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        paths.push(
            PathBuf::from(&home)
                .join(".config")
                .join("gh")
                .join("hosts.yml"),
        );
    }

    if let Some(appdata) = lookup_env(opts, "APPDATA")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        paths.push(PathBuf::from(appdata).join("GitHub CLI").join("hosts.yml"));
    }

    paths
}

/// Parses GitHub CLI `hosts.yml` without external YAML dependencies, supporting both top-level
/// `oauth_token` and multi-account `users.<active_user>.oauth_token` layouts.
#[must_use]
pub(crate) fn extract_token_from_gh_hosts_yml(contents: &str, target_host: &str) -> Option<String> {
    #[derive(Default)]
    struct HostEntry {
        oauth_token: Option<String>,
        active_user: Option<String>,
        user_tokens: Vec<(String, String)>,
    }

    let mut hosts: Vec<(String, HostEntry)> = Vec::new();
    let mut current_host_idx: Option<usize> = None;
    let mut in_users_block_indent: Option<usize> = None;
    let mut current_nested_user: Option<(String, usize)> = None;

    for raw_line in contents.lines() {
        let without_comment = strip_yaml_comment(raw_line);
        if without_comment.trim().is_empty() || without_comment.trim() == "---" {
            continue;
        }
        let indent = without_comment
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .map(|c| if c == '\t' { 2 } else { 1 })
            .sum::<usize>();
        let trimmed = without_comment.trim();

        let Some((raw_key, raw_val)) = trimmed.split_once(':') else {
            continue;
        };
        let key = unquote_yaml_scalar(raw_key.trim());
        let val = unquote_yaml_scalar(raw_val.trim());

        if indent == 0 {
            hosts.push((key, HostEntry::default()));
            current_host_idx = Some(hosts.len() - 1);
            in_users_block_indent = None;
            current_nested_user = None;
            continue;
        }

        let Some(h_idx) = current_host_idx else {
            continue;
        };
        let entry = &mut hosts[h_idx].1;

        if let Some(users_indent) = in_users_block_indent
            && indent <= users_indent
        {
            in_users_block_indent = None;
            current_nested_user = None;
        }

        if let Some(users_indent) = in_users_block_indent {
            if let Some((_, nested_user_indent)) = &current_nested_user
                && indent <= *nested_user_indent
            {
                current_nested_user = None;
            }

            if current_nested_user.is_none() && indent > users_indent && val.is_empty() {
                current_nested_user = Some((key, indent));
                continue;
            }

            if let Some((ref uname, nested_user_indent)) = current_nested_user
                && indent > nested_user_indent
                && matches!(key.as_str(), "oauth_token" | "token")
                && !val.is_empty()
            {
                entry.user_tokens.push((uname.clone(), val));
                continue;
            }
        }

        if key == "users" && val.is_empty() {
            in_users_block_indent = Some(indent);
            current_nested_user = None;
        } else if matches!(key.as_str(), "oauth_token" | "token") && !val.is_empty() {
            entry.oauth_token = Some(val);
        } else if key == "user" && !val.is_empty() {
            entry.active_user = Some(val);
        }
    }

    let resolve_entry_token = |entry: &HostEntry| -> Option<String> {
        if let Some(ref tok) = entry.oauth_token {
            return Some(tok.clone());
        }
        if let Some(ref active) = entry.active_user
            && let Some((_, tok)) = entry.user_tokens.iter().find(|(u, _)| u == active)
        {
            return Some(tok.clone());
        }
        entry.user_tokens.first().map(|(_, tok)| tok.clone())
    };

    let target_no_port = strip_host_port(target_host);
    for candidate in [target_host, target_no_port] {
        if let Some((_, entry)) = hosts
            .iter()
            .find(|(h, _)| h.eq_ignore_ascii_case(candidate))
            && let Some(tok) = resolve_entry_token(entry)
        {
            return Some(tok);
        }
    }

    None
}

fn strip_yaml_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_backslash = false;
    for (idx, ch) in line.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single && !prev_backslash => in_double = !in_double,
            '#' if !in_single && !in_double && (idx == 0 || line[..idx].ends_with([' ', '\t'])) => {
                return &line[..idx];
            }
            _ => {}
        }
        prev_backslash = ch == '\\' && !prev_backslash;
    }
    line
}

fn unquote_yaml_scalar(s: &str) -> String {
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        return s[1..s.len() - 1].to_string();
    }
    s.to_string()
}

/// Invokes `git credential fill` strictly non-interactively (`GIT_TERMINAL_PROMPT=0`,
/// `GCM_INTERACTIVE=never`, closed stdin, bounded timeout). Never blocks on a TTY prompt.
#[must_use]
pub fn query_git_credential_fill(
    protocol: &str,
    host: &str,
    workdir: Option<&Path>,
    timeout: Duration,
) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "credential.interactive=never", "credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    if let Some(dir) = workdir
        && dir.is_dir()
    {
        cmd.current_dir(dir);
    }

    let mut child = cmd.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let payload = format!("protocol={protocol}\nhost={host}\n\n");
        let _ = stdin.write_all(payload.as_bytes());
        let _ = stdin.flush();
        // Drop `stdin` immediately so any helper reading stdin sees EOF right away.
    }

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    use std::io::Read;
                    let _ = stdout.read_to_string(&mut out);
                }
                for line in out.lines() {
                    if let Some(pw) = line.strip_prefix("password=") {
                        let trimmed = pw.trim();
                        if !trimmed.is_empty() {
                            return Some(trimmed.to_string());
                        }
                    }
                }
                return None;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn mask_secret(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 8 {
        "****".to_string()
    } else {
        let prefix: String = chars[..4].iter().collect();
        format!("{prefix}****")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parses_github_https_ssh_ghe_and_local_remotes() {
        let https = parse_git_remote_with_env("https://github.com/ozone-vcs/oz.git", None, None)
            .expect("parse https");
        assert_eq!(https.scheme, RemoteScheme::Https);
        assert_eq!(https.transport, Some(GitTransportKind::Https));
        assert_eq!(https.host.as_deref(), Some("github.com"));
        assert_eq!(https.owner.as_deref(), Some("ozone-vcs"));
        assert_eq!(https.repo.as_deref(), Some("oz"));
        assert_eq!(https.api_base_url, "https://api.github.com");

        let scp = parse_git_remote_with_env("git@github.com:ozone-vcs/oz.git", None, None)
            .expect("parse scp ssh");
        assert_eq!(scp.scheme, RemoteScheme::Ssh);
        assert_eq!(scp.transport, Some(GitTransportKind::Ssh));
        assert_eq!(scp.host.as_deref(), Some("github.com"));
        assert_eq!(scp.owner.as_deref(), Some("ozone-vcs"));
        assert_eq!(scp.repo.as_deref(), Some("oz"));
        assert_eq!(scp.api_base_url, "https://api.github.com");

        let ssh_url =
            parse_git_remote_with_env("ssh://git@ghe.acme.internal:2222/platform/core", None, None)
                .expect("parse ssh://");
        assert_eq!(ssh_url.scheme, RemoteScheme::Ssh);
        assert_eq!(ssh_url.transport, Some(GitTransportKind::Ssh));
        assert_eq!(ssh_url.host.as_deref(), Some("ghe.acme.internal"));
        assert_eq!(ssh_url.owner.as_deref(), Some("platform"));
        assert_eq!(ssh_url.repo.as_deref(), Some("core"));
        assert_eq!(ssh_url.api_base_url, "https://ghe.acme.internal/api/v3");

        let mock_override = parse_git_remote_with_env(
            "https://github.com/acme/demo.git",
            None,
            Some("http://127.0.0.1:9999/"),
        )
        .expect("parse with env override");
        assert_eq!(mock_override.api_base_url, "http://127.0.0.1:9999");

        let local = parse_git_remote_with_env("/tmp/local-bare.git", None, None).expect("local");
        assert_eq!(local.scheme, RemoteScheme::File);
        assert_eq!(local.transport, None);
    }

    #[test]
    fn token_discovery_respects_strict_priority_order_and_hosts_yml() {
        let tmp = TempDir::new().expect("tempdir");
        let hosts_yml = tmp.path().join("hosts.yml");
        std::fs::write(
            &hosts_yml,
            r#"# gh cli hosts config
github.com:
    user: octocat
    git_protocol: https
    users:
        octocat:
            oauth_token: gho_from_nested_users_123456
ghe.corp.local:
    oauth_token: "ghp_enterprise_token_999999"
    user: alice
"#,
        )
        .expect("write hosts.yml");

        let mut env = BTreeMap::new();
        env.insert("GH_TOKEN".to_string(), "gh_token_val".to_string());
        env.insert("GITHUB_TOKEN".to_string(), "github_token_val".to_string());
        env.insert(
            "OZ_GITHUB_TOKEN".to_string(),
            "oz_github_token_val".to_string(),
        );

        // 1. Explicit wins over everything
        let d1 = discover_github_token_with_options(&TokenDiscoveryOptions {
            host: "github.com".to_string(),
            explicit_token: Some("explicit_tok".to_string()),
            env_overrides: Some(env.clone()),
            gh_hosts_file: Some(hosts_yml.clone()),
            use_git_credential_helper: false,
            ..Default::default()
        })
        .expect("explicit");
        assert_eq!(d1.token, "explicit_tok");
        assert_eq!(d1.source, TokenSource::Explicit);

        // 2. OZ_GITHUB_TOKEN wins over GITHUB_TOKEN & GH_TOKEN
        let d2 = discover_github_token_with_options(&TokenDiscoveryOptions {
            host: "github.com".to_string(),
            env_overrides: Some(env.clone()),
            gh_hosts_file: Some(hosts_yml.clone()),
            use_git_credential_helper: false,
            ..Default::default()
        })
        .expect("oz_github_token");
        assert_eq!(d2.token, "oz_github_token_val");
        assert_eq!(d2.source, TokenSource::OzGithubTokenEnv);

        // 3. GITHUB_TOKEN wins over GH_TOKEN
        env.remove("OZ_GITHUB_TOKEN");
        let d3 = discover_github_token_with_options(&TokenDiscoveryOptions {
            host: "github.com".to_string(),
            env_overrides: Some(env.clone()),
            gh_hosts_file: Some(hosts_yml.clone()),
            use_git_credential_helper: false,
            ..Default::default()
        })
        .expect("github_token");
        assert_eq!(d3.token, "github_token_val");
        assert_eq!(d3.source, TokenSource::GithubTokenEnv);

        // 4. GH_TOKEN wins over hosts.yml
        env.remove("GITHUB_TOKEN");
        let d4 = discover_github_token_with_options(&TokenDiscoveryOptions {
            host: "github.com".to_string(),
            env_overrides: Some(env.clone()),
            gh_hosts_file: Some(hosts_yml.clone()),
            use_git_credential_helper: false,
            ..Default::default()
        })
        .expect("gh_token");
        assert_eq!(d4.token, "gh_token_val");
        assert_eq!(d4.source, TokenSource::GhTokenEnv);

        // 5. hosts.yml nested user token + custom GHE host token
        env.remove("GH_TOKEN");
        let d5 = discover_github_token_with_options(&TokenDiscoveryOptions {
            host: "github.com".to_string(),
            env_overrides: Some(env.clone()),
            gh_hosts_file: Some(hosts_yml.clone()),
            use_git_credential_helper: false,
            ..Default::default()
        })
        .expect("hosts.yml github.com");
        assert_eq!(d5.token, "gho_from_nested_users_123456");
        assert_eq!(
            d5.source,
            TokenSource::GhHostsYml {
                path: hosts_yml.clone()
            }
        );

        let d6 = discover_github_token_with_options(&TokenDiscoveryOptions {
            host: "ghe.corp.local".to_string(),
            env_overrides: Some(env),
            gh_hosts_file: Some(hosts_yml),
            use_git_credential_helper: false,
            ..Default::default()
        })
        .expect("hosts.yml ghe");
        assert_eq!(d6.token, "ghp_enterprise_token_999999");
    }

    #[test]
    fn env_tokens_are_host_scoped_and_never_autodiscovered_over_non_loopback_http() {
        let tmp = TempDir::new().expect("tempdir");
        let hosts_yml = tmp.path().join("hosts.yml");
        std::fs::write(
            &hosts_yml,
            "github.com:\n    oauth_token: gho_github_hosts_tok\ngitlab.example.com:\n    oauth_token: glpat_gitlab_hosts_tok\n",
        )
        .expect("write hosts.yml");

        let empty_hosts = tmp.path().join("empty_hosts.yml");
        std::fs::write(&empty_hosts, "").expect("write empty hosts.yml");

        let mut base_env = BTreeMap::new();
        base_env.insert("OZ_GITHUB_TOKEN".to_string(), "oz_tok".to_string());
        base_env.insert("GITHUB_TOKEN".to_string(), "gh_actions_tok".to_string());
        base_env.insert("GH_TOKEN".to_string(), "gh_cli_tok".to_string());

        // 1. Non-GitHub HTTPS host (`https://gitlab.example.com`) MUST NOT receive GitHub env vars!
        let gitlab_none = discover_github_token_with_options(&TokenDiscoveryOptions {
            host: "gitlab.example.com".to_string(),
            protocol: "https".to_string(),
            env_overrides: Some(base_env.clone()),
            gh_hosts_file: Some(empty_hosts.clone()),
            use_git_credential_helper: false,
            ..Default::default()
        });
        assert!(
            gitlab_none.is_none(),
            "GitHub env tokens must not leak to https://gitlab.example.com"
        );

        // And when `hosts.yml` has a host-specific entry for `gitlab.example.com`, discovery
        // skips env vars 2-4 and falls through to step 5 (`hosts.yml`).
        let gitlab_hosts = discover_github_token_with_options(&TokenDiscoveryOptions {
            host: "gitlab.example.com".to_string(),
            protocol: "https".to_string(),
            env_overrides: Some(base_env.clone()),
            gh_hosts_file: Some(hosts_yml.clone()),
            use_git_credential_helper: false,
            ..Default::default()
        })
        .expect("host-matched hosts.yml token");
        assert_eq!(gitlab_hosts.token, "glpat_gitlab_hosts_tok");
        assert_eq!(
            gitlab_hosts.source,
            TokenSource::GhHostsYml {
                path: hosts_yml.clone()
            }
        );

        // 2. Non-loopback cleartext HTTP (`http://github.com`) MUST NOT auto-discover any token!
        let cleartext_gh = discover_github_token_with_options(&TokenDiscoveryOptions {
            host: "github.com".to_string(),
            protocol: "http".to_string(),
            env_overrides: Some(base_env.clone()),
            gh_hosts_file: Some(hosts_yml.clone()),
            use_git_credential_helper: false,
            ..Default::default()
        });
        assert!(
            cleartext_gh.is_none(),
            "tokens must never be auto-discovered over cleartext http://github.com"
        );

        // 3. `https://github.com` DOES return OZ_GITHUB_TOKEN / GITHUB_TOKEN / GH_TOKEN.
        let https_gh = discover_github_token_with_options(&TokenDiscoveryOptions {
            host: "github.com".to_string(),
            protocol: "https".to_string(),
            env_overrides: Some(base_env.clone()),
            gh_hosts_file: Some(empty_hosts.clone()),
            use_git_credential_helper: false,
            ..Default::default()
        })
        .expect("https github.com token");
        assert_eq!(https_gh.token, "oz_tok");
        assert_eq!(https_gh.source, TokenSource::OzGithubTokenEnv);

        // 4. Loopback `http://127.0.0.1:<port>`, `localhost:<port>`, and `[::1]:<port>` DO return env tokens.
        for loopback_host in ["127.0.0.1:41234", "localhost:8080", "[::1]:9000", "::1"] {
            let lb = discover_github_token_with_options(&TokenDiscoveryOptions {
                host: loopback_host.to_string(),
                protocol: "http".to_string(),
                env_overrides: Some(base_env.clone()),
                gh_hosts_file: Some(empty_hosts.clone()),
                use_git_credential_helper: false,
                ..Default::default()
            })
            .expect("loopback http token");
            assert_eq!(lb.token, "oz_tok", "failed for {loopback_host}");
        }

        // 5. Explicitly configured `GH_HOST` enables env tokens ONLY for that matching host.
        let mut gh_host_env = base_env.clone();
        gh_host_env.remove("OZ_GITHUB_TOKEN");
        gh_host_env.insert("GH_HOST".to_string(), "ghe.corp.internal".to_string());
        let ghe_by_gh_host = discover_github_token_with_options(&TokenDiscoveryOptions {
            host: "ghe.corp.internal:8443".to_string(),
            protocol: "https".to_string(),
            env_overrides: Some(gh_host_env.clone()),
            gh_hosts_file: Some(empty_hosts.clone()),
            use_git_credential_helper: false,
            ..Default::default()
        })
        .expect("GH_HOST matched");
        assert_eq!(ghe_by_gh_host.token, "gh_actions_tok");
        assert_eq!(ghe_by_gh_host.source, TokenSource::GithubTokenEnv);

        let wrong_ghe = discover_github_token_with_options(&TokenDiscoveryOptions {
            host: "other.corp.internal".to_string(),
            protocol: "https".to_string(),
            env_overrides: Some(gh_host_env),
            gh_hosts_file: Some(empty_hosts.clone()),
            use_git_credential_helper: false,
            ..Default::default()
        });
        assert!(wrong_ghe.is_none());

        // 6. Explicitly configured `OZ_GITHUB_API_URL` enables env tokens ONLY for that API host.
        let mut api_env = base_env;
        api_env.remove("OZ_GITHUB_TOKEN");
        api_env.remove("GITHUB_TOKEN");
        api_env.insert(
            "OZ_GITHUB_API_URL".to_string(),
            "https://git.enterprise.example/api/v3".to_string(),
        );
        let ghe_by_api_url = discover_github_token_with_options(&TokenDiscoveryOptions {
            host: "git.enterprise.example".to_string(),
            protocol: "https".to_string(),
            env_overrides: Some(api_env.clone()),
            gh_hosts_file: Some(empty_hosts.clone()),
            use_git_credential_helper: false,
            ..Default::default()
        })
        .expect("OZ_GITHUB_API_URL matched");
        assert_eq!(ghe_by_api_url.token, "gh_cli_tok");
        assert_eq!(ghe_by_api_url.source, TokenSource::GhTokenEnv);

        let unmatched_api = discover_github_token_with_options(&TokenDiscoveryOptions {
            host: "gitlab.enterprise.example".to_string(),
            protocol: "https".to_string(),
            env_overrides: Some(api_env),
            gh_hosts_file: Some(empty_hosts),
            use_git_credential_helper: false,
            ..Default::default()
        });
        assert!(unmatched_api.is_none());
    }
}
