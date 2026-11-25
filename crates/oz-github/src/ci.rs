// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `oz ci` check-run & GitHub Actions job log inspector, stripping ANSI codes, ISO-8601
//! runner timestamps, and boilerplate noise to produce low-token error excerpts for agent
//! self-healing loops. Supports both plain-text logs and PKZIP job log archives.

use crate::CiCheckSummary;
use crate::api::{
    ConditionalGet, GitHubActionsJob, GitHubCheckRun, GitHubClient, GitHubWorkflowRun,
};
use crate::pr::normalize_short_branch;
use crate::wire::{is_ancestor_commit, open_gix_unrestricted, read_ref_oid};
use flate2::read::DeflateDecoder;
use oz_core::{OzError, OzoneRepo, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::time::{Duration, Instant};

/// Default maximum lines included in each failing check's `log_excerpt`.
pub const DEFAULT_MAX_EXCERPT_LINES: usize = 40;
/// Default number of workflow runs returned by `oz ci --workflow <name>`.
pub const DEFAULT_WORKFLOW_RUN_LIMIT: usize = 5;
/// Default overall watch timeout for `oz ci --watch` (30 minutes).
pub const DEFAULT_WATCH_TIMEOUT_SECS: u64 = 1800;
/// Default startup grace period waiting for GitHub to register checks after a push.
pub const DEFAULT_WATCH_STARTUP_TIMEOUT_SECS: u64 = 45;
/// Hard ceiling for MCP bounded long-polling (`wait_seconds`) to avoid transport timeouts.
pub const MAX_MCP_WAIT_SECONDS: u64 = 120;

const MAX_EXCERPT_CHARS: usize = 4096;
const MAX_EXPANDED_EXCERPT_CHARS: usize = 32_768;

/// Options controlling [`query_ci_status`].
#[derive(Debug, Clone)]
pub struct CiQueryOptions {
    /// Optional explicit branch, tag, or commit SHA (defaults to active workspace branch / `HEAD`).
    pub target_ref: Option<String>,
    /// Optional pull request number (`--pr <num>`); overrides `target_ref` when provided.
    pub pr_number: Option<u64>,
    /// Remote name (defaults to `"origin"`).
    pub remote: Option<String>,
    /// Maximum lines per failing check's `log_excerpt` (defaults to `40`).
    pub max_excerpt_lines: usize,
    /// Optional check-name inclusion patterns (case-insensitive substring or `*`/`?` glob).
    pub include_checks: Vec<String>,
    /// Optional check-name exclusion patterns (case-insensitive substring or `*`/`?` glob).
    pub ignore_checks: Vec<String>,
    /// Explicit GitHub token override.
    pub auth_token: Option<String>,
    /// Explicit GitHub API base URL override (also honors `OZ_GITHUB_API_URL`).
    pub api_url: Option<String>,
}

impl Default for CiQueryOptions {
    fn default() -> Self {
        Self {
            target_ref: None,
            pr_number: None,
            remote: None,
            max_excerpt_lines: DEFAULT_MAX_EXCERPT_LINES,
            include_checks: Vec::new(),
            ignore_checks: Vec::new(),
            auth_token: None,
            api_url: None,
        }
    }
}

/// Terminal or bounded-poll outcome produced by [`watch_ci_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiWatchOutcome {
    /// All watched checks completed without failures.
    Passed,
    /// Exited early (`fail_fast`) as soon as a watched check failed.
    FailedFast,
    /// All watched checks completed and at least one failed (`wait_all`).
    Failed,
    /// Overall watch timeout expired while checks were still pending.
    TimedOut,
    /// Startup grace period expired with zero checks registered for the commit.
    NoChecksTimedOut,
    /// A newer commit superseded the watched branch or pull request.
    Superseded,
}

impl CiWatchOutcome {
    /// Canonical snake_case identifier for JSON/error codes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::FailedFast => "failed_fast",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::NoChecksTimedOut => "no_checks_timed_out",
            Self::Superseded => "superseded",
        }
    }

    /// Returns `true` only when watched CI checks all passed.
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Passed)
    }
}

/// Watch execution metadata attached to [`CiReport::watch`] by [`watch_ci_status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiWatchMetadata {
    pub outcome: CiWatchOutcome,
    pub elapsed_secs: u64,
    pub polls: u32,
    pub stopped_early: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_poll_secs: Option<u64>,
}

/// Options controlling [`watch_ci_status`].
#[derive(Debug, Clone)]
pub struct CiWatchOptions {
    pub query: CiQueryOptions,
    /// Exit immediately on the first failing watched check (default: `true`).
    pub fail_fast: bool,
    /// Overall watch timeout before returning [`CiWatchOutcome::TimedOut`].
    pub timeout: Duration,
    /// Grace period waiting for checks to appear when initially `no_checks`.
    pub startup_timeout: Duration,
    /// Optional fixed polling interval (defaults to adaptive `5s -> 10s -> 15s -> 30s`).
    pub poll_interval: Option<Duration>,
}

impl Default for CiWatchOptions {
    fn default() -> Self {
        Self {
            query: CiQueryOptions::default(),
            fail_fast: true,
            timeout: Duration::from_secs(DEFAULT_WATCH_TIMEOUT_SECS),
            startup_timeout: Duration::from_secs(DEFAULT_WATCH_STARTUP_TIMEOUT_SECS),
            poll_interval: None,
        }
    }
}

/// Incremental state-change event emitted to optional progress listeners during [`watch_ci_status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiWatchEvent {
    WaitingForChecks {
        ref_name: String,
        commit_sha: String,
        elapsed_secs: u64,
    },
    CheckUpdated {
        name: String,
        status: String,
        conclusion: Option<String>,
        failing_step: Option<String>,
    },
    RateLimited {
        retry_after_secs: u64,
    },
    Superseded {
        old_sha: String,
        new_sha: String,
    },
    Completed {
        outcome: CiWatchOutcome,
        elapsed_secs: u64,
        total_count: usize,
        failing_count: usize,
        pending_count: usize,
    },
}

/// Options controlling [`query_ci_job_log`] (`oz ci --job <id>`).
#[derive(Debug, Clone)]
pub struct CiJobLogOptions {
    pub job_id: u64,
    /// Optional step name filter for ZIP log extraction and excerpt focus.
    pub step: Option<String>,
    /// Maximum lines included in `log_excerpt` (defaults to `40`).
    pub max_excerpt_lines: usize,
    /// When `true`, returns the cleaned (ANSI/timestamp-stripped) tail instead of error-anchor excerpts.
    pub raw: bool,
    /// Remote name (defaults to `"origin"`).
    pub remote: Option<String>,
    /// Explicit GitHub token override.
    pub auth_token: Option<String>,
    /// Explicit GitHub API base URL override.
    pub api_url: Option<String>,
}

impl Default for CiJobLogOptions {
    fn default() -> Self {
        Self {
            job_id: 0,
            step: None,
            max_excerpt_lines: DEFAULT_MAX_EXCERPT_LINES,
            raw: false,
            remote: None,
            auth_token: None,
            api_url: None,
        }
    }
}

/// Single-job log drill-down report returned by [`query_ci_job_log`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiJobLogReport {
    pub job_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failing_step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_step: Option<String>,
    pub raw: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_excerpt: Option<String>,
}

/// Complete CI health & failure excerpt report returned by [`query_ci_status`] and [`watch_ci_status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiReport {
    pub ref_name: String,
    pub commit_sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    /// Aggregate state: `"passing"`, `"failing"`, `"pending"`, `"no_checks"`, or `"superseded"`.
    pub overall_status: String,
    pub all_passed: bool,
    pub total_count: usize,
    pub failing_count: usize,
    pub pending_count: usize,
    pub checks: Vec<CiCheckSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<CiWatchMetadata>,
}

/// Multi-PR CI aggregation report returned by [`query_multi_pr_ci_status`] (`oz ci --pr 1,2,3`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiPrCiReport {
    /// Aggregate state across all requested PRs: `"passing"`, `"failing"`, `"pending"`, or `"no_checks"`.
    pub overall_status: String,
    pub all_passed: bool,
    pub pr_count: usize,
    pub passing_prs: usize,
    pub failing_prs: usize,
    pub pending_prs: usize,
    pub reports: Vec<CiReport>,
}

/// Options controlling [`query_workflow_ci_status`] and [`watch_workflow_ci_status`] (`oz ci --workflow <name>`).
#[derive(Debug, Clone)]
pub struct CiWorkflowOptions {
    /// Workflow filename (e.g. `sentry-release.yaml`), path, numeric ID, or display name.
    pub workflow: String,
    /// Optional branch filter (`--branch` / `--ref`); when `None`, returns recent runs across all branches.
    pub branch: Option<String>,
    /// Maximum number of recent workflow runs to return (defaults to `5`).
    pub limit: usize,
    /// Maximum lines per failing job's `log_excerpt` (defaults to `40`).
    pub max_excerpt_lines: usize,
    /// Optional job-name inclusion patterns.
    pub include_checks: Vec<String>,
    /// Optional job-name exclusion patterns.
    pub ignore_checks: Vec<String>,
    /// Remote name (defaults to `"origin"`).
    pub remote: Option<String>,
    /// Explicit GitHub token override.
    pub auth_token: Option<String>,
    /// Explicit GitHub API base URL override.
    pub api_url: Option<String>,
}

impl Default for CiWorkflowOptions {
    fn default() -> Self {
        Self {
            workflow: String::new(),
            branch: None,
            limit: DEFAULT_WORKFLOW_RUN_LIMIT,
            max_excerpt_lines: DEFAULT_MAX_EXCERPT_LINES,
            include_checks: Vec::new(),
            ignore_checks: Vec::new(),
            remote: None,
            auth_token: None,
            api_url: None,
        }
    }
}

/// Summary of a single GitHub Actions workflow run with its job statuses and failure excerpts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiWorkflowRunSummary {
    pub run_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub commit_sha: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    pub jobs: Vec<CiCheckSummary>,
}

/// Workflow-level CI report returned by [`query_workflow_ci_status`] and [`watch_workflow_ci_status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiWorkflowReport {
    pub workflow: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_filter: Option<String>,
    /// Status of the latest run: `"passing"`, `"failing"`, `"pending"`, or `"no_runs"`.
    pub overall_status: String,
    pub all_passed: bool,
    pub total_runs: usize,
    pub failing_count: usize,
    pub pending_count: usize,
    pub runs: Vec<CiWorkflowRunSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<CiWatchMetadata>,
}

