// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! GitHub Issue inspection (`oz issue view <number|url>`) backed by GitHub REST API,
//! avoiding deprecated classic GraphQL `projectCards` fields.

use crate::api::{GitHubClient, GitHubIssue};
use crate::pr::{PrConversationComment, map_issue_comments};
use oz_core::{OzError, OzoneRepo, Result};
use serde::{Deserialize, Serialize};

/// Parsed GitHub issue reference from a number (`3311`, `#3311`) or URL (`https://github.com/o/r/issues/3311`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedIssueTarget {
    pub owner: Option<String>,
    pub repo: Option<String>,
    pub number: u64,
}

/// Parses an issue number (`3311`, `#3311`) or full GitHub issue/PR URL into a [`ParsedIssueTarget`].
pub fn parse_issue_target(raw: &str) -> Result<ParsedIssueTarget> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "issue number or URL cannot be empty".to_string(),
            recovery: vec!["oz issue view 3311".to_string()],
        });
    }

    let stripped_hash = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if let Ok(num) = stripped_hash.parse::<u64>()
        && num > 0
    {
        return Ok(ParsedIssueTarget {
            owner: None,
            repo: None,
            number: num,
        });
    }

    if let Ok(parsed_url) = url::Url::parse(trimmed) {
        let segs: Vec<&str> = parsed_url
            .path_segments()
            .map(|it| it.filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        if segs.len() >= 4
            && matches!(segs[2], "issues" | "pull")
            && let Ok(num) = segs[3].parse::<u64>()
            && num > 0
        {
            return Ok(ParsedIssueTarget {
                owner: Some(segs[0].to_string()),
                repo: Some(segs[1].to_string()),
                number: num,
            });
        }
    }

    Err(OzError::InvalidUsage {
        message: format!(
            "invalid issue target '{raw}': expected an issue number (e.g. `3311`) or GitHub issue URL"
        ),
        recovery: vec![
            "oz issue view 3311".to_string(),
            "oz issue view https://github.com/<owner>/<repo>/issues/3311".to_string(),
        ],
    })
}

/// Options controlling [`issue_view`].
#[derive(Debug, Clone, Default)]
pub struct IssueViewOptions {
    pub number: u64,
    pub owner_override: Option<String>,
    pub repo_override: Option<String>,
    pub include_comments: bool,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

/// Result returned by [`issue_view`] (`oz issue view <number>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueViewOutcome {
    pub owner: String,
    pub repo: String,
    pub issue: GitHubIssue,
    pub is_pull_request: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<PrConversationComment>,
}

/// Fetches a GitHub issue via REST (`GET /repos/{owner}/{repo}/issues/{number}`), optionally
/// including its conversation comments (`--comments`).
pub fn issue_view(repo: &OzoneRepo, opts: &IssueViewOptions) -> Result<IssueViewOutcome> {
    if opts.number == 0 {
        return Err(OzError::InvalidUsage {
            message: "issue number must be positive".to_string(),
            recovery: vec!["oz issue view 3311".to_string()],
        });
    }

    let (client, remote) = GitHubClient::for_repo(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;

    let owner = opts
        .owner_override
        .clone()
        .or(remote.owner)
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!(
                "remote '{}' does not contain a GitHub owner/repo slug",
                remote.sanitized_url
            ),
            recovery: vec!["Verify `git remote -v` points to a GitHub repository.".to_string()],
        })?;
    let repo_name =
        opts.repo_override
            .clone()
            .or(remote.repo)
            .ok_or_else(|| OzError::InvalidUsage {
                message: format!(
                    "remote '{}' does not contain a GitHub repository name",
                    remote.sanitized_url
                ),
                recovery: vec!["Verify `git remote -v` points to a GitHub repository.".to_string()],
            })?;

    let issue = client.get_issue(&owner, &repo_name, opts.number)?;
    let is_pull_request = issue.pull_request.is_some();
    let comments = if opts.include_comments {
        let raw = client
            .list_issue_comments(&owner, &repo_name, opts.number)
            .unwrap_or_default();
        map_issue_comments(raw)
    } else {
        Vec::new()
    };

    Ok(IssueViewOutcome {
        owner,
        repo: repo_name,
        issue,
        is_pull_request,
        comments,
    })
}

