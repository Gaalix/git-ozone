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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubPrRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Minimal GitHub user object embedded in PRs and reviews.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubUser {
    pub login: String,
}

/// GitHub Pull Request representation returned by REST endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<GitHubUser>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mergeable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mergeable_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_files: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// Changed file entry returned by `GET /repos/{owner}/{repo}/pulls/{number}/files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubPullRequestFile {
    pub filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_filename: Option<String>,
    pub status: String,
    #[serde(default)]
    pub additions: u32,
    #[serde(default)]
    pub deletions: u32,
    #[serde(default)]
    pub changes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
}

/// Pull request review returned by `GET/POST /repos/{owner}/{repo}/pulls/{number}/reviews`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubPullRequestReview {
    pub id: u64,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<GitHubUser>,
}

/// Pull request review comment from `GET /repos/{owner}/{repo}/pulls/{number}/comments`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubPullRequestComment {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request_review_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to_id: Option<u64>,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_commit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_hunk: Option<String>,
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<GitHubUser>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
}

/// Top-level issue or pull request conversation comment from `GET /repos/{owner}/{repo}/issues/{number}/comments`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubIssueComment {
    pub id: u64,
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<GitHubUser>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
}

/// GraphQL review thread resolution/outdated metadata keyed by REST comment `databaseId`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubReviewThreadMeta {
    pub thread_id: String,
    pub is_resolved: bool,
    pub is_outdated: bool,
    #[serde(default)]
    pub comment_ids: Vec<u64>,
}

/// Label attached to a GitHub issue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubLabel {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Milestone attached to a GitHub issue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubMilestone {
    pub number: u64,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// Marker object present on `GitHubIssue` when the issue number refers to a pull request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubIssuePullRequestRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
}

/// GitHub Issue returned by `GET /repos/{owner}/{repo}/issues/{number}` (REST, avoids deprecated GraphQL `projectCards`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubIssue {
    pub number: u64,
    #[serde(default)]
    pub html_url: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_reason: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<GitHubUser>,
    #[serde(default)]
    pub labels: Vec<GitHubLabel>,
    #[serde(default)]
    pub assignees: Vec<GitHubUser>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<GitHubMilestone>,
    #[serde(default)]
    pub comments: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<GitHubIssuePullRequestRef>,
}

/// GitHub Actions workflow definition from `GET /repos/{owner}/{repo}/actions/workflows`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubWorkflow {
    pub id: u64,
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub state: String,
}

#[derive(Debug, Deserialize)]
struct WorkflowsEnvelope {
    #[serde(default)]
    workflows: Vec<GitHubWorkflow>,
}

/// Single inline comment entry inside [`CreatePullRequestReviewPayload`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubReviewCommentPayload {
    pub path: String,
    pub line: u32,
    pub side: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<String>,
    pub body: String,
}

/// Request body for `POST /repos/{owner}/{repo}/pulls/{number}/reviews`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePullRequestReviewPayload {
    pub commit_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub event: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<GitHubReviewCommentPayload>,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
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