/// Queries GitHub Check Runs and Actions jobs for the target branch/commit/PR, downloads logs
/// for any failing jobs (plain text or ZIP), and extracts compact error excerpts.
pub fn query_ci_status(repo: &OzoneRepo, opts: &CiQueryOptions) -> Result<CiReport> {
    let remote_name = opts
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("origin");
    let max_lines = if opts.max_excerpt_lines == 0 {
        DEFAULT_MAX_EXCERPT_LINES
    } else {
        opts.max_excerpt_lines
    };

    let (client, parsed_remote) = GitHubClient::for_repo(
        repo,
        Some(remote_name),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let _ = client.require_authenticated()?;
    let (owner, gh_repo) = parsed_remote.require_owner_repo()?;

    query_ci_status_with_client(repo, &client, owner, gh_repo, opts, max_lines)
}

fn query_ci_status_with_client(
    repo: &OzoneRepo,
    client: &GitHubClient,
    owner: &str,
    gh_repo: &str,
    opts: &CiQueryOptions,
    max_lines: usize,
) -> Result<CiReport> {
    let (ref_name, commit_sha, resolved_pr_num, _initial_pr_head_sha) =
        resolve_ci_target(repo, client, owner, gh_repo, opts)?;

    let query_ref = if !commit_sha.is_empty() {
        commit_sha.as_str()
    } else {
        ref_name.as_str()
    };

    let check_runs = client
        .list_check_runs_for_ref(owner, gh_repo, query_ref)
        .unwrap_or_default();

    let mut jobs_by_id: HashMap<u64, GitHubActionsJob> = HashMap::new();
    let mut jobs_by_name: HashMap<String, GitHubActionsJob> = HashMap::new();

    let branch_filter = (!ref_name.is_empty() && ref_name != commit_sha).then_some(ref_name.as_str());
    let sha_filter = (!commit_sha.is_empty()).then_some(commit_sha.as_str());
    if let Ok(runs) = client.list_workflow_runs(owner, gh_repo, branch_filter, sha_filter) {
        for run in runs.into_iter().take(10) {
            if !commit_sha.is_empty() && run.head_sha != commit_sha {
                continue;
            }
            if let Ok(jobs) = client.list_jobs_for_workflow_run(owner, gh_repo, run.id) {
                for job in jobs {
                    jobs_by_id.insert(job.id, job.clone());
                    jobs_by_name.entry(job.name.clone()).or_insert(job);
                }
            }
        }
    }

    let mut failure_cache = HashMap::new();
    Ok(assemble_ci_report(
        client,
        owner,
        gh_repo,
        ref_name,
        commit_sha,
        resolved_pr_num,
        &check_runs,
        &jobs_by_id,
        &jobs_by_name,
        &opts.include_checks,
        &opts.ignore_checks,
        max_lines,
        &mut failure_cache,
    ))
}

/// Queries CI status across multiple pull requests in one command (`oz ci --pr 3320,3321,3322`).
pub fn query_multi_pr_ci_status(
    repo: &OzoneRepo,
    pr_numbers: &[u64],
    base_opts: &CiQueryOptions,
) -> Result<MultiPrCiReport> {
    let mut deduped = Vec::new();
    let mut seen = HashSet::new();
    for &n in pr_numbers {
        if seen.insert(n) {
            deduped.push(n);
        }
    }
    if deduped.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "`--pr` requires at least one pull request number".to_string(),
            recovery: vec!["oz ci --pr 3320,3321 --json".to_string()],
        });
    }

    let remote_name = base_opts
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("origin");
    let max_lines = if base_opts.max_excerpt_lines == 0 {
        DEFAULT_MAX_EXCERPT_LINES
    } else {
        base_opts.max_excerpt_lines
    };

    let (client, parsed_remote) = GitHubClient::for_repo(
        repo,
        Some(remote_name),
        base_opts.auth_token.as_deref(),
        base_opts.api_url.as_deref(),
    )?;
    let _ = client.require_authenticated()?;
    let (owner, gh_repo) = parsed_remote.require_owner_repo()?;

    let mut reports = Vec::with_capacity(deduped.len());
    for pr_num in deduped {
        let mut single_opts = base_opts.clone();
        single_opts.pr_number = Some(pr_num);
        single_opts.target_ref = None;
        reports.push(query_ci_status_with_client(
            repo,
            &client,
            owner,
            gh_repo,
            &single_opts,
            max_lines,
        )?);
    }

    let pr_count = reports.len();
    let passing_prs = reports.iter().filter(|r| r.all_passed).count();
    let failing_prs = reports.iter().filter(|r| r.failing_count > 0).count();
    let pending_prs = reports
        .iter()
        .filter(|r| r.failing_count == 0 && r.pending_count > 0)
        .count();

    let overall_status = if reports.iter().all(|r| r.total_count == 0) {
        "no_checks".to_string()
    } else if failing_prs > 0 {
        "failing".to_string()
    } else if pending_prs > 0 {
        "pending".to_string()
    } else if passing_prs == pr_count {
        "passing".to_string()
    } else {
        "pending".to_string()
    };
    let all_passed = pr_count > 0 && passing_prs == pr_count;

    Ok(MultiPrCiReport {
        overall_status,
        all_passed,
        pr_count,
        passing_prs,
        failing_prs,
        pending_prs,
        reports,
    })
}

/// Queries recent runs and jobs for a specific GitHub Actions workflow (`oz ci --workflow <file-or-name>`).
pub fn query_workflow_ci_status(
    repo: &OzoneRepo,
    opts: &CiWorkflowOptions,
) -> Result<CiWorkflowReport> {
    let workflow_query = opts.workflow.trim();
    if workflow_query.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "`--workflow` requires a workflow filename, ID, or name".to_string(),
            recovery: vec!["oz ci --workflow ci.yml --json".to_string()],
        });
    }

    let remote_name = opts
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("origin");
    let max_lines = if opts.max_excerpt_lines == 0 {
        DEFAULT_MAX_EXCERPT_LINES
    } else {
        opts.max_excerpt_lines
    };
    let limit = if opts.limit == 0 {
        DEFAULT_WORKFLOW_RUN_LIMIT
    } else {
        opts.limit.min(50)
    };
    let branch_filter = opts
        .branch
        .as_deref()
        .map(normalize_short_branch)
        .filter(|s| !s.is_empty());

    let (client, parsed_remote) = GitHubClient::for_repo(
        repo,
        Some(remote_name),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let _ = client.require_authenticated()?;
    let (owner, gh_repo) = parsed_remote.require_owner_repo()?;

    let (resolved_wf_id, runs) =
        fetch_workflow_runs_resolved(&client, owner, gh_repo, workflow_query, branch_filter.as_deref(), limit)?;

    let mut failure_cache = HashMap::new();
    let summaries = build_workflow_run_summaries(
        &client,
        owner,
        gh_repo,
        &runs,
        &opts.include_checks,
        &opts.ignore_checks,
        max_lines,
        &mut failure_cache,
        true,
    );

    Ok(assemble_workflow_report(
        workflow_query.to_string(),
        Some(resolved_wf_id),
        branch_filter,
        summaries,
        None,
    ))
}

