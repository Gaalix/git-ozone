// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-Rust Git wire protocol client, token discovery, and GitHub stacked PR / sync / CI workflows.

#![allow(
    clippy::missing_errors_doc,
    clippy::too_many_lines,
    clippy::struct_excessive_bools
)]

pub mod api;
pub mod auth;
pub mod ci;
pub mod issue;
pub mod pr;
pub mod release;
pub mod ssh_config;
pub mod sync;
pub mod wire;

pub use api::{
    GitHubActionsJob, GitHubCheckRun, GitHubCheckRunOutput, GitHubClient, GitHubIssue,
    GitHubIssueComment, GitHubIssuePullRequestRef, GitHubJobStep, GitHubLabel, GitHubMilestone,
    GitHubPrRef, GitHubPullRequest, GitHubPullRequestComment, GitHubPullRequestFile,
    GitHubPullRequestReview, GitHubRelease, GitHubReleaseAsset, GitHubUser, GitHubWorkflow,
    GitHubWorkflowRun,
};
pub use auth::{
    DEFAULT_GITHUB_API_URL, DiscoveredToken, GitHubAuthProbe, ParsedGitRemote, RemoteScheme,
    TokenDiscoveryOptions, TokenSource, discover_github_token, discover_github_token_with_options,
    parse_git_remote, parse_git_remote_with_env, probe_github_auth, query_git_credential_fill,
    require_github_token,
};
pub use ci::{
    CiJobLogOptions, CiJobLogReport, CiQueryOptions, CiReport, CiRunsOptions, CiRunsReport,
    CiWatchEvent, CiWatchMetadata, CiWatchOptions, CiWatchOutcome, CiWorkflowOptions,
    CiWorkflowReport, CiWorkflowRunSummary, DEFAULT_MAX_EXCERPT_LINES,
    DEFAULT_WATCH_STARTUP_TIMEOUT_SECS, DEFAULT_WATCH_TIMEOUT_SECS, DEFAULT_WORKFLOW_RUN_LIMIT,
    MAX_MCP_WAIT_SECONDS, MultiPrCiReport, query_ci_job_log, query_ci_runs, query_ci_status,
    query_multi_pr_ci_status, query_workflow_ci_status, watch_ci_status, watch_workflow_ci_status,
};
pub use issue::{
    IssueCommentOptions, IssueCommentOutcome, IssueListItem, IssueListOptions, IssueListOutcome,
    IssueViewOptions, IssueViewOutcome, ParsedIssueTarget, issue_comment, issue_list, issue_view,
    parse_issue_target,
};
pub use pr::{
    OZ_STACK_END, OZ_STACK_START, PrCommentSide, PrCommentsOptions, PrCommentsOutcome,
    PrConversationComment, PrCreateOptions, PrCreateOutcome, PrDiffOptions, PrDiffOutcome,
    PrEditOptions, PrEditOutcome, PrInlineCommentInput, PrListItem, PrListOptions, PrListOutcome,
    PrReadOptions, PrReadOutcome, PrReplyOptions, PrReplyOutcome, PrResolveOptions,
    PrResolveOutcome, PrReviewEvent, PrReviewOptions, PrReviewOutcome, PrReviewThread,
    PrReviewThreadComment, PrStackOptions, PrStackOutcome, PrStatusItem, PrStatusOptions,
    PrStatusOutcome, PrViewFileSummary, PrViewOptions, PrViewOutcome, PrViewReviewSummary,
    StackPrItem, parse_inline_comment_spec, pr_comments, pr_create, pr_diff, pr_edit, pr_list,
    pr_read, pr_reply, pr_resolve, pr_review, pr_stack, pr_status, pr_view,
};
pub use release::{
    ReleaseAssetSummary, ReleaseCreateOptions, ReleaseCreateOutcome, ReleaseEditOptions,
    ReleaseEditOutcome, ReleaseListItem, ReleaseListOptions, ReleaseListOutcome,
    ReleaseViewOptions, ReleaseViewOutcome, release_create, release_edit, release_list,
    release_view,
};
pub use ssh_config::{
    ResolvedSshHost, SshConfigSearchContext, format_openssh_command, resolve_ssh_host,
    resolve_ssh_host_with_context,
};
pub use sync::{
    PullOptions, PullOutcome, PullStrategy, RetargetedPrSummary, SyncOptions, SyncOutcome,
    pull_current_branch, sync_repo,
};
pub use wire::{
    CloneOptions, CloneOutcome, FetchOptions, FetchOutcome, FetchedRefUpdate, ForceLeaseSpec,
    HttpAuthScheme, PushOptions, PushOutcome, PushedRefUpdate, clone_repo, fetch_remote, push_refs,
    push_refs_with_options,
};

use serde::{Deserialize, Serialize};

/// Supported Git remote transport protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitTransportKind {
    Https,
    Ssh,
}

/// Pull request creation parameters for `oz pr create` / `oz pr stack`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestSpec {
    pub title: String,
    pub body: String,
    pub head_branch: String,
    pub base_branch: String,
    pub draft: bool,
}

/// Compact CI check summary extracted by `oz ci --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiCheckSummary {
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failing_step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
}
