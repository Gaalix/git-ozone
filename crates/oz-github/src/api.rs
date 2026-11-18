// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-Rust GitHub REST & GraphQL client (`reqwest` + `rustls`) supporting GitHub.com,
//! GitHub Enterprise (`OZ_GITHUB_API_URL`), and local mock servers.

use crate::PullRequestSpec;
use crate::auth::{
    DEFAULT_GITHUB_API_URL, ParsedGitRemote, RemoteScheme, TokenDiscoveryOptions,
    discover_github_token_with_options, resolve_repo_remote,
};
use crate::wire::run_blocking_safe;
use http::StatusCode;
use oz_core::{OzError, OzoneRepo, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const GITHUB_API_VERSION: &str = "2022-11-28";
const CLIENT_USER_AGENT: &str = concat!("ozone-vcs/", env!("CARGO_PKG_VERSION"));

/// Branch pointer on a GitHub Pull Request (`head` or `base`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubPrRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// GitHub Pull Request representation returned by REST endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubPullRequest {
    pub number: u64,
    #[serde(default)]
    pub html_url: String,
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub merged: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_commit_sha: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub head: GitHubPrRef,
    pub base: GitHubPrRef,
}

impl GitHubPullRequest {
    /// Returns `true` when GitHub reports this PR as merged (`merged == true` or `merged_at.is_some()`).
    #[must_use]
    pub fn is_merged(&self) -> bool {
        self.merged || self.merged_at.is_some()
    }
}

/// Partial update payload for `PATCH /repos/{owner}/{repo}/pulls/{number}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePullRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// Output details attached to a GitHub Check Run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubCheckRunOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Single GitHub Check Run from `GET /repos/{owner}/{repo}/commits/{ref}/check-runs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubCheckRun {
    pub id: u64,
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
    #[serde(default)]
    pub output: GitHubCheckRunOutput,
}

#[derive(Debug, Deserialize)]
struct CheckRunsEnvelope {
    #[serde(default)]
    check_runs: Vec<GitHubCheckRun>,
}

/// GitHub Actions workflow run summary from `GET /repos/{owner}/{repo}/actions/runs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubWorkflowRun {
    pub id: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub head_branch: Option<String>,
    pub head_sha: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkflowRunsEnvelope {
    #[serde(default)]
    workflow_runs: Vec<GitHubWorkflowRun>,
}

/// Single step inside a GitHub Actions job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubJobStep {
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub number: u32,
}

/// GitHub Actions job from `GET /repos/{owner}/{repo}/actions/runs/{run_id}/jobs` or `/actions/jobs/{job_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubActionsJob {
    pub id: u64,
    #[serde(default)]
    pub run_id: Option<u64>,
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
    #[serde(default)]
    pub steps: Vec<GitHubJobStep>,
}

#[derive(Debug, Deserialize)]
struct ActionsJobsEnvelope {
    #[serde(default)]
    jobs: Vec<GitHubActionsJob>,
}

/// Pure-Rust GitHub API client supporting both blocking and async callers safely.
#[derive(Debug, Clone)]
pub struct GitHubClient {
    api_base_url: String,
    token: Option<String>,
    timeout: Duration,
}

impl GitHubClient {
    /// Creates a new GitHub API client for `api_base_url` (defaults to `https://api.github.com` if empty).
    #[must_use]
    pub fn new(api_base_url: impl Into<String>, token: Option<String>) -> Self {
        let raw = api_base_url.into();
        let trimmed = raw.trim().trim_end_matches('/');
        let base = if trimmed.is_empty() {
            DEFAULT_GITHUB_API_URL.to_string()
        } else {
            trimmed.to_string()
        };
        Self {
            api_base_url: base,
            token: token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
            timeout: Duration::from_secs(30),
        }
    }