/// Options controlling [`issue_list`] (`oz issue list`).
#[derive(Debug, Clone)]
pub struct IssueListOptions {
    /// Issue state filter (`open`, `closed`, `all`; defaults to `open`).
    pub state: Option<String>,
    /// Optional GitHub search query text (`--search`).
    pub search: Option<String>,
    /// Optional author login filter (`--author`).
    pub author: Option<String>,
    /// Optional assignee login filter (`--assignee`).
    pub assignee: Option<String>,
    /// Optional label filters (`--label`).
    pub labels: Vec<String>,
    /// Maximum number of issues to return (defaults to `20`).
    pub limit: usize,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

impl Default for IssueListOptions {
    fn default() -> Self {
        Self {
            state: None,
            search: None,
            author: None,
            assignee: None,
            labels: Vec::new(),
            limit: 20,
            remote: None,
            auth_token: None,
            api_url: None,
        }
    }
}

/// Compact issue summary returned by [`issue_list`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueListItem {
    pub number: u64,
    pub state: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<String>,
    pub comments: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub html_url: String,
}

/// Result returned by [`issue_list`] (`oz issue list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueListOutcome {
    pub owner: String,
    pub repo: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    pub total_returned: usize,
    pub issues: Vec<IssueListItem>,
}

/// Lists or searches GitHub issues via REST (`oz issue list`).
pub fn issue_list(repo: &OzoneRepo, opts: &IssueListOptions) -> Result<IssueListOutcome> {
    let (client, remote) = GitHubClient::for_repo(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let (owner, repo_name) = remote.require_owner_repo()?;
    let state = opts
        .state
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "open".to_string());
    if !matches!(state.as_str(), "open" | "closed" | "all") {
        return Err(OzError::InvalidUsage {
            message: format!("invalid issue state '{state}': expected open, closed, or all"),
            recovery: vec!["oz issue list --state open".to_string()],
        });
    }
    let limit = if opts.limit == 0 {
        20
    } else {
        opts.limit.min(100)
    };

    let use_search = opts.search.as_deref().is_some_and(|s| !s.trim().is_empty())
        || opts.author.as_deref().is_some_and(|s| !s.trim().is_empty());

    let raw_issues = if use_search {
        let mut q_parts = vec![format!("repo:{owner}/{repo_name}"), "is:issue".to_string()];
        if state != "all" {
            q_parts.push(format!("is:{state}"));
        }
        if let Some(ref author) = opts.author {
            let trimmed = author.trim().trim_start_matches('@');
            if !trimmed.is_empty() {
                q_parts.push(format!("author:{trimmed}"));
            }
        }
        if let Some(ref assignee) = opts.assignee {
            let trimmed = assignee.trim().trim_start_matches('@');
            if !trimmed.is_empty() {
                q_parts.push(format!("assignee:{trimmed}"));
            }
        }
        for label in &opts.labels {
            let trimmed = label.trim();
            if !trimmed.is_empty() {
                if trimmed.contains(' ') {
                    q_parts.push(format!("label:\"{trimmed}\""));
                } else {
                    q_parts.push(format!("label:{trimmed}"));
                }
            }
        }
        if let Some(ref search) = opts.search {
            let trimmed = search.trim();
            if !trimmed.is_empty() {
                q_parts.push(trimmed.to_string());
            }
        }
        let resp = client.search_issues_and_prs(&q_parts.join(" "), limit)?;
        resp.items
    } else {
        // GitHub's `/repos/{owner}/{repo}/issues` includes PRs; fetch extra so filtering leaves up to `limit`.
        let fetch_limit = (limit.saturating_mul(2)).clamp(limit, 100);
        client
            .list_issues(
                owner,
                repo_name,
                Some(&state),
                &opts.labels,
                opts.assignee.as_deref(),
                fetch_limit,
            )?
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .take(limit)
            .collect()
    };

    let issues: Vec<IssueListItem> = raw_issues
        .into_iter()
        .filter(|i| i.pull_request.is_none())
        .take(limit)
        .map(|i| IssueListItem {
            number: i.number,
            state: i.state,
            title: i.title,
            author: i.user.map(|u| u.login),
            labels: i.labels.into_iter().map(|l| l.name).collect(),
            assignees: i.assignees.into_iter().map(|u| u.login).collect(),
            comments: i.comments,
            updated_at: i.updated_at,
            html_url: i.html_url,
        })
        .collect();
    let total_returned = issues.len();

    Ok(IssueListOutcome {
        owner: owner.to_string(),
        repo: repo_name.to_string(),
        state,
        search: opts.search.clone(),
        total_returned,
        issues,
    })
}

