// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-Rust Git wire protocol client, token discovery, and GitHub stacked PR / sync / CI workflows.

pub mod api;
pub mod auth;
pub mod ci;
pub mod pr;
pub mod ssh_config;
pub mod sync;
pub mod wire;

pub use api::{
    GitHubActionsJob, GitHubCheckRun, GitHubCheckRunOutput, GitHubClient, GitHubJobStep,
    GitHubPrRef, GitHubPullRequest, GitHubWorkflowRun, UpdatePullRequest,
};
pub use auth::{
    DEFAULT_GITHUB_API_URL, DiscoveredToken, GitHubAuthProbe, ParsedGitRemote, RemoteScheme,
    TokenDiscoveryOptions, TokenSource, discover_github_token, discover_github_token_with_options,
    extract_token_from_gh_hosts_yml, load_git_config_from_gix_repo, parse_git_remote,
    parse_git_remote_with_env, probe_github_auth, query_git_credential_fill,
    read_remote_url_from_gix, read_remote_url_from_repo, require_github_token, resolve_repo_remote,
    strip_host_port,
};
pub use ssh_config::{
    ResolvedSshHost, SshConfigSearchContext, build_gix_ssh_options, format_openssh_command,
    resolve_ssh_host, resolve_ssh_host_with_context,
};
pub use ci::{
    CiQueryOptions, CiReport, DEFAULT_MAX_EXCERPT_LINES, extract_log_text_from_bytes,
    query_ci_status, strip_actions_timestamp, strip_ansi_escapes, summarize_failing_log,
};
pub use pr::{
    OZ_STACK_END, OZ_STACK_START, PrCreateOptions, PrCreateOutcome, PrStackOptions,
    PrStackOutcome, StackPrItem, infer_title_body_and_change_ids, pr_create, pr_stack,
    render_stack_navigation_block, strip_stack_navigation_block, upsert_stack_navigation_block,
};
pub use sync::{RetargetedPrSummary, SyncOptions, SyncOutcome, sync_repo};
pub use wire::{
    CloneOptions, CloneOutcome, FetchOptions, FetchOutcome, FetchedRefUpdate, ForceLeaseSpec,
    HttpAuthScheme, PushOptions, PushOutcome, PushedRefUpdate, clone_repo, fetch_remote,
    push_refs, push_refs_with_options,
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
    pub conclusion: Option<String>,
    pub failing_step: Option<String>,
    pub log_excerpt: Option<String>,
}