    /// Resolves a repository's remote and non-interactive token and constructs a `(GitHubClient, ParsedGitRemote)`.
    pub fn for_repo(
        repo: &OzoneRepo,
        remote_name: Option<&str>,
        explicit_token: Option<&str>,
        api_url_override: Option<&str>,
    ) -> Result<(Self, ParsedGitRemote)> {
        let parsed = resolve_repo_remote(repo, remote_name, api_url_override)?;
        let protocol = match parsed.scheme {
            RemoteScheme::Http => "http",
            _ => "https",
        };
        let host = parsed
            .host
            .clone()
            .unwrap_or_else(|| "github.com".to_string());
        let discovered = discover_github_token_with_options(&TokenDiscoveryOptions {
            host,
            protocol: protocol.to_string(),
            explicit_token: explicit_token.map(str::to_string),
            credential_workdir: Some(repo.repo_root.clone()),
            ..Default::default()
        });
        let client = Self::new(
            parsed.api_base_url.clone(),
            discovered.map(|d| d.token),
        );
        Ok((client, parsed))
    }

    /// Returns the resolved base URL (without trailing slash).
    #[must_use]
    pub fn api_base_url(&self) -> &str {
        &self.api_base_url
    }

    /// Returns `true` if a bearer token is configured on this client.
    #[must_use]
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// Returns the token if present, or an actionable [`OzError::InvalidUsage`] when missing.
    pub fn require_authenticated(&self) -> Result<&str> {
        self.token.as_deref().ok_or_else(|| OzError::InvalidUsage {
            message: format!(
                "GitHub authentication token required for API calls to '{}'",
                self.api_base_url
            ),
            recovery: vec![
                "export GITHUB_TOKEN=<personal-access-token>".to_string(),
                "gh auth login".to_string(),
            ],
        })
    }

    /// Lists pull requests (`GET /repos/{owner}/{repo}/pulls`).
    pub fn list_pull_requests(
        &self,
        owner: &str,
        repo: &str,
        state: Option<&str>,
        head: Option<&str>,
        base: Option<&str>,
    ) -> Result<Vec<GitHubPullRequest>> {
        let mut params = vec![("per_page".to_string(), "100".to_string())];
        if let Some(st) = state {
            params.push(("state".to_string(), st.to_string()));
        }
        if let Some(hd) = head {
            params.push(("head".to_string(), hd.to_string()));
        }
        if let Some(bs) = base {
            params.push(("base".to_string(), bs.to_string()));
        }
        let path = format!("/repos/{owner}/{repo}/pulls");
        self.get_json_with_query(&path, &params)
    }

    /// Fetches a single pull request (`GET /repos/{owner}/{repo}/pulls/{number}`).
    pub fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<GitHubPullRequest> {
        let path = format!("/repos/{owner}/{repo}/pulls/{number}");
        self.get_json_with_query(&path, &[])
    }

    /// Creates a new pull request (`POST /repos/{owner}/{repo}/pulls`).
    pub fn create_pull_request(
        &self,
        owner: &str,
        repo: &str,
        spec: &PullRequestSpec,
    ) -> Result<GitHubPullRequest> {
        #[derive(Serialize)]
        struct CreatePayload<'a> {
            title: &'a str,
            body: &'a str,
            head: &'a str,
            base: &'a str,
            draft: bool,
        }