/// Watches the latest run of a GitHub Actions workflow (`oz ci --workflow <name> --watch`)
/// until it passes, fails, or times out.
pub fn watch_workflow_ci_status<F>(
    repo: &OzoneRepo,
    wf_opts: &CiWorkflowOptions,
    watch_opts: &CiWatchOptions,
    mut on_event: F,
) -> Result<CiWorkflowReport>
where
    F: FnMut(CiWatchEvent),
{
    let workflow_query = wf_opts.workflow.trim();
    if workflow_query.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "`--workflow` requires a workflow filename, ID, or name".to_string(),
            recovery: vec!["oz ci --workflow ci.yml --watch --json".to_string()],
        });
    }

    let remote_name = wf_opts
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("origin");
    let max_lines = if wf_opts.max_excerpt_lines == 0 {
        DEFAULT_MAX_EXCERPT_LINES
    } else {
        wf_opts.max_excerpt_lines
    };
    let branch_filter = wf_opts
        .branch
        .as_deref()
        .map(normalize_short_branch)
        .filter(|s| !s.is_empty());

    let (client, parsed_remote) = GitHubClient::for_repo(
        repo,
        Some(remote_name),
        wf_opts.auth_token.as_deref(),
        wf_opts.api_url.as_deref(),
    )?;
    let _ = client.require_authenticated()?;
    let (owner, gh_repo) = parsed_remote.require_owner_repo()?;

    let (resolved_wf_id, initial_runs) = fetch_workflow_runs_resolved(
        &client,
        owner,
        gh_repo,
        workflow_query,
        branch_filter.as_deref(),
        1,
    )?;

    let start = Instant::now();
    let mut polls: u32 = 0;
    let mut runs_etag: Option<String> = None;
    let mut cached_runs: Vec<GitHubWorkflowRun> = initial_runs;
    let mut jobs_cache: HashMap<u64, (Option<String>, Vec<GitHubActionsJob>)> = HashMap::new();
    let mut failure_cache: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    let mut seen_states: HashMap<String, (String, Option<String>)> = HashMap::new();
    let mut emitted_waiting = false;

    loop {
        polls = polls.saturating_add(1);
        let mut rate_limit_wait: Option<u64> = None;

        if polls > 1 {
            match client.list_workflow_runs_for_workflow_conditional(
                owner,
                gh_repo,
                &resolved_wf_id,
                branch_filter.as_deref(),
                1,
                runs_etag.as_deref(),
            ) {
                Ok(ConditionalGet::Modified { value, etag, .. }) => {
                    cached_runs = value;
                    if etag.is_some() {
                        runs_etag = etag;
                    }
                }
                Ok(ConditionalGet::NotModified { etag, .. }) => {
                    if etag.is_some() {
                        runs_etag = etag;
                    }
                }
                Ok(ConditionalGet::RateLimited { retry_after_secs, .. }) => {
                    rate_limit_wait = Some(rate_limit_wait.unwrap_or(0).max(retry_after_secs));
                }
                Err(_) => {}
            }
        }

        let mut run_summaries = Vec::new();
        for run in cached_runs.iter().take(1) {
            let prev_etag = jobs_cache.get(&run.id).and_then(|(e, _)| e.clone());
            match client.list_jobs_for_workflow_run_conditional(
                owner,
                gh_repo,
                run.id,
                prev_etag.as_deref(),
            ) {
                Ok(ConditionalGet::Modified { value, etag, .. }) => {
                    jobs_cache.insert(run.id, (etag.or(prev_etag), value));
                }
                Ok(ConditionalGet::NotModified { etag, .. }) => {
                    if let Some(entry) = jobs_cache.get_mut(&run.id)
                        && etag.is_some()
                    {
                        entry.0 = etag;
                    }
                }
                Ok(ConditionalGet::RateLimited { retry_after_secs, .. }) => {
                    rate_limit_wait = Some(rate_limit_wait.unwrap_or(0).max(retry_after_secs));
                }
                Err(_) => {}
            }
            let jobs = jobs_cache
                .get(&run.id)
                .map(|(_, j)| j.as_slice())
                .unwrap_or(&[]);
            run_summaries.push(build_single_workflow_run_summary(
                &client,
                owner,
                gh_repo,
                run,
                jobs,
                &wf_opts.include_checks,
                &wf_opts.ignore_checks,
                max_lines,
                &mut failure_cache,
                true,
            ));
        }

        let elapsed = start.elapsed();
        let elapsed_secs = elapsed.as_secs();

        if let Some(latest) = run_summaries.first() {
            for job in &latest.jobs {
                let state = (job.status.clone(), job.conclusion.clone());
                if seen_states.get(&job.name) != Some(&state) {
                    seen_states.insert(job.name.clone(), state);
                    on_event(CiWatchEvent::CheckUpdated {
                        name: job.name.clone(),
                        status: job.status.clone(),
                        conclusion: job.conclusion.clone(),
                        failing_step: job.failing_step.clone(),
                    });
                }
            }
        } else if !emitted_waiting {
            emitted_waiting = true;
            on_event(CiWatchEvent::WaitingForChecks {
                ref_name: workflow_query.to_string(),
                commit_sha: String::new(),
                elapsed_secs,
            });
        }

        let report = assemble_workflow_report(
            workflow_query.to_string(),
            Some(resolved_wf_id.clone()),
            branch_filter.clone(),
            run_summaries,
            None,
        );

        let total_jobs = report.runs.first().map(|r| r.jobs.len()).unwrap_or(0);
        if !report.runs.is_empty() {
            if report.failing_count > 0 && watch_opts.fail_fast {
                let stopped_early = report.pending_count > 0;
                let outcome = if stopped_early {
                    CiWatchOutcome::FailedFast
                } else {
                    CiWatchOutcome::Failed
                };
                on_event(CiWatchEvent::Completed {
                    outcome,
                    elapsed_secs,
                    total_count: total_jobs,
                    failing_count: report.failing_count,
                    pending_count: report.pending_count,
                });
                return Ok(CiWorkflowReport {
                    watch: Some(CiWatchMetadata {
                        outcome,
                        elapsed_secs,
                        polls,
                        stopped_early,
                        superseded_by_sha: None,
                        recommended_poll_secs: None,
                    }),
                    ..report
                });
            }
            if report.pending_count == 0 {
                let outcome = if report.failing_count > 0 {
                    CiWatchOutcome::Failed
                } else {
                    CiWatchOutcome::Passed
                };
                on_event(CiWatchEvent::Completed {
                    outcome,
                    elapsed_secs,
                    total_count: total_jobs,
                    failing_count: report.failing_count,
                    pending_count: report.pending_count,
                });
                return Ok(CiWorkflowReport {
                    watch: Some(CiWatchMetadata {
                        outcome,
                        elapsed_secs,
                        polls,
                        stopped_early: false,
                        superseded_by_sha: None,
                        recommended_poll_secs: None,
                    }),
                    ..report
                });
            }
        } else if elapsed >= watch_opts.startup_timeout {
            let outcome = CiWatchOutcome::NoChecksTimedOut;
            on_event(CiWatchEvent::Completed {
                outcome,
                elapsed_secs,
                total_count: 0,
                failing_count: 0,
                pending_count: 0,
            });
            return Ok(CiWorkflowReport {
                watch: Some(CiWatchMetadata {
                    outcome,
                    elapsed_secs,
                    polls,
                    stopped_early: false,
                    superseded_by_sha: None,
                    recommended_poll_secs: Some(10),
                }),
                ..report
            });
        }

        if elapsed >= watch_opts.timeout {
            let outcome = if report.runs.is_empty() {
                CiWatchOutcome::NoChecksTimedOut
            } else {
                CiWatchOutcome::TimedOut
            };
            let recommended = recommended_poll_secs(&report.overall_status, elapsed_secs);
            on_event(CiWatchEvent::Completed {
                outcome,
                elapsed_secs,
                total_count: total_jobs,
                failing_count: report.failing_count,
                pending_count: report.pending_count,
            });
            return Ok(CiWorkflowReport {
                watch: Some(CiWatchMetadata {
                    outcome,
                    elapsed_secs,
                    polls,
                    stopped_early: false,
                    superseded_by_sha: None,
                    recommended_poll_secs: recommended,
                }),
                ..report
            });
        }

        let base_sleep = adaptive_poll_interval(polls.saturating_sub(1), watch_opts.poll_interval);
        let desired_sleep = if let Some(rl_secs) = rate_limit_wait {
            on_event(CiWatchEvent::RateLimited {
                retry_after_secs: rl_secs,
            });
            Duration::from_secs(rl_secs).max(base_sleep)
        } else {
            base_sleep
        };
        let effective_deadline = if report.runs.is_empty() {
            watch_opts.startup_timeout.min(watch_opts.timeout)
        } else {
            watch_opts.timeout
        };
        let remaining = effective_deadline.saturating_sub(elapsed);
        if remaining.is_zero() {
            continue;
        }
        std::thread::sleep(desired_sleep.min(remaining));
    }
}

fn normalize_workflow_selector(raw: &str) -> String {
    let trimmed = raw.trim().replace('\\', "/");
    let stripped = trimmed
        .strip_prefix("./")
        .unwrap_or(&trimmed)
        .strip_prefix(".github/workflows/")
        .unwrap_or(&trimmed);
    stripped.to_string()
}

fn fetch_workflow_runs_resolved(
    client: &GitHubClient,
    owner: &str,
    gh_repo: &str,
    workflow_query: &str,
    branch_filter: Option<&str>,
    limit: usize,
) -> Result<(String, Vec<GitHubWorkflowRun>)> {
    let normalized = normalize_workflow_selector(workflow_query);
    let looks_direct = normalized.chars().all(|c| c.is_ascii_digit())
        || normalized.ends_with(".yml")
        || normalized.ends_with(".yaml");

    if looks_direct {
        match client.list_workflow_runs_for_workflow(owner, gh_repo, &normalized, branch_filter, limit) {
            Ok(runs) => return Ok((normalized, runs)),
            Err(first_err) => {
                // Fall back to resolving by workflow catalog (e.g., mismatched extension or display name).
                if let Ok(resolved_id) = resolve_workflow_from_catalog(client, owner, gh_repo, workflow_query) {
                    let runs = client.list_workflow_runs_for_workflow(
                        owner,
                        gh_repo,
                        &resolved_id,
                        branch_filter,
                        limit,
                    )?;
                    return Ok((resolved_id, runs));
                }
                return Err(first_err);
            }
        }
    }

    let resolved_id = resolve_workflow_from_catalog(client, owner, gh_repo, workflow_query)?;
    let runs = client.list_workflow_runs_for_workflow(owner, gh_repo, &resolved_id, branch_filter, limit)?;
    Ok((resolved_id, runs))
}

fn resolve_workflow_from_catalog(
    client: &GitHubClient,
    owner: &str,
    gh_repo: &str,
    workflow_query: &str,
) -> Result<String> {
    let workflows = client.list_workflows(owner, gh_repo)?;
    let needle = normalize_workflow_selector(workflow_query).to_ascii_lowercase();
    let needle_stem = needle
        .strip_suffix(".yaml")
        .or_else(|| needle.strip_suffix(".yml"))
        .unwrap_or(&needle);

    if let Some(wf) = workflows.iter().find(|w| {
        let path_file = normalize_workflow_selector(&w.path).to_ascii_lowercase();
        let path_stem = path_file
            .strip_suffix(".yaml")
            .or_else(|| path_file.strip_suffix(".yml"))
            .unwrap_or(&path_file);
        w.id.to_string() == needle
            || path_file == needle
            || path_stem == needle_stem
            || w.name.to_ascii_lowercase() == needle
    }) {
        return Ok(wf.id.to_string());
    }

    if let Some(wf) = workflows.iter().find(|w| {
        w.name.to_ascii_lowercase().contains(&needle)
            || w.path.to_ascii_lowercase().contains(&needle)
    }) {
        return Ok(wf.id.to_string());
    }

    let available: Vec<String> = workflows
        .iter()
        .take(10)
        .map(|w| normalize_workflow_selector(&w.path))
        .collect();
    Err(OzError::InvalidUsage {
        message: format!("workflow '{workflow_query}' not found in {owner}/{gh_repo}"),
        recovery: if available.is_empty() {
            vec!["Verify `.github/workflows/*.yml` exists in this repository.".to_string()]
        } else {
            vec![format!("Available workflows: {}", available.join(", "))]
        },
    })
}