/// Options controlling [`issue_comment`] (`oz issue comment <number|url>`).
#[derive(Debug, Clone, Default)]
pub struct IssueCommentOptions {
    pub number: u64,
    pub owner_override: Option<String>,
    pub repo_override: Option<String>,
    pub body: String,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

/// Result returned by [`issue_comment`] (`oz issue comment`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueCommentOutcome {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub comment: PrConversationComment,
}

/// Posts a top-level conversation comment on a GitHub issue or pull request.
pub fn issue_comment(repo: &OzoneRepo, opts: &IssueCommentOptions) -> Result<IssueCommentOutcome> {
    if opts.number == 0 {
        return Err(OzError::InvalidUsage {
            message: "issue number must be positive".to_string(),
            recovery: vec!["oz issue comment 3344 --body \"...\"".to_string()],
        });
    }
    if opts.body.trim().is_empty() {
        return Err(OzError::InvalidUsage {
            message: "comment body cannot be empty".to_string(),
            recovery: vec![
                "oz issue comment 3344 --body \"...\"".to_string(),
                "oz issue comment 3344 --body-file -".to_string(),
            ],
        });
    }

    let (client, remote) = GitHubClient::for_repo(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let _ = client.require_authenticated()?;
    let owner = opts
        .owner_override
        .clone()
        .or(remote.owner)
        .ok_or_else(|| OzError::InvalidUsage {
            message: format!(
                "remote '{}' does not contain a GitHub owner/repo slug",
                remote.sanitized_url
            ),
            recovery: vec!["Verify `git remote -v` points to a GitHub repository.".to_string()],
        })?;
    let repo_name =
        opts.repo_override
            .clone()
            .or(remote.repo)
            .ok_or_else(|| OzError::InvalidUsage {
                message: format!(
                    "remote '{}' does not contain a GitHub repository name",
                    remote.sanitized_url
                ),
                recovery: vec!["Verify `git remote -v` points to a GitHub repository.".to_string()],
            })?;

    let created = client.create_issue_comment(&owner, &repo_name, opts.number, &opts.body)?;
    Ok(IssueCommentOutcome {
        owner,
        repo: repo_name,
        number: opts.number,
        comment: PrConversationComment {
            id: created.id,
            author: created.user.map(|u| u.login),
            body: created.body,
            created_at: created.created_at,
            html_url: created.html_url,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_issue_numbers_and_urls() {
        let a = parse_issue_target("3311").unwrap();
        assert_eq!((a.owner, a.repo, a.number), (None, None, 3311));

        let b = parse_issue_target("#3311").unwrap();
        assert_eq!((b.owner, b.repo, b.number), (None, None, 3311));

        let c =
            parse_issue_target("https://github.com/acme/demo/issues/3311#issuecomment-1").unwrap();
        assert_eq!(
            (c.owner.as_deref(), c.repo.as_deref(), c.number),
            (Some("acme"), Some("demo"), 3311)
        );

        assert!(parse_issue_target("not-an-issue").is_err());
        assert!(parse_issue_target("0").is_err());
    }
}