        let payload = CreatePayload {
            title: &spec.title,
            body: &spec.body,
            head: &spec.head_branch,
            base: &spec.base_branch,
            draft: spec.draft,
        };
        let path = format!("/repos/{owner}/{repo}/pulls");
        self.send_json(http::Method::POST, &path, &payload)
    }

    /// Updates an existing pull request (`PATCH /repos/{owner}/{repo}/pulls/{number}`).
    pub fn update_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        patch: &UpdatePullRequest,
    ) -> Result<GitHubPullRequest> {
        let path = format!("/repos/{owner}/{repo}/pulls/{number}");
        self.send_json(http::Method::PATCH, &path, patch)
    }

    /// Retargets a pull request's `base` branch (`PATCH /repos/{owner}/{repo}/pulls/{number}`).
    pub fn retarget_pull_request_base(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        new_base: &str,
    ) -> Result<GitHubPullRequest> {
        self.update_pull_request(
            owner,
            repo,
            number,
            &UpdatePullRequest {
                base: Some(new_base.to_string()),
                ..Default::default()
            },
        )
    }

    /// Executes a GraphQL query against `<api_base_url>/graphql` (or GHE `/api/graphql`).
    pub fn graphql(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value> {
        #[derive(Serialize)]
        struct GraphQlBody<'a> {
            query: &'a str,
            variables: serde_json::Value,
        }

        let endpoint = if let Some(ghe_root) = self.api_base_url.strip_suffix("/api/v3") {
            format!("{ghe_root}/api/graphql")
        } else {
            format!("{}/graphql", self.api_base_url)
        };

        let body_val = serde_json::to_value(GraphQlBody { query, variables })?;
        let this = self.clone();
        run_blocking_safe(move || {
            let http_client = this.build_blocking_client()?;
            let mut req = http_client
                .post(&endpoint)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
                .header("User-Agent", CLIENT_USER_AGENT)
                .json(&body_val);
            if let Some(ref tok) = this.token {
                req = req.bearer_auth(tok);
            }
            let resp = req
                .send()
                .map_err(|err| OzError::Other(format!("GitHub GraphQL request failed: {err}")))?;
            decode_json_response(resp, "POST /graphql")
        })
    }

    /// Lists commit check-runs (`GET /repos/{owner}/{repo}/commits/{git_ref}/check-runs`).
    pub fn list_check_runs_for_ref(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<Vec<GitHubCheckRun>> {
        let encoded_ref = encode_path_segment(git_ref);
        let path = format!("/repos/{owner}/{repo}/commits/{encoded_ref}/check-runs");
        let params = vec![("per_page".to_string(), "100".to_string())];
        let env: CheckRunsEnvelope = self.get_json_with_query(&path, &params)?;
        Ok(env.check_runs)
    }

    /// Lists GitHub Actions workflow runs (`GET /repos/{owner}/{repo}/actions/runs`).
    pub fn list_workflow_runs(
        &self,
        owner: &str,
        repo: &str,
        branch: Option<&str>,
        head_sha: Option<&str>,
    ) -> Result<Vec<GitHubWorkflowRun>> {
        let mut params = vec![("per_page".to_string(), "30".to_string())];
        if let Some(br) = branch {
            params.push(("branch".to_string(), br.to_string()));
        }
        if let Some(sha) = head_sha {
            params.push(("head_sha".to_string(), sha.to_string()));
        }
        let path = format!("/repos/{owner}/{repo}/actions/runs");
        let env: WorkflowRunsEnvelope = self.get_json_with_query(&path, &params)?;
        Ok(env.workflow_runs)
    }

    /// Lists jobs for a GitHub Actions workflow run (`GET /repos/{owner}/{repo}/actions/runs/{run_id}/jobs`).
    pub fn list_jobs_for_workflow_run(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
    ) -> Result<Vec<GitHubActionsJob>> {
        let path = format!("/repos/{owner}/{repo}/actions/runs/{run_id}/jobs");
        let params = vec![("per_page".to_string(), "100".to_string())];
        let env: ActionsJobsEnvelope = self.get_json_with_query(&path, &params)?;
        Ok(env.jobs)
    }

    /// Fetches a single GitHub Actions job (`GET /repos/{owner}/{repo}/actions/jobs/{job_id}`).
    pub fn get_actions_job(
        &self,
        owner: &str,
        repo: &str,
        job_id: u64,
    ) -> Result<GitHubActionsJob> {
        let path = format!("/repos/{owner}/{repo}/actions/jobs/{job_id}");
        self.get_json_with_query(&path, &[])
    }

    /// Downloads raw logs (plain UTF-8 text or PKZIP archive) for a GitHub Actions job
    /// (`GET /repos/{owner}/{repo}/actions/jobs/{job_id}/logs`).
    pub fn download_job_logs(
        &self,
        owner: &str,
        repo: &str,
        job_id: u64,
    ) -> Result<Vec<u8>> {
        let url = format!("{}/repos/{owner}/{repo}/actions/jobs/{job_id}/logs", self.api_base_url);
        let this = self.clone();
        run_blocking_safe(move || {
            let http_client = this.build_blocking_client()?;
            let mut req = http_client
                .get(&url)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
                .header("User-Agent", CLIENT_USER_AGENT);
            if let Some(ref tok) = this.token {
                req = req.bearer_auth(tok);
            }
            let resp = req
                .send()
                .map_err(|err| OzError::Other(format!("failed downloading job {job_id} logs: {err}")))?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().unwrap_or_default();
                return Err(map_http_status_error(status, &format!("GET /actions/jobs/{job_id}/logs"), &body));
            }
            let bytes = resp
                .bytes()
                .map_err(|err| OzError::Other(format!("failed reading job {job_id} log bytes: {err}")))?;
            Ok(bytes.to_vec())
        })
    }

    fn build_blocking_client(&self) -> Result<reqwest::blocking::Client> {
        reqwest::blocking::Client::builder()
            .timeout(self.timeout)
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|err| OzError::Other(format!("failed building rustls HTTP client: {err}")))
    }

    fn get_json_with_query<T>(&self, path: &str, query_params: &[(String, String)]) -> Result<T>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let mut full_url = url::Url::parse(&format!("{}{path}", self.api_base_url)).map_err(|err| {
            OzError::InvalidUsage {
                message: format!("invalid GitHub API URL '{}{path}': {err}", self.api_base_url),
                recovery: Vec::new(),
            }
        })?;
        if !query_params.is_empty() {
            let mut pairs = full_url.query_pairs_mut();
            for (k, v) in query_params {
                pairs.append_pair(k, v);
            }
        }
        let endpoint = full_url.to_string();
        let label = format!("GET {path}");
        let this = self.clone();

        run_blocking_safe(move || {
            let http_client = this.build_blocking_client()?;
            let mut req = http_client
                .get(&endpoint)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
                .header("User-Agent", CLIENT_USER_AGENT);
            if let Some(ref tok) = this.token {
                req = req.bearer_auth(tok);
            }
            let resp = req
                .send()
                .map_err(|err| OzError::Other(format!("GitHub API request {label} failed: {err}")))?;
            decode_json_response(resp, &label)
        })
    }

    fn send_json<B, T>(&self, method: http::Method, path: &str, body: &B) -> Result<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned + Send + 'static,
    {
        let payload = serde_json::to_value(body)?;
        let endpoint = format!("{}{path}", self.api_base_url);
        let label = format!("{method} {path}");
        let this = self.clone();

        run_blocking_safe(move || {
            let http_client = this.build_blocking_client()?;
            let mut req = http_client
                .request(method, &endpoint)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
                .header("User-Agent", CLIENT_USER_AGENT)
                .json(&payload);
            if let Some(ref tok) = this.token {
                req = req.bearer_auth(tok);
            }
            let resp = req
                .send()
                .map_err(|err| OzError::Other(format!("GitHub API request {label} failed: {err}")))?;
            decode_json_response(resp, &label)
        })
    }
}