#[expect(clippy::too_many_arguments)]
fn build_workflow_run_summaries(
    client: &GitHubClient,
    owner: &str,
    gh_repo: &str,
    runs: &[GitHubWorkflowRun],
    include_checks: &[String],
    ignore_checks: &[String],
    max_lines: usize,
    failure_cache: &mut HashMap<String, (Option<String>, Option<String>)>,
    fetch_failure_logs: bool,
) -> Vec<CiWorkflowRunSummary> {
    runs.iter()
        .enumerate()
        .map(|(idx, run)| {
            // Fetch jobs for the latest run, any in-progress run, or any failing run (avoid extra API calls for old passing runs).
            let should_fetch_jobs = idx == 0
                || run.status != "completed"
                || is_failing_conclusion(run.conclusion.as_deref());
            let jobs = if should_fetch_jobs {
                client
                    .list_jobs_for_workflow_run(owner, gh_repo, run.id)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            build_single_workflow_run_summary(
                client,
                owner,
                gh_repo,
                run,
                &jobs,
                include_checks,
                ignore_checks,
                max_lines,
                failure_cache,
                fetch_failure_logs,
            )
        })
        .collect()
}

#[expect(clippy::too_many_arguments)]
fn build_single_workflow_run_summary(
    client: &GitHubClient,
    owner: &str,
    gh_repo: &str,
    run: &GitHubWorkflowRun,
    jobs: &[GitHubActionsJob],
    include_checks: &[String],
    ignore_checks: &[String],
    max_lines: usize,
    failure_cache: &mut HashMap<String, (Option<String>, Option<String>)>,
    fetch_failure_logs: bool,
) -> CiWorkflowRunSummary {
    let mut job_summaries = Vec::new();
    for job in jobs {
        if !check_matches_filters(&job.name, include_checks, ignore_checks) {
            continue;
        }
        job_summaries.push(build_check_summary(
            client,
            owner,
            gh_repo,
            None,
            Some(job),
            max_lines,
            failure_cache,
            fetch_failure_logs,
        ));
    }
    CiWorkflowRunSummary {
        run_id: run.id,
        workflow_name: run.name.clone(),
        event: run.event.clone(),
        branch: run.head_branch.clone(),
        commit_sha: run.head_sha.clone(),
        status: run.status.clone(),
        conclusion: run.conclusion.clone(),
        html_url: run.html_url.clone(),
        created_at: run.created_at.clone(),
        updated_at: run.updated_at.clone(),
        jobs: job_summaries,
    }
}

fn assemble_workflow_report(
    workflow: String,
    workflow_id: Option<String>,
    branch_filter: Option<String>,
    runs: Vec<CiWorkflowRunSummary>,
    watch: Option<CiWatchMetadata>,
) -> CiWorkflowReport {
    let total_runs = runs.len();
    let (overall_status, all_passed, failing_count, pending_count) =
        if let Some(latest) = runs.first() {
            let job_failing = latest
                .jobs
                .iter()
                .filter(|j| is_failing_conclusion(j.conclusion.as_deref()))
                .count();
            let job_pending = latest
                .jobs
                .iter()
                .filter(|j| j.status != "completed")
                .count();
            let run_failing = is_failing_conclusion(latest.conclusion.as_deref());
            let run_pending = latest.status != "completed";
            let failing = if job_failing > 0 {
                job_failing
            } else {
                usize::from(run_failing)
            };
            let pending = if job_pending > 0 {
                job_pending
            } else {
                usize::from(run_pending)
            };
            let overall = if failing > 0 {
                "failing"
            } else if pending > 0 {
                "pending"
            } else {
                "passing"
            };
            (overall.to_string(), failing == 0 && pending == 0, failing, pending)
        } else {
            ("no_runs".to_string(), false, 0, 0)
        };

    CiWorkflowReport {
        workflow,
        workflow_id,
        branch_filter,
        overall_status,
        all_passed,
        total_runs,
        failing_count,
        pending_count,
        runs,
        watch,
    }
}

/// Polls GitHub Check Runs and Actions jobs until all watched checks pass, a check fails
/// (`fail_fast` exits immediately with its cleaned log excerpt), a newer commit supersedes
/// the watched branch/PR, or `timeout` / `startup_timeout` expires.
///
/// Uses HTTP `ETag` / `If-None-Match` conditional caching (`304 Not Modified`), rate-limit
/// backoff, and failure-log caching across ticks.
pub fn watch_ci_status<F>(
    repo: &OzoneRepo,
    opts: &CiWatchOptions,
    mut on_event: F,
) -> Result<CiReport>
where
    F: FnMut(CiWatchEvent),
{
    let query = &opts.query;
    let remote_name = query
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("origin");
    let max_lines = if query.max_excerpt_lines == 0 {
        DEFAULT_MAX_EXCERPT_LINES
    } else {
        query.max_excerpt_lines
    };

    let (client, parsed_remote) = GitHubClient::for_repo(
        repo,
        Some(remote_name),
        query.auth_token.as_deref(),
        query.api_url.as_deref(),
    )?;
    let _ = client.require_authenticated()?;
    let (owner, gh_repo) = parsed_remote.require_owner_repo()?;

    let (ref_name, commit_sha, resolved_pr_num, initial_pr_head_sha) =
        resolve_ci_target(repo, &client, owner, gh_repo, query)?;
    let gix_repo = open_gix_unrestricted(&repo.repo_root).ok();
    let mut initial_stale_pr_sha: Option<String> =
        initial_pr_head_sha.filter(|s| !s.is_empty() && s != &commit_sha);
    let mut preexisting_other_run_ids: HashSet<u64> = HashSet::new();
    let mut seen_watched_workflow_run = false;

    let query_ref = if !commit_sha.is_empty() {
        commit_sha.clone()
    } else {
        ref_name.clone()
    };
    let branch_filter = (!ref_name.is_empty() && ref_name != commit_sha && ref_name != "HEAD")
        .then_some(ref_name.as_str());
    let sha_filter = (!commit_sha.is_empty()).then_some(commit_sha.as_str());
    // When watching a branch/PR (not a pinned SHA), query workflow runs by branch so we can
    // also detect if a newer commit superseded the watched commit.
    let wf_sha_filter = if branch_filter.is_some() {
        None
    } else {
        sha_filter
    };

    let start = Instant::now();
    let mut polls: u32 = 0;

    // ETag + response caches across watch ticks.
    let mut check_runs_etag: Option<String> = None;
    let mut cached_check_runs: Vec<GitHubCheckRun> = Vec::new();
    let mut workflow_runs_etag: Option<String> = None;
    let mut cached_workflow_runs: Vec<GitHubWorkflowRun> = Vec::new();
    let mut jobs_cache: HashMap<u64, (Option<String>, Vec<GitHubActionsJob>)> = HashMap::new();
    let mut pr_etag: Option<String> = None;
    let mut failure_log_cache: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();

    // Track last seen `(status, conclusion)` per check name so `on_event` only fires on deltas.
    let mut seen_states: HashMap<String, (String, Option<String>)> = HashMap::new();
    let mut emitted_waiting_for_checks = false;

    loop {
        polls = polls.saturating_add(1);
        let mut rate_limit_wait: Option<u64> = None;
        let mut superseded_by_sha: Option<String> = None;

        // 1. Check PR head SHA drift when watching a pull request.
        if let Some(pr_num) = resolved_pr_num
            && !commit_sha.is_empty()
            && polls > 1
        {
            match client.get_pull_request_conditional(owner, gh_repo, pr_num, pr_etag.as_deref()) {
                Ok(ConditionalGet::Modified { value, etag, .. }) => {
                    if etag.is_some() {
                        pr_etag = etag;
                    }
                    if !value.head.sha.is_empty() {
                        if value.head.sha == commit_sha {
                            // PR API has caught up to the pushed commit.
                            initial_stale_pr_sha = None;
                        } else if is_sha_ancestor_of_watched(
                            gix_repo.as_ref(),
                            &value.head.sha,
                            &commit_sha,
                        ) || initial_stale_pr_sha.as_deref() == Some(value.head.sha.as_str())
                        {
                            // GitHub's PR API (`pulls/{n}.head.sha`) can lag behind `git push`
                            // for several seconds; an ancestor or pre-push stale SHA is API lag,
                            // not a newer superseding push.
                        } else {
                            superseded_by_sha = Some(value.head.sha);
                        }
                    }
                }
                Ok(ConditionalGet::NotModified { etag, .. }) => {
                    if etag.is_some() {
                        pr_etag = etag;
                    }
                }
                Ok(ConditionalGet::RateLimited { retry_after_secs, .. }) => {
                    rate_limit_wait = Some(rate_limit_wait.unwrap_or(0).max(retry_after_secs));
                }
                Err(_) => {}
            }
        }

        // 2. Conditionally fetch check runs for the target commit/ref.
        match client.list_check_runs_for_ref_conditional(
            owner,
            gh_repo,
            &query_ref,
            check_runs_etag.as_deref(),
        ) {
            Ok(ConditionalGet::Modified { value, etag, .. }) => {
                cached_check_runs = value;
                if etag.is_some() {
                    check_runs_etag = etag;
                }
            }
            Ok(ConditionalGet::NotModified { etag, .. }) => {
                if etag.is_some() {
                    check_runs_etag = etag;
                }
            }
            Ok(ConditionalGet::RateLimited { retry_after_secs, .. }) => {
                rate_limit_wait = Some(rate_limit_wait.unwrap_or(0).max(retry_after_secs));
            }
            Err(err) if polls == 1 => return Err(err),
            Err(_) => {}
        }

        // 3. Conditionally fetch workflow runs & their jobs.
        match client.list_workflow_runs_conditional(
            owner,
            gh_repo,
            branch_filter,
            wf_sha_filter,
            workflow_runs_etag.as_deref(),
        ) {
            Ok(ConditionalGet::Modified { value, etag, .. }) => {
                cached_workflow_runs = value;
                if etag.is_some() {
                    workflow_runs_etag = etag;
                }
            }
            Ok(ConditionalGet::NotModified { etag, .. }) => {
                if etag.is_some() {
                    workflow_runs_etag = etag;
                }
            }
            Ok(ConditionalGet::RateLimited { retry_after_secs, .. }) => {
                rate_limit_wait = Some(rate_limit_wait.unwrap_or(0).max(retry_after_secs));
            }
            Err(_) => {}
        }

        // Track whether we have observed a workflow run for `commit_sha`, and record pre-existing
        // older runs on poll 1 (before the newly pushed run registers) so we never mistake them
        // for a newer superseding commit.
        if !commit_sha.is_empty() && cached_workflow_runs.iter().any(|r| r.head_sha == commit_sha) {
            seen_watched_workflow_run = true;
        }
        if polls == 1 && !seen_watched_workflow_run {
            for r in &cached_workflow_runs {
                if r.head_sha != commit_sha {
                    preexisting_other_run_ids.insert(r.id);
                }
            }
        }

        // Detect newer commit pushed to the same branch from workflow runs (ordered newest-first:
        // only runs appearing *strictly before* any run for `commit_sha` can supersede it, and
        // any ancestor / pre-push stale SHA is ignored as API lag).
        if superseded_by_sha.is_none()
            && branch_filter.is_some()
            && !commit_sha.is_empty()
            && let Some(newer_run) = cached_workflow_runs
                .iter()
                .take_while(|r| r.head_sha != commit_sha)
                .find(|r| {
                    r.head_sha != commit_sha
                        && !r.head_sha.is_empty()
                        && r.head_branch.as_deref() == branch_filter
                        && !preexisting_other_run_ids.contains(&r.id)
                        && initial_stale_pr_sha.as_deref() != Some(r.head_sha.as_str())
                        && !is_sha_ancestor_of_watched(gix_repo.as_ref(), &r.head_sha, &commit_sha)
                })
        {
            // Only treat a newer workflow run as superseding if checks on the watched SHA are
            // cancelled or still non-passing (avoid false positives if someone inspects an older
            // already-green commit by branch name).
            let any_cancelled = cached_check_runs
                .iter()
                .any(|c| c.conclusion.as_deref() == Some("cancelled"));
            let all_completed_passed = !cached_check_runs.is_empty()
                && cached_check_runs
                    .iter()
                    .all(|c| c.status == "completed" && !is_failing_conclusion(c.conclusion.as_deref()));
            if any_cancelled || !all_completed_passed {
                superseded_by_sha = Some(newer_run.head_sha.clone());
            }
        }

        let mut jobs_by_id: HashMap<u64, GitHubActionsJob> = HashMap::new();
        let mut jobs_by_name: HashMap<String, GitHubActionsJob> = HashMap::new();

        for run in cached_workflow_runs.iter().take(10) {
            if !commit_sha.is_empty() && run.head_sha != commit_sha {
                continue;
            }
            let prev_etag = jobs_cache.get(&run.id).and_then(|(e, _)| e.clone());
            match client.list_jobs_for_workflow_run_conditional(
                owner,
                gh_repo,
                run.id,
                prev_etag.as_deref(),
            ) {
                Ok(ConditionalGet::Modified { value, etag, .. }) => {
                    jobs_cache.insert(run.id, (etag.or(prev_etag), value));
                }
                Ok(ConditionalGet::NotModified { etag, .. }) => {
                    if let Some(entry) = jobs_cache.get_mut(&run.id)
                        && etag.is_some()
                    {
                        entry.0 = etag;
                    }
                }
                Ok(ConditionalGet::RateLimited { retry_after_secs, .. }) => {
                    rate_limit_wait = Some(rate_limit_wait.unwrap_or(0).max(retry_after_secs));
                }
                Err(_) => {}
            }
            if let Some((_, jobs)) = jobs_cache.get(&run.id) {
                for job in jobs {
                    jobs_by_id.insert(job.id, job.clone());
                    jobs_by_name
                        .entry(job.name.clone())
                        .or_insert_with(|| job.clone());
                }
            }
        }

        // If superseded by a newer commit on the branch/PR, finalize immediately without
        // downloading cancelled job logs as false failures.
        if let Some(new_sha) = superseded_by_sha {
            let elapsed_secs = start.elapsed().as_secs();
            on_event(CiWatchEvent::Superseded {
                old_sha: commit_sha.clone(),
                new_sha: new_sha.clone(),
            });
            let mut report = assemble_ci_report_inner(
                &client,
                owner,
                gh_repo,
                ref_name,
                commit_sha,
                resolved_pr_num,
                &cached_check_runs,
                &jobs_by_id,
                &jobs_by_name,
                &query.include_checks,
                &query.ignore_checks,
                max_lines,
                &mut failure_log_cache,
                false,
            );
            report.overall_status = "superseded".to_string();
            report.all_passed = false;
            report.watch = Some(CiWatchMetadata {
                outcome: CiWatchOutcome::Superseded,
                elapsed_secs,
                polls,
                stopped_early: true,
                superseded_by_sha: Some(new_sha),
                recommended_poll_secs: None,
            });
            on_event(CiWatchEvent::Completed {
                outcome: CiWatchOutcome::Superseded,
                elapsed_secs,
                total_count: report.total_count,
                failing_count: report.failing_count,
                pending_count: report.pending_count,
            });
            return Ok(report);
        }

        let mut report = assemble_ci_report(
            &client,
            owner,
            gh_repo,
            ref_name.clone(),
            commit_sha.clone(),
            resolved_pr_num,
            &cached_check_runs,
            &jobs_by_id,
            &jobs_by_name,
            &query.include_checks,
            &query.ignore_checks,
            max_lines,
            &mut failure_log_cache,
        );

        let elapsed = start.elapsed();
        let elapsed_secs = elapsed.as_secs();

        // Emit incremental state-change events for checks.
        if report.total_count == 0 {
            if !emitted_waiting_for_checks {
                emitted_waiting_for_checks = true;
                on_event(CiWatchEvent::WaitingForChecks {
                    ref_name: ref_name.clone(),
                    commit_sha: commit_sha.clone(),
                    elapsed_secs,
                });
            }
        } else {
            for check in &report.checks {
                let state = (check.status.clone(), check.conclusion.clone());
                let changed = seen_states.get(&check.name) != Some(&state);
                if changed {
                    seen_states.insert(check.name.clone(), state);
                    on_event(CiWatchEvent::CheckUpdated {
                        name: check.name.clone(),
                        status: check.status.clone(),
                        conclusion: check.conclusion.clone(),
                        failing_step: check.failing_step.clone(),
                    });
                }
            }
        }

        // Check terminal conditions.
        if report.total_count > 0 {
            if report.failing_count > 0 && opts.fail_fast {
                let stopped_early = report.pending_count > 0;
                let outcome = if stopped_early {
                    CiWatchOutcome::FailedFast
                } else {
                    CiWatchOutcome::Failed
                };
                report.watch = Some(CiWatchMetadata {
                    outcome,
                    elapsed_secs,
                    polls,
                    stopped_early,
                    superseded_by_sha: None,
                    recommended_poll_secs: None,
                });
                on_event(CiWatchEvent::Completed {
                    outcome,
                    elapsed_secs,
                    total_count: report.total_count,
                    failing_count: report.failing_count,
                    pending_count: report.pending_count,
                });
                return Ok(report);
            }

            if report.pending_count == 0 {
                let outcome = if report.failing_count > 0 {
                    CiWatchOutcome::Failed
                } else {
                    CiWatchOutcome::Passed
                };
                report.watch = Some(CiWatchMetadata {
                    outcome,
                    elapsed_secs,
                    polls,
                    stopped_early: false,
                    superseded_by_sha: None,
                    recommended_poll_secs: None,
                });
                on_event(CiWatchEvent::Completed {
                    outcome,
                    elapsed_secs,
                    total_count: report.total_count,
                    failing_count: report.failing_count,
                    pending_count: report.pending_count,
                });
                return Ok(report);
            }
        } else if elapsed >= opts.startup_timeout {
            let outcome = CiWatchOutcome::NoChecksTimedOut;
            report.watch = Some(CiWatchMetadata {
                outcome,
                elapsed_secs,
                polls,
                stopped_early: false,
                superseded_by_sha: None,
                recommended_poll_secs: Some(10),
            });
            on_event(CiWatchEvent::Completed {
                outcome,
                elapsed_secs,
                total_count: 0,
                failing_count: 0,
                pending_count: 0,
            });
            return Ok(report);
        }

        if elapsed >= opts.timeout {
            let outcome = if report.total_count == 0 {
                CiWatchOutcome::NoChecksTimedOut
            } else {
                CiWatchOutcome::TimedOut
            };
            let recommended = recommended_poll_secs(&report.overall_status, elapsed_secs);
            report.watch = Some(CiWatchMetadata {
                outcome,
                elapsed_secs,
                polls,
                stopped_early: false,
                superseded_by_sha: None,
                recommended_poll_secs: recommended,
            });
            on_event(CiWatchEvent::Completed {
                outcome,
                elapsed_secs,
                total_count: report.total_count,
                failing_count: report.failing_count,
                pending_count: report.pending_count,
            });
            return Ok(report);
        }

        // Compute next sleep duration (rate-limit backoff takes precedence, clamped to remaining budget).
        let base_sleep = adaptive_poll_interval(polls.saturating_sub(1), opts.poll_interval);
        let desired_sleep = if let Some(rl_secs) = rate_limit_wait {
            on_event(CiWatchEvent::RateLimited {
                retry_after_secs: rl_secs,
            });
            Duration::from_secs(rl_secs).max(base_sleep)
        } else {
            base_sleep
        };

        let effective_deadline = if report.total_count == 0 {
            opts.startup_timeout.min(opts.timeout)
        } else {
            opts.timeout
        };
        let remaining = effective_deadline.saturating_sub(elapsed);
        if remaining.is_zero() {
            continue;
        }
        std::thread::sleep(desired_sleep.min(remaining));
    }
}

/// Fetches logs for a single GitHub Actions job (`oz ci --job <id>`) with optional step
/// filtering, expanded line budget, and raw cleaned-tail output.
pub fn query_ci_job_log(repo: &OzoneRepo, opts: &CiJobLogOptions) -> Result<CiJobLogReport> {
    if opts.job_id == 0 {
        return Err(OzError::InvalidUsage {
            message: "`--job` requires a positive GitHub Actions job ID".to_string(),
            recovery: vec!["oz ci --json".to_string()],
        });
    }
    let remote_name = opts
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("origin");
    let max_lines = if opts.max_excerpt_lines == 0 {
        DEFAULT_MAX_EXCERPT_LINES
    } else {
        opts.max_excerpt_lines
    };

    let (client, parsed_remote) = GitHubClient::for_repo(
        repo,
        Some(remote_name),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let _ = client.require_authenticated()?;
    let (owner, gh_repo) = parsed_remote.require_owner_repo()?;

    let job = client.get_actions_job(owner, gh_repo, opts.job_id)?;
    let failing_step = job
        .steps
        .iter()
        .find(|st| is_failing_conclusion(st.conclusion.as_deref()))
        .map(|st| st.name.clone());
    let target_step = opts
        .step
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| failing_step.clone());

    let raw_bytes = client.download_job_logs(owner, gh_repo, opts.job_id)?;
    let decoded = extract_log_text_from_bytes(&raw_bytes, target_step.as_deref())?;

    let (inferred_step, excerpt) = if opts.raw {
        (
            target_step.clone(),
            extract_cleaned_log_tail(&decoded, target_step.as_deref(), max_lines),
        )
    } else {
        summarize_failing_log(&decoded, target_step.as_deref(), max_lines)
    };

    let run_id = job
        .run_id
        .or_else(|| extract_actions_run_id_from_url(job.html_url.as_deref()));

    Ok(CiJobLogReport {
        job_id: job.id,
        run_id,
        name: job.name,
        status: job.status,
        conclusion: job.conclusion,
        html_url: job.html_url,
        failing_step: failing_step.or(inferred_step),
        selected_step: target_step,
        raw: opts.raw,
        log_excerpt: excerpt,
    })
}

#[expect(clippy::too_many_arguments)]
fn assemble_ci_report(
    client: &GitHubClient,
    owner: &str,
    gh_repo: &str,
    ref_name: String,
    commit_sha: String,
    pr_number: Option<u64>,
    check_runs: &[GitHubCheckRun],
    jobs_by_id: &HashMap<u64, GitHubActionsJob>,
    jobs_by_name: &HashMap<String, GitHubActionsJob>,
    include_checks: &[String],
    ignore_checks: &[String],
    max_lines: usize,
    failure_cache: &mut HashMap<String, (Option<String>, Option<String>)>,
) -> CiReport {
    assemble_ci_report_inner(
        client,
        owner,
        gh_repo,
        ref_name,
        commit_sha,
        pr_number,
        check_runs,
        jobs_by_id,
        jobs_by_name,
        include_checks,
        ignore_checks,
        max_lines,
        failure_cache,
        true,
    )
}

#[expect(clippy::too_many_arguments)]
fn assemble_ci_report_inner(
    client: &GitHubClient,
    owner: &str,
    gh_repo: &str,
    ref_name: String,
    commit_sha: String,
    pr_number: Option<u64>,
    check_runs: &[GitHubCheckRun],
    jobs_by_id: &HashMap<u64, GitHubActionsJob>,
    jobs_by_name: &HashMap<String, GitHubActionsJob>,
    include_checks: &[String],
    ignore_checks: &[String],
    max_lines: usize,
    failure_cache: &mut HashMap<String, (Option<String>, Option<String>)>,
    fetch_failure_logs: bool,
) -> CiReport {
    let mut summaries: Vec<CiCheckSummary> = Vec::new();
    let mut covered_job_ids: HashSet<u64> = HashSet::new();

    for cr in check_runs {
        let matched_job = extract_actions_job_id_from_url(cr.details_url.as_deref())
            .or_else(|| extract_actions_job_id_from_url(cr.html_url.as_deref()))
            .and_then(|jid| {
                covered_job_ids.insert(jid);
                jobs_by_id
                    .get(&jid)
                    .cloned()
                    .or_else(|| {
                        if fetch_failure_logs && is_failing_conclusion(cr.conclusion.as_deref()) {
                            client.get_actions_job(owner, gh_repo, jid).ok()
                        } else {
                            None
                        }
                    })
            })
            .or_else(|| {
                let j = jobs_by_name.get(&cr.name)?.clone();
                covered_job_ids.insert(j.id);
                Some(j)
            });

        if !check_matches_filters(&cr.name, include_checks, ignore_checks) {
            continue;
        }

        summaries.push(build_check_summary(
            client,
            owner,
            gh_repo,
            Some(cr),
            matched_job.as_ref(),
            max_lines,
            failure_cache,
            fetch_failure_logs,
        ));
    }

    // Append any Actions jobs not already represented in `check_runs`.
    let mut extra_jobs: Vec<&GitHubActionsJob> = jobs_by_id
        .values()
        .filter(|j| !covered_job_ids.contains(&j.id))
        .collect();
    extra_jobs.sort_by_key(|j| j.id);
    for job in extra_jobs {
        if !check_matches_filters(&job.name, include_checks, ignore_checks) {
            continue;
        }
        summaries.push(build_check_summary(
            client,
            owner,
            gh_repo,
            None,
            Some(job),
            max_lines,
            failure_cache,
            fetch_failure_logs,
        ));
    }

    let total_count = summaries.len();
    let failing_count = summaries
        .iter()
        .filter(|s| is_failing_conclusion(s.conclusion.as_deref()))
        .count();
    let pending_count = summaries
        .iter()
        .filter(|s| s.status != "completed")
        .count();

    let overall_status = if total_count == 0 {
        "no_checks".to_string()
    } else if failing_count > 0 {
        "failing".to_string()
    } else if pending_count > 0 {
        "pending".to_string()
    } else {
        "passing".to_string()
    };
    let all_passed = total_count > 0 && failing_count == 0 && pending_count == 0;

    CiReport {
        ref_name,
        commit_sha,
        pr_number,
        overall_status,
        all_passed,
        total_count,
        failing_count,
        pending_count,
        checks: summaries,
        watch: None,
    }
}

fn adaptive_poll_interval(poll_index: u32, fixed: Option<Duration>) -> Duration {
    if let Some(d) = fixed {
        return d;
    }
    match poll_index {
        0..=1 => Duration::from_secs(5),
        2..=4 => Duration::from_secs(10),
        5..=8 => Duration::from_secs(15),
        _ => Duration::from_secs(30),
    }
}

fn recommended_poll_secs(overall_status: &str, elapsed_secs: u64) -> Option<u64> {
    match overall_status {
        "no_checks" => Some(10),
        "pending" => Some(if elapsed_secs >= 120 { 30 } else { 15 }),
        _ => None,
    }
}

pub(crate) fn check_matches_filters(
    name: &str,
    include_checks: &[String],
    ignore_checks: &[String],
) -> bool {
    let included = if include_checks.is_empty() {
        true
    } else {
        include_checks
            .iter()
            .filter(|p| !p.trim().is_empty())
            .any(|pat| pattern_matches_ci_name(pat, name))
    };
    if !included {
        return false;
    }
    !ignore_checks
        .iter()
        .filter(|p| !p.trim().is_empty())
        .any(|pat| pattern_matches_ci_name(pat, name))
}

fn pattern_matches_ci_name(pattern: &str, name: &str) -> bool {
    let p = pattern.trim().to_ascii_lowercase();
    let n = name.to_ascii_lowercase();
    if p.contains('*') || p.contains('?') {
        wildcard_match(&p, &n)
    } else {
        n.contains(&p)
    }
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_idx, mut match_idx) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_idx = Some(pi);
            match_idx = ti;
            pi += 1;
        } else if let Some(si) = star_idx {
            pi = si + 1;
            match_idx += 1;
            ti = match_idx;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn is_sha_ancestor_of_watched(
    gix_repo: Option<&gix::Repository>,
    candidate_sha: &str,
    watched_sha: &str,
) -> bool {
    if candidate_sha.is_empty() || watched_sha.is_empty() || candidate_sha == watched_sha {
        return false;
    }
    let Some(repo) = gix_repo else {
        return false;
    };
    let Ok(cand_oid) = gix::hash::ObjectId::from_hex(candidate_sha.as_bytes()) else {
        return false;
    };
    let Ok(watched_oid) = gix::hash::ObjectId::from_hex(watched_sha.as_bytes()) else {
        return false;
    };
    is_ancestor_commit(repo, cand_oid, watched_oid).unwrap_or(false)
}

fn resolve_ci_target(
    repo: &OzoneRepo,
    client: &GitHubClient,
    owner: &str,
    gh_repo: &str,
    opts: &CiQueryOptions,
) -> Result<(String, String, Option<u64>, Option<String>)> {
    if let Some(pr_num) = opts.pr_number {
        let pr = client.get_pull_request(owner, gh_repo, pr_num)?;
        let sha = pr.head.sha.clone();
        return Ok((pr.head.ref_name, sha.clone(), Some(pr_num), Some(sha)));
    }

    let gix_repo = open_gix_unrestricted(&repo.repo_root)?;
    if let Some(ref explicit) = opts.target_ref {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            let short = normalize_short_branch(trimmed);
            if let Some(oid) = read_ref_oid(&gix_repo, &format!("refs/heads/{short}"))? {
                return Ok((short, oid.to_hex().to_string(), None, None));
            }
            if let Ok(oid) = gix_repo.rev_parse_single(trimmed) {
                return Ok((trimmed.to_string(), oid.detach().to_hex().to_string(), None, None));
            }
            return Ok((trimmed.to_string(), String::new(), None, None));
        }
    }

    let branch = repo.branch.clone().unwrap_or_else(|| "HEAD".to_string());
    let sha = repo
        .head_oid
        .clone()
        .or_else(|| {
            read_ref_oid(&gix_repo, &format!("refs/heads/{branch}"))
                .ok()
                .flatten()
                .map(|id| id.to_hex().to_string())
        })
        .unwrap_or_default();

    if branch == "HEAD" && sha.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "repository HEAD has no commits to query CI status for".to_string(),
            recovery: vec!["Pass `--pr <num>` or `--ref <sha>`.".to_string()],
        });
    }

    let matched_pr = if branch != "HEAD" {
        let head_filter = format!("{owner}:{branch}");
        client
            .list_pull_requests(owner, gh_repo, Some("open"), Some(&head_filter), None)
            .ok()
            .and_then(|list| list.into_iter().find(|p| p.head.ref_name == branch))
            .map(|p| (p.number, p.head.sha))
    } else {
        None
    };

    let (pr_num, pr_head_sha) = match matched_pr {
        Some((n, s)) => (Some(n), Some(s)),
        None => (None, None),
    };

    Ok((branch, sha, pr_num, pr_head_sha))
}

