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
    let repo_name = opts
        .repo_override
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_issue_numbers_and_urls() {
        let a = parse_issue_target("3311").unwrap();
        assert_eq!((a.owner, a.repo, a.number), (None, None, 3311));

        let b = parse_issue_target("#3311").unwrap();
        assert_eq!((b.owner, b.repo, b.number), (None, None, 3311));

        let c = parse_issue_target("https://github.com/acme/demo/issues/3311#issuecomment-1").unwrap();
        assert_eq!(
            (c.owner.as_deref(), c.repo.as_deref(), c.number),
            (Some("acme"), Some("demo"), 3311)
        );

        assert!(parse_issue_target("not-an-issue").is_err());
        assert!(parse_issue_target("0").is_err());
    }
}