fn decode_json_response<T: DeserializeOwned>(
    resp: reqwest::blocking::Response,
    label: &str,
) -> Result<T> {
    let status = resp.status();
    let raw = resp
        .text()
        .map_err(|err| OzError::Other(format!("failed reading {label} response body: {err}")))?;
    if !status.is_success() {
        return Err(map_http_status_error(status, label, &raw));
    }
    serde_json::from_str(&raw).map_err(|err| {
        OzError::Other(format!(
            "failed parsing JSON response from {label}: {err} (body: {})",
            truncate_str(&raw, 240)
        ))
    })
}

fn map_http_status_error(status: StatusCode, label: &str, raw_body: &str) -> OzError {
    let detail = serde_json::from_str::<serde_json::Value>(raw_body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| truncate_str(raw_body.trim(), 200));

    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => OzError::CheckFailed {
            message: format!(
                "GitHub API authentication/authorization failed ({status}) on {label}: {detail}"
            ),
            recovery: vec![
                "export GITHUB_TOKEN=<personal-access-token>".to_string(),
                "gh auth login".to_string(),
            ],
        },
        StatusCode::NOT_FOUND => OzError::CheckFailed {
            message: format!("GitHub resource not found (404) on {label}: {detail}"),
            recovery: Vec::new(),
        },
        StatusCode::UNPROCESSABLE_ENTITY => OzError::CheckFailed {
            message: format!("GitHub validation failed (422) on {label}: {detail}"),
            recovery: Vec::new(),
        },
        _ => OzError::Other(format!("GitHub API error ({status}) on {label}: {detail}")),
    }
}

fn encode_path_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}