#[expect(clippy::too_many_arguments)]
fn build_check_summary(
    client: &GitHubClient,
    owner: &str,
    gh_repo: &str,
    check_run: Option<&GitHubCheckRun>,
    job: Option<&GitHubActionsJob>,
    max_lines: usize,
    failure_cache: &mut HashMap<String, (Option<String>, Option<String>)>,
    fetch_failure_logs: bool,
) -> CiCheckSummary {
    let name = check_run
        .map(|c| c.name.clone())
        .or_else(|| job.map(|j| j.name.clone()))
        .unwrap_or_else(|| "unknown".to_string());
    let status = check_run
        .map(|c| c.status.clone())
        .or_else(|| job.map(|j| j.status.clone()))
        .unwrap_or_else(|| "unknown".to_string());
    let conclusion = check_run
        .and_then(|c| c.conclusion.clone())
        .or_else(|| job.and_then(|j| j.conclusion.clone()));

    let job_id_opt = job
        .map(|j| j.id)
        .or_else(|| check_run.and_then(|c| extract_actions_job_id_from_url(c.details_url.as_deref())))
        .or_else(|| check_run.and_then(|c| extract_actions_job_id_from_url(c.html_url.as_deref())));
    let run_id_opt = job
        .and_then(|j| j.run_id)
        .or_else(|| check_run.and_then(|c| extract_actions_run_id_from_url(c.details_url.as_deref())))
        .or_else(|| check_run.and_then(|c| extract_actions_run_id_from_url(c.html_url.as_deref())))
        .or_else(|| job.and_then(|j| extract_actions_run_id_from_url(j.html_url.as_deref())));
    let html_url = check_run
        .and_then(|c| c.html_url.clone().or_else(|| c.details_url.clone()))
        .or_else(|| job.and_then(|j| j.html_url.clone()));

    if !is_failing_conclusion(conclusion.as_deref()) || !fetch_failure_logs {
        return CiCheckSummary {
            name,
            status,
            conclusion,
            failing_step: None,
            log_excerpt: None,
            job_id: job_id_opt,
            run_id: run_id_opt,
            html_url,
        };
    }

    let step_from_job = job.and_then(|j| {
        j.steps
            .iter()
            .find(|st| is_failing_conclusion(st.conclusion.as_deref()))
            .map(|st| st.name.clone())
    });

    let cache_key = job_id_opt
        .map(|jid| format!("job:{jid}"))
        .unwrap_or_else(|| format!("name:{name}"));
    if let Some((cached_step, cached_excerpt)) = failure_cache.get(&cache_key) {
        return CiCheckSummary {
            name,
            status,
            conclusion,
            failing_step: step_from_job.or_else(|| cached_step.clone()),
            log_excerpt: cached_excerpt.clone(),
            job_id: job_id_opt,
            run_id: run_id_opt,
            html_url,
        };
    }

    let mut raw_log_text = None;
    if let Some(jid) = job_id_opt
        && let Ok(bytes) = client.download_job_logs(owner, gh_repo, jid)
        && let Ok(text) = extract_log_text_from_bytes(&bytes, step_from_job.as_deref())
        && !text.trim().is_empty()
    {
        raw_log_text = Some(text);
    }

    if raw_log_text.is_none()
        && let Some(cr) = check_run
    {
        let fallback = [
            cr.output.title.as_deref(),
            cr.output.summary.as_deref(),
            cr.output.text.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
        if !fallback.is_empty() {
            raw_log_text = Some(fallback);
        }
    }

    let (parsed_step, excerpt) = match raw_log_text {
        Some(ref text) => summarize_failing_log(text, step_from_job.as_deref(), max_lines),
        None => (None, None),
    };

    let final_step = step_from_job.or(parsed_step);
    failure_cache.insert(cache_key, (final_step.clone(), excerpt.clone()));

    CiCheckSummary {
        name,
        status,
        conclusion,
        failing_step: final_step,
        log_excerpt: excerpt,
        job_id: job_id_opt,
        run_id: run_id_opt,
        html_url,
    }
}

pub(crate) fn is_failing_conclusion(conclusion: Option<&str>) -> bool {
    matches!(
        conclusion.map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("failure" | "timed_out" | "cancelled" | "startup_failure" | "action_required")
    )
}

fn extract_actions_job_id_from_url(url: Option<&str>) -> Option<u64> {
    let u = url?;
    // Matches `.../actions/runs/<run_id>/job/<job_id>` or `.../jobs/<job_id>`
    if let Some((_, after)) = u.split_once("/job/") {
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(id) = digits.parse::<u64>() {
            return Some(id);
        }
    }
    if let Some((_, after)) = u.split_once("/jobs/") {
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(id) = digits.parse::<u64>() {
            return Some(id);
        }
    }
    None
}

fn extract_actions_run_id_from_url(url: Option<&str>) -> Option<u64> {
    let u = url?;
    if let Some((_, after)) = u.split_once("/actions/runs/") {
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(id) = digits.parse::<u64>() {
            return Some(id);
        }
    }
    None
}

/// Decodes raw job log bytes as either a PKZIP archive (`PK\x03\x04` / `PK\x05\x06`) or UTF-8 text.
pub fn extract_log_text_from_bytes(
    bytes: &[u8],
    preferred_step_name: Option<&str>,
) -> Result<String> {
    if is_zip_archive(bytes) {
        let files = extract_zip_text_entries(bytes)?;
        if files.is_empty() {
            return Ok(String::new());
        }
        if let Some(step) = preferred_step_name {
            let needle = sanitize_step_needle(step);
            if !needle.is_empty() {
                let matching: Vec<&(String, String)> = files
                    .iter()
                    .filter(|(name, _)| sanitize_step_needle(name).contains(&needle))
                    .collect();
                if !matching.is_empty() {
                    return Ok(matching
                        .into_iter()
                        .map(|(_, content)| content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"));
                }
            }
        }
        // Prefer entries containing error markers if multiple files exist in the ZIP.
        let with_errors: Vec<&(String, String)> = files
            .iter()
            .filter(|(_, content)| {
                content.contains("##[error]")
                    || content.contains("error[E")
                    || content.contains("panicked at")
                    || content.contains("FAILED")
            })
            .collect();
        if !with_errors.is_empty() {
            return Ok(with_errors
                .into_iter()
                .map(|(_, content)| content.as_str())
                .collect::<Vec<_>>()
                .join("\n"));
        }

        return Ok(files
            .into_iter()
            .map(|(_, content)| content)
            .collect::<Vec<_>>()
            .join("\n"));
    }

    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn sanitize_step_needle(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_zip_archive(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && (bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06"))
}

/// Pure-Rust PKZIP reader supporting Central Directory (`PK\x05\x06` + `PK\x01\x02`) and Local
/// File Header (`PK\x03\x04`) entries for Stored (`0`) and Deflate (`8`) compression methods.
fn extract_zip_text_entries(bytes: &[u8]) -> Result<Vec<(String, String)>> {
    // 1. Locate End of Central Directory (`PK\x05\x06`) scanning backwards from end of archive.
    let min_eocd_len = 22usize;
    if bytes.len() >= min_eocd_len {
        let search_start = bytes.len().saturating_sub(65_557);
        let mut eocd_pos = None;
        for idx in (search_start..=bytes.len() - min_eocd_len).rev() {
            if &bytes[idx..idx + 4] == b"PK\x05\x06" {
                eocd_pos = Some(idx);
                break;
            }
        }

        if let Some(eocd) = eocd_pos {
            let total_entries = u16::from_le_bytes([bytes[eocd + 10], bytes[eocd + 11]]) as usize;
            let cd_offset = u32::from_le_bytes([
                bytes[eocd + 16],
                bytes[eocd + 17],
                bytes[eocd + 18],
                bytes[eocd + 19],
            ]) as usize;

            if cd_offset <= bytes.len() {
                let mut entries = Vec::with_capacity(total_entries);
                let mut cursor = cd_offset;
                for _ in 0..total_entries {
                    if cursor + 46 > bytes.len() || &bytes[cursor..cursor + 4] != b"PK\x01\x02" {
                        break;
                    }
                    let method = u16::from_le_bytes([bytes[cursor + 10], bytes[cursor + 11]]);
                    let comp_size = u32::from_le_bytes([
                        bytes[cursor + 20],
                        bytes[cursor + 21],
                        bytes[cursor + 22],
                        bytes[cursor + 23],
                    ]) as usize;
                    let name_len =
                        u16::from_le_bytes([bytes[cursor + 28], bytes[cursor + 29]]) as usize;
                    let extra_len =
                        u16::from_le_bytes([bytes[cursor + 30], bytes[cursor + 31]]) as usize;
                    let comment_len =
                        u16::from_le_bytes([bytes[cursor + 32], bytes[cursor + 33]]) as usize;
                    let local_hdr_offset = u32::from_le_bytes([
                        bytes[cursor + 42],
                        bytes[cursor + 43],
                        bytes[cursor + 44],
                        bytes[cursor + 45],
                    ]) as usize;

                    let name_end = cursor + 46 + name_len;
                    if name_end > bytes.len() {
                        break;
                    }
                    let file_name = String::from_utf8_lossy(&bytes[cursor + 46..name_end]).into_owned();
                    cursor = name_end + extra_len + comment_len;

                    if file_name.ends_with('/') {
                        continue;
                    }
                    if local_hdr_offset + 30 > bytes.len()
                        || &bytes[local_hdr_offset..local_hdr_offset + 4] != b"PK\x03\x04"
                    {
                        continue;
                    }
                    let l_name_len = u16::from_le_bytes([
                        bytes[local_hdr_offset + 26],
                        bytes[local_hdr_offset + 27],
                    ]) as usize;
                    let l_extra_len = u16::from_le_bytes([
                        bytes[local_hdr_offset + 28],
                        bytes[local_hdr_offset + 29],
                    ]) as usize;
                    let data_start = local_hdr_offset + 30 + l_name_len + l_extra_len;
                    let Some(data_end) = data_start.checked_add(comp_size) else {
                        continue;
                    };
                    if data_end > bytes.len() {
                        continue;
                    }

                    let text = decode_zip_payload(&bytes[data_start..data_end], method)?;
                    entries.push((file_name, text));
                }
                return Ok(entries);
            }
        }
    }

    // Fallback: sequential scan of Local File Headers (`PK\x03\x04`).
    let mut entries = Vec::new();
    let mut pos = 0usize;
    while pos + 30 <= bytes.len() && &bytes[pos..pos + 4] == b"PK\x03\x04" {
        let method = u16::from_le_bytes([bytes[pos + 8], bytes[pos + 9]]);
        let comp_size = u32::from_le_bytes([
            bytes[pos + 18],
            bytes[pos + 19],
            bytes[pos + 20],
            bytes[pos + 21],
        ]) as usize;
        let name_len = u16::from_le_bytes([bytes[pos + 26], bytes[pos + 27]]) as usize;
        let extra_len = u16::from_le_bytes([bytes[pos + 28], bytes[pos + 29]]) as usize;
        let name_start = pos + 30;
        let data_start = name_start + name_len + extra_len;
        let data_end = data_start.saturating_add(comp_size);
        if data_end > bytes.len() {
            break;
        }
        let name = String::from_utf8_lossy(&bytes[name_start..name_start + name_len]).into_owned();
        if !name.ends_with('/') {
            let text = decode_zip_payload(&bytes[data_start..data_end], method)?;
            entries.push((name, text));
        }
        pos = data_end;
    }

    Ok(entries)
}

fn decode_zip_payload(compressed: &[u8], method: u16) -> Result<String> {
    match method {
        0 => Ok(String::from_utf8_lossy(compressed).into_owned()),
        8 => {
            let mut decoder = DeflateDecoder::new(compressed);
            let mut out = Vec::new();
            decoder
                .read_to_end(&mut out)
                .map_err(|err| OzError::Other(format!("failed inflating ZIP log entry: {err}")))?;
            Ok(String::from_utf8_lossy(&out).into_owned())
        }
        other => Err(OzError::Other(format!(
            "unsupported ZIP compression method {other} in CI job logs"
        ))),
    }
}

/// Cleans raw GitHub Actions logs (stripping UTF-8 BOMs, ISO-8601 runner timestamps, and ANSI
/// escape sequences), infers `failing_step` from `##[group]` markers when missing, and extracts
/// a concise, high-signal `log_excerpt` focused on compiler errors, panics, and test failures.
#[must_use]
pub fn summarize_failing_log(
    raw_log: &str,
    known_step: Option<&str>,
    max_lines: usize,
) -> (Option<String>, Option<String>) {
    let mut cleaned_lines: Vec<String> = Vec::new();
    let mut group_for_line: Vec<Option<String>> = Vec::new();
    let mut active_group: Option<String> = None;

    for raw_line in raw_log.lines() {
        let no_bom = raw_line.trim_start_matches('\u{feff}');
        let no_ts = strip_actions_timestamp(no_bom);
        let clean = strip_ansi_escapes(no_ts).trim_end().to_string();
        let trimmed = clean.trim();

        if let Some(group_title) = trimmed.strip_prefix("##[group]") {
            let g = group_title.trim();
            if !g.is_empty() {
                active_group = Some(g.to_string());
            }
            continue;
        }
        if trimmed == "##[endgroup]" || trimmed.starts_with("##[debug]") {
            if trimmed == "##[endgroup]" {
                // Keep `active_group` until next group if an error immediately follows `##[endgroup]`,
                // such as `##[error]Process completed with exit code 1.`.
            }
            continue;
        }
        if trimmed.is_empty() && cleaned_lines.last().is_some_and(String::is_empty) {
            continue;
        }

        let normalized = if let Some(err_body) = trimmed.strip_prefix("##[error]") {
            format!("error: {}", err_body.trim())
        } else {
            clean
        };

        cleaned_lines.push(normalized);
        group_for_line.push(active_group.clone());
    }

    while cleaned_lines.last().is_some_and(|l| l.trim().is_empty()) {
        cleaned_lines.pop();
        group_for_line.pop();
    }

    if cleaned_lines.is_empty() {
        return (known_step.map(str::to_string), None);
    }

    // Find high-signal diagnostic anchors (compiler errors, panics, assertions, FAILED tests, ##[error]).
    let mut selected_indices: Vec<usize> = Vec::new();
    let mut inferred_step: Option<String> = None;

    for (idx, line) in cleaned_lines.iter().enumerate() {
        if is_high_signal_error_line(line) {
            if inferred_step.is_none() {
                inferred_step = group_for_line[idx].clone();
            }
            let start = idx.saturating_sub(1);
            let end = (idx + 4).min(cleaned_lines.len());
            for k in start..end {
                if !selected_indices.contains(&k) {
                    selected_indices.push(k);
                }
            }
        }
    }

    let excerpt_lines: Vec<String> = if !selected_indices.is_empty() {
        selected_indices.sort_unstable();
        selected_indices.dedup();
        selected_indices
            .into_iter()
            .take(max_lines)
            .map(|i| cleaned_lines[i].clone())
            .collect()
    } else {
        let start = cleaned_lines.len().saturating_sub(max_lines);
        if inferred_step.is_none() {
            inferred_step = group_for_line.last().cloned().flatten();
        }
        cleaned_lines[start..].to_vec()
    };

    let max_chars = excerpt_char_budget(max_lines);
    let mut excerpt = excerpt_lines.join("\n").trim().to_string();
    if excerpt.len() > max_chars {
        let mut cut = max_chars;
        while !excerpt.is_char_boundary(cut) {
            cut -= 1;
        }
        excerpt.truncate(cut);
        excerpt.push_str("\n... [truncated]");
    }

    let step = known_step.map(str::to_string).or(inferred_step);
    (step, (!excerpt.is_empty()).then_some(excerpt))
}

fn excerpt_char_budget(max_lines: usize) -> usize {
    if max_lines <= DEFAULT_MAX_EXCERPT_LINES {
        MAX_EXCERPT_CHARS
    } else {
        max_lines
            .saturating_mul(160)
            .clamp(MAX_EXCERPT_CHARS, MAX_EXPANDED_EXCERPT_CHARS)
    }
}

/// Cleans raw GitHub Actions logs (stripping BOM, runner timestamps, ANSI codes, and debug lines)
/// and returns the last `max_lines` lines (optionally scoped to a matching `##[group]` step).
fn extract_cleaned_log_tail(
    raw_log: &str,
    step_filter: Option<&str>,
    max_lines: usize,
) -> Option<String> {
    let step_needle = step_filter.map(sanitize_step_needle).filter(|s| !s.is_empty());
    let mut all_cleaned: Vec<String> = Vec::new();
    let mut step_cleaned: Vec<String> = Vec::new();
    let mut active_group: Option<String> = None;

    for raw_line in raw_log.lines() {
        let no_bom = raw_line.trim_start_matches('\u{feff}');
        let no_ts = strip_actions_timestamp(no_bom);
        let clean = strip_ansi_escapes(no_ts).trim_end().to_string();
        let trimmed = clean.trim();

        if let Some(group_title) = trimmed.strip_prefix("##[group]") {
            let g = group_title.trim();
            if !g.is_empty() {
                active_group = Some(g.to_string());
            }
            continue;
        }
        if trimmed == "##[endgroup]" || trimmed.starts_with("##[debug]") {
            continue;
        }
        let normalized = if let Some(err_body) = trimmed.strip_prefix("##[error]") {
            format!("error: {}", err_body.trim())
        } else {
            clean
        };

        if let Some(ref needle) = step_needle
            && active_group
                .as_deref()
                .is_some_and(|g| sanitize_step_needle(g).contains(needle))
        {
            step_cleaned.push(normalized.clone());
        }
        all_cleaned.push(normalized);
    }

    let source = if !step_cleaned.is_empty() {
        step_cleaned
    } else {
        all_cleaned
    };
    if source.is_empty() {
        return None;
    }

    let start = source.len().saturating_sub(max_lines);
    let mut tail = source[start..].join("\n").trim().to_string();
    let max_chars = excerpt_char_budget(max_lines);
    if tail.len() > max_chars {
        let mut cut = tail.len() - max_chars;
        while !tail.is_char_boundary(cut) {
            cut += 1;
        }
        tail = format!("... [truncated]\n{}", &tail[cut..]);
    }
    (!tail.is_empty()).then_some(tail)
}

fn is_high_signal_error_line(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("error[E")
        || t.starts_with("error:")
        || t.starts_with("Error:")
        || t.contains("panicked at")
        || t.starts_with("thread '") && t.contains("panicked")
        || (t.starts_with("test ") && t.ends_with("... FAILED"))
        || t.starts_with("FAIL ")
        || t.starts_with("--- FAIL:")
        || t.contains("AssertionError")
        || t.starts_with("Traceback (most recent call last):")
        || t.starts_with("Caused by:")
}

/// Strips a leading GitHub Actions runner timestamp like `2026-11-18T13:20:01.1234567Z `.
#[must_use]
pub fn strip_actions_timestamp(line: &str) -> &str {
    let bytes = line.as_bytes();
    // Minimum `YYYY-MM-DDTHH:MM:SSZ ` is 21 bytes.
    if bytes.len() >= 21
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && (bytes[10] == b'T' || bytes[10] == b' ')
        && bytes[11..13].iter().all(u8::is_ascii_digit)
        && bytes[13] == b':'
        && bytes[14..16].iter().all(u8::is_ascii_digit)
        && bytes[16] == b':'
        && bytes[17..19].iter().all(u8::is_ascii_digit)
        && let Some(space_pos) = line[19..].find(' ')
    {
        let z_slice = &line[19..19 + space_pos];
        if z_slice == "Z"
            || (z_slice.starts_with('.')
                && z_slice.ends_with('Z')
                && z_slice[1..z_slice.len() - 1]
                    .bytes()
                    .all(|b| b.is_ascii_digit()))
        {
            return &line[19 + space_pos + 1..];
        }
    }
    line
}

/// Strips ANSI CSI (`\x1b[...m`) and OSC (`\x1b]...\x07`) escape sequences.
#[must_use]
pub fn strip_ansi_escapes(input: &str) -> String {
    if !input.contains('\x1b') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    let _ = chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    let _ = chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            let _ = chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::DeflateEncoder;
    use std::io::Write;

    fn build_single_file_zip_deflate(filename: &str, content: &[u8]) -> Vec<u8> {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(content).expect("deflate");
        let compressed = encoder.finish().expect("finish deflate");

        let name_bytes = filename.as_bytes();
        let comp_len = compressed.len() as u32;
        let uncomp_len = content.len() as u32;

        let mut zip = Vec::new();
        // Local File Header (`PK\x03\x04`)
        zip.extend_from_slice(b"PK\x03\x04");
        zip.extend_from_slice(&20u16.to_le_bytes()); // version needed
        zip.extend_from_slice(&0u16.to_le_bytes()); // flags
        zip.extend_from_slice(&8u16.to_le_bytes()); // method = 8 (Deflate)
        zip.extend_from_slice(&0u16.to_le_bytes()); // mod time
        zip.extend_from_slice(&0u16.to_le_bytes()); // mod date
        zip.extend_from_slice(&0u32.to_le_bytes()); // crc32 (unchecked)
        zip.extend_from_slice(&comp_len.to_le_bytes());
        zip.extend_from_slice(&uncomp_len.to_le_bytes());
        zip.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes()); // extra len
        zip.extend_from_slice(name_bytes);
        zip.extend_from_slice(&compressed);

        let cd_offset = zip.len() as u32;
        // Central Directory Header (`PK\x01\x02`)
        zip.extend_from_slice(b"PK\x01\x02");
        zip.extend_from_slice(&20u16.to_le_bytes()); // version made by
        zip.extend_from_slice(&20u16.to_le_bytes()); // version needed
        zip.extend_from_slice(&0u16.to_le_bytes()); // flags
        zip.extend_from_slice(&8u16.to_le_bytes()); // method = 8
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u32.to_le_bytes());
        zip.extend_from_slice(&comp_len.to_le_bytes());
        zip.extend_from_slice(&uncomp_len.to_le_bytes());
        zip.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u32.to_le_bytes());
        zip.extend_from_slice(&0u32.to_le_bytes()); // local header offset = 0
        zip.extend_from_slice(name_bytes);

        let cd_size = (zip.len() as u32) - cd_offset;
        // End of Central Directory (`PK\x05\x06`)
        zip.extend_from_slice(b"PK\x05\x06");
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&1u16.to_le_bytes());
        zip.extend_from_slice(&1u16.to_le_bytes());
        zip.extend_from_slice(&cd_size.to_le_bytes());
        zip.extend_from_slice(&cd_offset.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip
    }

    #[test]
    fn extracts_zip_and_plain_logs_stripping_timestamps_and_ansi() {
        let raw_log = "\u{feff}2026-11-18T13:20:00.1111111Z ##[group]Run cargo test\n\
2026-11-18T13:20:01.2222222Z    Compiling demo v0.1.0\n\
2026-11-18T13:20:02.3333333Z \x1b[31merror[E0425]\x1b[0m: cannot find value `missing_var` in this scope\n\
2026-11-18T13:20:02.3333334Z   --> src/lib.rs:42:9\n\
2026-11-18T13:20:02.4444444Z ##[endgroup]\n\
2026-11-18T13:20:02.5555555Z ##[error]Process completed with exit code 101.\n";

        let zip_bytes = build_single_file_zip_deflate("build/4_Run cargo test.txt", raw_log.as_bytes());
        let decoded = extract_log_text_from_bytes(&zip_bytes, Some("Run cargo test"))
            .expect("decode zip log");

        let (step, excerpt_opt) = summarize_failing_log(&decoded, None, 20);
        assert_eq!(step.as_deref(), Some("Run cargo test"));
        let excerpt = excerpt_opt.expect("excerpt");
        assert!(!excerpt.contains("2026-11-18T"));
        assert!(!excerpt.contains('\x1b'));
        assert!(excerpt.contains("error[E0425]: cannot find value `missing_var` in this scope"));
        assert!(excerpt.contains("--> src/lib.rs:42:9"));
    }

    #[test]
    fn check_name_filtering_supports_substrings_and_globs() {
        assert!(check_matches_filters(
            "Rust / cargo-clippy (ubuntu-latest)",
            &["clippy".to_string()],
            &[]
        ));
        assert!(!check_matches_filters(
            "Rust / cargo-test (ubuntu-latest)",
            &["clippy".to_string()],
            &[]
        ));
        assert!(check_matches_filters(
            "Rust / cargo-test (ubuntu-latest)",
            &["*ubuntu*".to_string()],
            &["*macos*".to_string()]
        ));
        assert!(!check_matches_filters(
            "codecov/patch",
            &[],
            &["codecov".to_string()]
        ));
    }

    #[test]
    fn extracts_run_id_and_raw_log_tail() {
        assert_eq!(
            extract_actions_run_id_from_url(Some(
                "https://github.com/acme/rocket/actions/runs/500123/job/9001"
            )),
            Some(500_123)
        );

        let raw_log = "2026-11-18T13:20:00Z ##[group]Build\n\
2026-11-18T13:20:01Z line 1\n\
2026-11-18T13:20:02Z line 2\n\
2026-11-18T13:20:03Z ##[endgroup]\n\
2026-11-18T13:20:04Z ##[group]Run tests\n\
2026-11-18T13:20:05Z \x1b[32mok\x1b[0m test_a\n\
2026-11-18T13:20:06Z \x1b[31mfail\x1b[0m test_b\n\
2026-11-18T13:20:07Z summary line\n";
        let tail = extract_cleaned_log_tail(raw_log, Some("Run tests"), 2).expect("tail");
        assert_eq!(tail, "fail test_b\nsummary line");
    }
}