/// Outcome of a conditional HTTP GET supporting `If-None-Match` / `ETag` and rate-limit backoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConditionalGet<T> {
    /// Server returned `304 Not Modified` (zero GitHub REST rate-limit cost).
    NotModified {
        etag: Option<String>,
        retry_after_secs: Option<u64>,
    },
    /// Server returned `200 OK` with a fresh JSON body and optional `ETag`.
    Modified {
        value: T,
        etag: Option<String>,
        retry_after_secs: Option<u64>,
    },
    /// Server returned `429` or rate-limited `403` with a suggested backoff duration.
    RateLimited {
        retry_after_secs: u64,
        message: String,
    },
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

    /// Conditionally fetches a single pull request with `If-None-Match` (`ETag`) support.
    pub fn get_pull_request_conditional(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        if_none_match: Option<&str>,
    ) -> Result<ConditionalGet<GitHubPullRequest>> {
        let path = format!("/repos/{owner}/{repo}/pulls/{number}");
        self.get_json_conditional(&path, &[], if_none_match)
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

    /// Lists all changed files in a pull request (`GET /repos/{owner}/{repo}/pulls/{number}/files`),
    /// paginating automatically up to GitHub's 3,000-file limit.
    pub fn list_pull_request_files(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<GitHubPullRequestFile>> {
        let path = format!("/repos/{owner}/{repo}/pulls/{number}/files");
        let mut all = Vec::new();
        for page in 1u32..=30 {
            let batch: Vec<GitHubPullRequestFile> = self.get_json_with_query(
                &path,
                &[
                    ("per_page".to_string(), "100".to_string()),
                    ("page".to_string(), page.to_string()),
                ],
            )?;
            let count = batch.len();
            all.extend(batch);
            if count < 100 {
                break;
            }
        }
        Ok(all)
    }

    /// Lists reviews on a pull request (`GET /repos/{owner}/{repo}/pulls/{number}/reviews`),
    /// paginating automatically.
    pub fn list_pull_request_reviews(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<GitHubPullRequestReview>> {
        let path = format!("/repos/{owner}/{repo}/pulls/{number}/reviews");
        let mut all = Vec::new();
        for page in 1u32..=10 {
            let batch: Vec<GitHubPullRequestReview> = self.get_json_with_query(
                &path,
                &[
                    ("per_page".to_string(), "100".to_string()),
                    ("page".to_string(), page.to_string()),
                ],
            )?;
            let count = batch.len();
            all.extend(batch);
            if count < 100 {
                break;
            }
        }
        Ok(all)
    }

    /// Lists inline review comments on a pull request (`GET /repos/{owner}/{repo}/pulls/{number}/comments`),
    /// paginating automatically.
    pub fn list_pull_request_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<GitHubPullRequestComment>> {
        let path = format!("/repos/{owner}/{repo}/pulls/{number}/comments");
        let mut all = Vec::new();
        for page in 1u32..=10 {
            let batch: Vec<GitHubPullRequestComment> = self.get_json_with_query(
                &path,
                &[
                    ("per_page".to_string(), "100".to_string()),
                    ("page".to_string(), page.to_string()),
                ],
            )?;
            let count = batch.len();
            all.extend(batch);
            if count < 100 {
                break;
            }
        }
        Ok(all)
    }

    /// Lists top-level issue / PR conversation comments (`GET /repos/{owner}/{repo}/issues/{number}/comments`),
    /// paginating automatically.
    pub fn list_issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<GitHubIssueComment>> {
        let path = format!("/repos/{owner}/{repo}/issues/{number}/comments");
        let mut all = Vec::new();
        for page in 1u32..=10 {
            let batch: Vec<GitHubIssueComment> = self.get_json_with_query(
                &path,
                &[
                    ("per_page".to_string(), "100".to_string()),
                    ("page".to_string(), page.to_string()),
                ],
            )?;
            let count = batch.len();
            all.extend(batch);
            if count < 100 {
                break;
            }
        }
        Ok(all)
    }

    /// Fetches a single GitHub issue via REST (`GET /repos/{owner}/{repo}/issues/{number}`),
    /// avoiding deprecated classic GraphQL `projectCards` fields.
    pub fn get_issue(&self, owner: &str, repo: &str, number: u64) -> Result<GitHubIssue> {
        let path = format!("/repos/{owner}/{repo}/issues/{number}");
        self.get_json_with_query(&path, &[])
    }

    /// Best-effort GraphQL lookup for PR `reviewThreads` (`isResolved`, `isOutdated`, thread IDs).
    pub fn list_pull_request_review_threads_graphql(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<GitHubReviewThreadMeta>> {
        const QUERY: &str = r#"
            query($owner: String!, $repo: String!, $number: Int!, $after: String) {
              repository(owner: $owner, name: $repo) {
                pullRequest(number: $number) {
                  reviewThreads(first: 100, after: $after) {
                    pageInfo { hasNextPage endCursor }
                    nodes {
                      id
                      isResolved
                      isOutdated
                      comments(first: 50) {
                        nodes { databaseId }
                      }
                    }
                  }
                }
              }
            }
        "#;

        let mut threads = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..5 {
            let vars = serde_json::json!({
                "owner": owner,
                "repo": repo,
                "number": number,
                "after": cursor,
            });
            let resp = self.graphql(QUERY, vars)?;
            let Some(rt) = resp
                .get("data")
                .and_then(|d| d.get("repository"))
                .and_then(|r| r.get("pullRequest"))
                .and_then(|p| p.get("reviewThreads"))
            else {
                break;
            };
            if let Some(nodes) = rt.get("nodes").and_then(|n| n.as_array()) {
                for node in nodes {
                    let Some(thread_id) = node.get("id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let is_resolved = node
                        .get("isResolved")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let is_outdated = node
                        .get("isOutdated")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let comment_ids = node
                        .get("comments")
                        .and_then(|c| c.get("nodes"))
                        .and_then(|n| n.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|item| item.get("databaseId").and_then(|v| v.as_u64()))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    threads.push(GitHubReviewThreadMeta {
                        thread_id: thread_id.to_string(),
                        is_resolved,
                        is_outdated,
                        comment_ids,
                    });
                }
            }
            let has_next = rt
                .get("pageInfo")
                .and_then(|p| p.get("hasNextPage"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !has_next {
                break;
            }
            cursor = rt
                .get("pageInfo")
                .and_then(|p| p.get("endCursor"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }

        Ok(threads)
    }

    /// Submits a pull request review (`POST /repos/{owner}/{repo}/pulls/{number}/reviews`).
    pub fn create_pull_request_review(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        payload: &CreatePullRequestReviewPayload,
    ) -> Result<GitHubPullRequestReview> {
        let path = format!("/repos/{owner}/{repo}/pulls/{number}/reviews");
        self.send_json(http::Method::POST, &path, payload)
    }

    /// Fetches raw file bytes at a specific ref/commit via the GitHub Contents API
    /// (`GET /repos/{owner}/{repo}/contents/{path}?ref={git_ref}`).
    pub fn get_file_contents_at_ref(
        &self,
        owner: &str,
        repo: &str,
        file_path: &str,
        git_ref: &str,
    ) -> Result<Vec<u8>> {
        let trimmed = file_path
            .replace('\\', "/")
            .trim_start_matches("./")
            .trim_start_matches('/')
            .to_string();
        let mut full_url =
            url::Url::parse(&format!("{}/repos/{owner}/{repo}/contents", self.api_base_url))
                .map_err(|err| OzError::InvalidUsage {
                    message: format!("invalid GitHub API URL: {err}"),
                    recovery: Vec::new(),
                })?;
        {
            let mut segs = full_url.path_segments_mut().map_err(|()| OzError::Other(
                "failed to encode GitHub contents path".to_string(),
            ))?;
            for seg in trimmed.split('/').filter(|s| !s.is_empty()) {
                segs.push(seg);
            }
        }
        full_url.query_pairs_mut().append_pair("ref", git_ref);
        let endpoint = full_url.to_string();
        let label = format!("GET /repos/{owner}/{repo}/contents/{trimmed}?ref={git_ref}");
        let this = self.clone();

        run_blocking_safe(move || {
            let http_client = this.build_blocking_client()?;
            let mut req = http_client
                .get(&endpoint)
                .header("Accept", "application/vnd.github.raw+json")
                .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
                .header("User-Agent", CLIENT_USER_AGENT);
            if let Some(ref tok) = this.token {
                req = req.bearer_auth(tok);
            }
            let resp = req
                .send()
                .map_err(|err| OzError::Other(format!("GitHub API request {label} failed: {err}")))?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().unwrap_or_default();
                return Err(map_http_status_error(status, &label, &body));
            }
            let bytes = resp
                .bytes()
                .map_err(|err| OzError::Other(format!("failed reading {label} body: {err}")))?;
            Ok(bytes.to_vec())
        })
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

    /// Conditionally lists commit check-runs with `If-None-Match` (`ETag`) support.
    pub fn list_check_runs_for_ref_conditional(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
        if_none_match: Option<&str>,
    ) -> Result<ConditionalGet<Vec<GitHubCheckRun>>> {
        let encoded_ref = encode_path_segment(git_ref);
        let path = format!("/repos/{owner}/{repo}/commits/{encoded_ref}/check-runs");
        let params = vec![("per_page".to_string(), "100".to_string())];
        Ok(map_conditional(
            self.get_json_conditional::<CheckRunsEnvelope>(&path, &params, if_none_match)?,
            |env| env.check_runs,
        ))
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

    /// Conditionally lists GitHub Actions workflow runs with `If-None-Match` (`ETag`) support.
    pub fn list_workflow_runs_conditional(
        &self,
        owner: &str,
        repo: &str,
        branch: Option<&str>,
        head_sha: Option<&str>,
        if_none_match: Option<&str>,
    ) -> Result<ConditionalGet<Vec<GitHubWorkflowRun>>> {
        let mut params = vec![("per_page".to_string(), "30".to_string())];
        if let Some(br) = branch {
            params.push(("branch".to_string(), br.to_string()));
        }
        if let Some(sha) = head_sha {
            params.push(("head_sha".to_string(), sha.to_string()));
        }
        let path = format!("/repos/{owner}/{repo}/actions/runs");
        Ok(map_conditional(
            self.get_json_conditional::<WorkflowRunsEnvelope>(&path, &params, if_none_match)?,
            |env| env.workflow_runs,
        ))
    }

    /// Lists GitHub Actions workflows (`GET /repos/{owner}/{repo}/actions/workflows`).
    pub fn list_workflows(&self, owner: &str, repo: &str) -> Result<Vec<GitHubWorkflow>> {
        let path = format!("/repos/{owner}/{repo}/actions/workflows");
        let params = vec![("per_page".to_string(), "100".to_string())];
        let env: WorkflowsEnvelope = self.get_json_with_query(&path, &params)?;
        Ok(env.workflows)
    }

    /// Lists runs for a specific GitHub Actions workflow (`GET /repos/{owner}/{repo}/actions/workflows/{workflow_id}/runs`).
    pub fn list_workflow_runs_for_workflow(
        &self,
        owner: &str,
        repo: &str,
        workflow_id: &str,
        branch: Option<&str>,
        per_page: usize,
    ) -> Result<Vec<GitHubWorkflowRun>> {
        let encoded = encode_path_segment(workflow_id);
        let path = format!("/repos/{owner}/{repo}/actions/workflows/{encoded}/runs");
        let mut params = vec![("per_page".to_string(), per_page.clamp(1, 100).to_string())];
        if let Some(br) = branch {
            params.push(("branch".to_string(), br.to_string()));
        }
        let env: WorkflowRunsEnvelope = self.get_json_with_query(&path, &params)?;
        Ok(env.workflow_runs)
    }

    /// Conditionally lists runs for a specific GitHub Actions workflow with `ETag` support.
    pub fn list_workflow_runs_for_workflow_conditional(
        &self,
        owner: &str,
        repo: &str,
        workflow_id: &str,
        branch: Option<&str>,
        per_page: usize,
        if_none_match: Option<&str>,
    ) -> Result<ConditionalGet<Vec<GitHubWorkflowRun>>> {
        let encoded = encode_path_segment(workflow_id);
        let path = format!("/repos/{owner}/{repo}/actions/workflows/{encoded}/runs");
        let mut params = vec![("per_page".to_string(), per_page.clamp(1, 100).to_string())];
        if let Some(br) = branch {
            params.push(("branch".to_string(), br.to_string()));
        }
        Ok(map_conditional(
            self.get_json_conditional::<WorkflowRunsEnvelope>(&path, &params, if_none_match)?,
            |env| env.workflow_runs,
        ))
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

    /// Conditionally lists jobs for a workflow run with `If-None-Match` (`ETag`) support.
    pub fn list_jobs_for_workflow_run_conditional(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
        if_none_match: Option<&str>,
    ) -> Result<ConditionalGet<Vec<GitHubActionsJob>>> {
        let path = format!("/repos/{owner}/{repo}/actions/runs/{run_id}/jobs");
        let params = vec![("per_page".to_string(), "100".to_string())];
        Ok(map_conditional(
            self.get_json_conditional::<ActionsJobsEnvelope>(&path, &params, if_none_match)?,
            |env| env.jobs,
        ))
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

    fn get_json_conditional<T>(
        &self,
        path: &str,
        query_params: &[(String, String)],
        if_none_match: Option<&str>,
    ) -> Result<ConditionalGet<T>>
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
        let inm = if_none_match
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
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
            if let Some(ref etag) = inm {
                req = req.header("If-None-Match", etag);
            }
            let resp = req
                .send()
                .map_err(|err| OzError::Other(format!("GitHub API request {label} failed: {err}")))?;

            let status = resp.status();
            let etag = resp
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let retry_after = parse_rate_limit_backoff_secs(resp.headers());

            if status == StatusCode::NOT_MODIFIED {
                return Ok(ConditionalGet::NotModified {
                    etag: etag.or(inm),
                    retry_after_secs: retry_after,
                });
            }

            if status == StatusCode::TOO_MANY_REQUESTS
                || (status == StatusCode::FORBIDDEN && is_rate_limited_headers(resp.headers()))
            {
                let raw = resp.text().unwrap_or_default();
                let wait = retry_after.unwrap_or(30);
                return Ok(ConditionalGet::RateLimited {
                    retry_after_secs: wait,
                    message: format!(
                        "GitHub API rate limited ({status}) on {label}: {}",
                        truncate_str(raw.trim(), 160)
                    ),
                });
            }

            let raw = resp.text().map_err(|err| {
                OzError::Other(format!("failed reading {label} response body: {err}"))
            })?;
            if !status.is_success() {
                if status == StatusCode::FORBIDDEN
                    && raw.to_ascii_lowercase().contains("rate limit")
                {
                    return Ok(ConditionalGet::RateLimited {
                        retry_after_secs: retry_after.unwrap_or(30),
                        message: format!(
                            "GitHub API rate limited ({status}) on {label}: {}",
                            truncate_str(raw.trim(), 160)
                        ),
                    });
                }
                return Err(map_http_status_error(status, &label, &raw));
            }

            let value: T = serde_json::from_str(&raw).map_err(|err| {
                OzError::Other(format!(
                    "failed parsing JSON response from {label}: {err} (body: {})",
                    truncate_str(&raw, 240)
                ))
            })?;
            Ok(ConditionalGet::Modified {
                value,
                etag,
                retry_after_secs: retry_after,
            })
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

fn map_conditional<T, U>(resp: ConditionalGet<T>, f: impl FnOnce(T) -> U) -> ConditionalGet<U> {
    match resp {
        ConditionalGet::NotModified {
            etag,
            retry_after_secs,
        } => ConditionalGet::NotModified {
            etag,
            retry_after_secs,
        },
        ConditionalGet::Modified {
            value,
            etag,
            retry_after_secs,
        } => ConditionalGet::Modified {
            value: f(value),
            etag,
            retry_after_secs,
        },
        ConditionalGet::RateLimited {
            retry_after_secs,
            message,
        } => ConditionalGet::RateLimited {
            retry_after_secs,
            message,
        },
    }
}

fn is_rate_limited_headers(headers: &reqwest::header::HeaderMap) -> bool {
    if headers.contains_key(reqwest::header::RETRY_AFTER) {
        return true;
    }
    headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        == Some(0)
}

fn parse_rate_limit_backoff_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    if let Some(secs) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        return Some(secs.clamp(1, 300));
    }
    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    if remaining == Some(0) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Some(reset) = headers
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
        {
            return Some(reset.saturating_sub(now).clamp(1, 300));
        }
        return Some(30);
    }
    None
}
