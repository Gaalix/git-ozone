// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pull request creation (`oz pr create`) and stacked PR orchestration (`oz pr stack`)
//! with `Change-Id:` preservation, atomic multi-branch pushes, and deterministic
//! `<!-- oz-stack-start --> ... <!-- oz-stack-end -->` cross-link navigation tables.

use crate::PullRequestSpec;
use crate::api::{
    CreatePullRequestReviewPayload, GitHubClient, GitHubIssueComment, GitHubPullRequest,
    GitHubPullRequestComment, GitHubPullRequestFile, GitHubPullRequestReview,
    GitHubReviewCommentPayload, GitHubReviewThreadMeta, UpdatePullRequest,
};
use crate::wire::{
    HttpAuthScheme, PushOptions, is_ancestor_commit, open_gix_unrestricted, push_refs, read_ref_oid,
};
use gix::bstr::ByteSlice;
use gix::hash::ObjectId;
use oz_core::{
    ChangeKind, DiffHunk, DiffLine, DiffLineKind, DiffReport, FileChange, OzError, OzoneRepo,
    Result, ShowBlobOptions, ShowBlobReport, file_id, filter_changes_by_targets_and_excludes,
    hunk_id, is_binary_bytes, show_blob, slice_text_lines,
};
use oz_integrate::{StackBranchStatus, stack_status};
use oz_oplog::ActorInfo;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

/// Opening HTML comment marker for Ozone's managed PR stack navigation block.
pub const OZ_STACK_START: &str = "<!-- oz-stack-start -->";
/// Closing HTML comment marker for Ozone's managed PR stack navigation block.
pub const OZ_STACK_END: &str = "<!-- oz-stack-end -->";

/// Options controlling [`pr_create`].
#[derive(Debug, Clone, Default)]
pub struct PrCreateOptions {
    /// PR title (inferred from commit(s) if `None`).
    pub title: Option<String>,
    /// PR body markdown (inferred from commit(s) if `None`).
    pub body: Option<String>,
    /// Base branch on GitHub (defaults to `"main"`).
    pub base: Option<String>,
    /// Head branch to open PR from (defaults to active workspace branch).
    pub head: Option<String>,
    /// Create PR as draft (`--draft`).
    pub draft: bool,
    /// Skip pushing `head` branch before creating/updating PR (`--no-push`).
    pub no_push: bool,
    /// Remote name (defaults to `"origin"`).
    pub remote: Option<String>,
    /// Explicit GitHub token override.
    pub auth_token: Option<String>,
    /// Explicit GitHub API base URL override (also honors `OZ_GITHUB_API_URL`).
    pub api_url: Option<String>,
    /// HTTP auth scheme for Git wire push when pushing over HTTP/HTTPS.
    pub git_auth_scheme: HttpAuthScheme,
}

/// Outcome returned by [`pr_create`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrCreateOutcome {
    pub pull_request: GitHubPullRequest,
    pub created: bool,
    pub pushed: bool,
    pub head_branch: String,
    pub base_branch: String,
    pub change_ids: Vec<String>,
}

/// Options controlling [`pr_stack`].
#[derive(Debug, Clone, Default)]
pub struct PrStackOptions {
    /// Root base branch of the stack (defaults to `"main"`).
    pub base: Option<String>,
    /// Optional explicit stack branches (auto-discovered from `base` through current stack if empty).
    pub branches: Vec<String>,
    /// Open newly created PRs as drafts (`--draft`).
    pub draft: bool,
    /// Skip atomic `push_refs` of stack branches (`--no-push`).
    pub no_push: bool,
    /// Remote name (defaults to `"origin"`).
    pub remote: Option<String>,
    /// Explicit GitHub token override.
    pub auth_token: Option<String>,
    /// Explicit GitHub API base URL override.
    pub api_url: Option<String>,
    /// HTTP auth scheme for Git wire push when pushing over HTTP/HTTPS.
    pub git_auth_scheme: HttpAuthScheme,
}

/// Entry describing a single PR in an [`PrStackOutcome`] or stack navigation block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackPrItem {
    pub number: u64,
    pub html_url: String,
    pub title: String,
    pub head_branch: String,
    pub base_branch: String,
    pub status: String,
    pub created: bool,
    pub change_ids: Vec<String>,
}

/// Complete outcome returned by [`pr_stack`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStackOutcome {
    pub root_base: String,
    pub pushed: bool,
    pub atomic_push_op_id: Option<String>,
    pub items: Vec<StackPrItem>,
}

/// Creates or idempotently updates a GitHub Pull Request for `head -> base`, pushing `head`
/// first (unless `opts.no_push` is `true`) and preserving `Change-Id:` metadata.
pub fn pr_create(
    repo: &OzoneRepo,
    opts: &PrCreateOptions,
    actor: ActorInfo,
) -> Result<PrCreateOutcome> {
    let remote_name = opts
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("origin");

    let head_branch = opts
        .head
        .as_deref()
        .map(normalize_short_branch)
        .filter(|s| !s.is_empty())
        .or_else(|| repo.branch.as_deref().map(normalize_short_branch))
        .ok_or_else(|| OzError::InvalidUsage {
            message: "cannot determine PR head branch from detached HEAD; pass `--head <branch>` or switch to a branch"
                .to_string(),
            recovery: vec!["oz status --json".to_string()],
        })?;

    let base_branch = opts
        .base
        .as_deref()
        .map(normalize_short_branch)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string());

    if head_branch == base_branch {
        return Err(OzError::InvalidUsage {
            message: format!(
                "PR head branch '{head_branch}' must differ from base branch '{base_branch}'"
            ),
            recovery: vec![
                "Spawn or switch to a feature branch before running `oz pr create`.".to_string(),
            ],
        });
    }

    let (client, parsed_remote) = GitHubClient::for_repo(
        repo,
        Some(remote_name),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let _ = client.require_authenticated()?;
    let (owner, gh_repo) = parsed_remote.require_owner_repo()?;

    let gix_repo = open_gix_unrestricted(&repo.repo_root)?;
    let (inferred_title, inferred_body, change_ids) =
        infer_title_body_and_change_ids(&gix_repo, &base_branch, &head_branch)?;

    let title = opts
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or(inferred_title, str::to_string);

    let raw_body = opts.body.clone().unwrap_or(inferred_body);
    let body_with_change_id = attach_change_ids_to_body(&raw_body, &change_ids);

    let mut pushed = false;
    if !opts.no_push {
        let needs_lease = branch_needs_lease_push(&gix_repo, remote_name, &head_branch)?;
        let _ = push_refs(
            repo,
            &PushOptions {
                remote: Some(remote_name.to_string()),
                refspecs: vec![head_branch.clone()],
                atomic: false,
                force: false,
                force_with_lease: needs_lease,
                lease_specs: Vec::new(),
                delete: false,
                set_upstream: true,
                auth_token: opts.auth_token.clone(),
                auth_scheme: opts.git_auth_scheme,
            },
            actor,
        )?;
        pushed = true;
    }

    let existing_pr = find_open_pr_for_head(&client, owner, gh_repo, &head_branch)?;
    let (pull_request, created) = if let Some(existing) = existing_pr {
        let existing_body = existing.body.as_deref().unwrap_or_default();
        let preserved_stack_block = extract_existing_stack_block(existing_body);
        let merged_body = match preserved_stack_block {
            Some(block) => upsert_stack_navigation_block(&body_with_change_id, &block),
            None => body_with_change_id,
        };
        let new_base = if opts.base.is_some() && existing.base.ref_name != base_branch {
            Some(base_branch.clone())
        } else {
            None
        };
        let updated = client.update_pull_request(
            owner,
            gh_repo,
            existing.number,
            &UpdatePullRequest {
                title: Some(title),
                body: Some(merged_body),
                base: new_base,
                state: None,
            },
        )?;
        (updated, false)
    } else {
        let created_pr = client.create_pull_request(
            owner,
            gh_repo,
            &PullRequestSpec {
                title,
                body: body_with_change_id,
                head_branch: head_branch.clone(),
                base_branch: base_branch.clone(),
                draft: opts.draft,
            },
        )?;
        (created_pr, true)
    };

    Ok(PrCreateOutcome {
        base_branch: pull_request.base.ref_name.clone(),
        head_branch,
        pull_request,
        created,
        pushed,
        change_ids,
    })
}

/// Discovers the stacked branch chain above `base` (default `"main"`), pushes all stack branches
/// atomically (`push_refs` with `atomic: true`), creates/updates each PR with exact stacked
/// `base_branch` (`main <- stack-1 <- stack-2 ...`), and injects/updates a deterministic
/// cross-linked stack navigation markdown block (`<!-- oz-stack-start --> ... <!-- oz-stack-end -->`)
/// across all PRs in the stack.
pub fn pr_stack(
    repo: &OzoneRepo,
    opts: &PrStackOptions,
    actor: ActorInfo,
) -> Result<PrStackOutcome> {
    struct DraftedStackPr {
        pr: GitHubPullRequest,
        base_branch: String,
        head_branch: String,
        user_body: String,
        created: bool,
        change_ids: Vec<String>,
    }

    let remote_name = opts
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("origin");
    let root_base = opts
        .base
        .as_deref()
        .map(normalize_short_branch)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string());

    let (client, parsed_remote) = GitHubClient::for_repo(
        repo,
        Some(remote_name),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let _ = client.require_authenticated()?;
    let (owner, gh_repo) = parsed_remote.require_owner_repo()?;

    let stack_chain = discover_ordered_stack(repo, &root_base, &opts.branches)?;
    if stack_chain.is_empty() {
        return Err(OzError::InvalidUsage {
            message: format!("no stacked branches found above '{root_base}'"),
            recovery: vec![
                format!("Create a stacked branch on top of '{root_base}' first."),
                "oz stack status --json".to_string(),
            ],
        });
    }

    let gix_repo = open_gix_unrestricted(&repo.repo_root)?;

    // 1. Push all stack branches atomically in one `push_refs` transaction!
    let mut pushed = false;
    let mut atomic_push_op_id = None;
    if !opts.no_push {
        let mut any_needs_lease = false;
        for node in &stack_chain {
            if branch_needs_lease_push(&gix_repo, remote_name, &node.branch)? {
                any_needs_lease = true;
                break;
            }
        }
        let push_out = push_refs(
            repo,
            &PushOptions {
                remote: Some(remote_name.to_string()),
                refspecs: stack_chain.iter().map(|n| n.branch.clone()).collect(),
                atomic: true,
                force: false,
                force_with_lease: any_needs_lease,
                lease_specs: Vec::new(),
                delete: false,
                set_upstream: true,
                auth_token: opts.auth_token.clone(),
                auth_scheme: opts.git_auth_scheme,
            },
            actor,
        )?;
        pushed = true;
        atomic_push_op_id = push_out.op_id;
    }

    // 2. Pass 1: Create or update each PR with its exact stacked `base_branch` so every PR has `#number`.
    let open_prs = client.list_pull_requests(owner, gh_repo, Some("open"), None, None)?;
    let mut open_by_head: HashMap<String, GitHubPullRequest> = HashMap::new();
    for pr in open_prs {
        open_by_head.entry(pr.head.ref_name.clone()).or_insert(pr);
    }

    let mut drafted = Vec::with_capacity(stack_chain.len());
    for node in &stack_chain {
        let expected_base = node
            .parent_branch
            .clone()
            .unwrap_or_else(|| root_base.clone());
        let (inferred_title, inferred_body, change_ids) =
            infer_title_body_and_change_ids(&gix_repo, &expected_base, &node.branch)?;

        if let Some(existing) = open_by_head.remove(&node.branch) {
            let existing_clean =
                strip_stack_navigation_block(existing.body.as_deref().unwrap_or_default());
            let base_prose = if existing_clean.trim().is_empty() {
                inferred_body
            } else {
                existing_clean
            };
            let user_body = attach_change_ids_to_body(&base_prose, &change_ids);
            let pr = if existing.base.ref_name == expected_base {
                existing
            } else {
                client.update_pull_request(
                    owner,
                    gh_repo,
                    existing.number,
                    &UpdatePullRequest {
                        base: Some(expected_base.clone()),
                        ..Default::default()
                    },
                )?
            };

            drafted.push(DraftedStackPr {
                pr,
                base_branch: expected_base,
                head_branch: node.branch.clone(),
                user_body,
                created: false,
                change_ids,
            });
        } else {
            let user_body = attach_change_ids_to_body(&inferred_body, &change_ids);
            let created_pr = client.create_pull_request(
                owner,
                gh_repo,
                &PullRequestSpec {
                    title: inferred_title,
                    body: user_body.clone(),
                    head_branch: node.branch.clone(),
                    base_branch: expected_base.clone(),
                    draft: opts.draft,
                },
            )?;
            drafted.push(DraftedStackPr {
                pr: created_pr,
                base_branch: expected_base,
                head_branch: node.branch.clone(),
                user_body,
                created: true,
                change_ids,
            });
        }
    }

    // 3. Pass 2: Inject deterministic `<!-- oz-stack-start --> ... <!-- oz-stack-end -->` cross-links into all PR bodies!
    let nav_items: Vec<StackPrItem> = drafted
        .iter()
        .map(|d| StackPrItem {
            number: d.pr.number,
            html_url: d.pr.html_url.clone(),
            title: d.pr.title.clone(),
            head_branch: d.head_branch.clone(),
            base_branch: d.base_branch.clone(),
            status: format_pr_status(&d.pr),
            created: d.created,
            change_ids: d.change_ids.clone(),
        })
        .collect();

    let mut final_items = Vec::with_capacity(drafted.len());
    for mut item_draft in drafted {
        let nav_block = render_stack_navigation_block(&root_base, &nav_items, item_draft.pr.number);
        let full_body = upsert_stack_navigation_block(&item_draft.user_body, &nav_block);
        if item_draft.pr.body.as_deref() != Some(full_body.as_str()) {
            item_draft.pr = client.update_pull_request(
                owner,
                gh_repo,
                item_draft.pr.number,
                &UpdatePullRequest {
                    body: Some(full_body),
                    ..Default::default()
                },
            )?;
        }
        final_items.push(StackPrItem {
            number: item_draft.pr.number,
            html_url: item_draft.pr.html_url.clone(),
            title: item_draft.pr.title.clone(),
            head_branch: item_draft.head_branch,
            base_branch: item_draft.base_branch,
            status: format_pr_status(&item_draft.pr),
            created: item_draft.created,
            change_ids: item_draft.change_ids,
        });
    }

    Ok(PrStackOutcome {
        root_base,
        pushed,
        atomic_push_op_id,
        items: final_items,
    })
}

/// Renders the deterministic `<!-- oz-stack-start --> ... <!-- oz-stack-end -->` markdown block
/// cross-linking every PR in the stack and marking `current_pr_number` with `👉`.
#[must_use]
pub(crate) fn render_stack_navigation_block(
    root_base: &str,
    items: &[StackPrItem],
    current_pr_number: u64,
) -> String {
    let mut out = String::new();
    out.push_str(OZ_STACK_START);
    out.push('\n');
    let _ = write!(out, "### Stacked PRs (base: `{root_base}`)\n\n");
    out.push_str("| Current | PR | Branch | Base | Status |\n");
    out.push_str("| :---: | :--- | :--- | :--- | :--- |\n");
    for item in items {
        let marker = if item.number == current_pr_number {
            "👉"
        } else {
            ""
        };
        let pr_link = if item.html_url.is_empty() {
            format!("#{}", item.number)
        } else {
            format!("[#{}]({})", item.number, item.html_url)
        };
        let _ = writeln!(
            out,
            "| {} | {} | `{}` | `{}` | {} |",
            marker, pr_link, item.head_branch, item.base_branch, item.status
        );
    }
    out.push_str(OZ_STACK_END);
    out
}

/// Inserts or replaces the `<!-- oz-stack-start --> ... <!-- oz-stack-end -->` section in `body` idempotently.
#[must_use]
pub(crate) fn upsert_stack_navigation_block(body: &str, block: &str) -> String {
    if let (Some(start_idx), Some(end_rel)) = (body.find(OZ_STACK_START), body.find(OZ_STACK_END))
        && end_rel >= start_idx
    {
        let after_end = end_rel + OZ_STACK_END.len();
        let prefix = body[..start_idx].trim_end();
        let suffix = body[after_end..].trim_start();
        let mut combined = String::new();
        if !prefix.is_empty() {
            combined.push_str(prefix);
            combined.push_str("\n\n");
        }
        combined.push_str(block);
        if !suffix.is_empty() {
            combined.push_str("\n\n");
            combined.push_str(suffix);
        }
        return combined;
    }

    let trimmed = body.trim();
    if trimmed.is_empty() {
        block.to_string()
    } else {
        format!("{trimmed}\n\n{block}")
    }
}

/// Removes any managed `<!-- oz-stack-start --> ... <!-- oz-stack-end -->` block from `body`.
#[must_use]
pub(crate) fn strip_stack_navigation_block(body: &str) -> String {
    if let (Some(start_idx), Some(end_rel)) = (body.find(OZ_STACK_START), body.find(OZ_STACK_END))
        && end_rel >= start_idx
    {
        let after_end = end_rel + OZ_STACK_END.len();
        let prefix = body[..start_idx].trim_end();
        let suffix = body[after_end..].trim_start();
        return match (prefix.is_empty(), suffix.is_empty()) {
            (true, true) => String::new(),
            (false, true) => prefix.to_string(),
            (true, false) => suffix.to_string(),
            (false, false) => format!("{prefix}\n\n{suffix}"),
        };
    }
    body.trim().to_string()
}

fn extract_existing_stack_block(body: &str) -> Option<String> {
    let start_idx = body.find(OZ_STACK_START)?;
    let end_idx = body.find(OZ_STACK_END)?;
    if end_idx < start_idx {
        return None;
    }
    Some(body[start_idx..end_idx + OZ_STACK_END.len()].to_string())
}

pub(crate) fn format_pr_status(pr: &GitHubPullRequest) -> String {
    if pr.is_merged() {
        "Merged".to_string()
    } else if pr.state.eq_ignore_ascii_case("closed") {
        "Closed".to_string()
    } else if pr.draft {
        "Draft".to_string()
    } else {
        "Open".to_string()
    }
}

#[derive(Debug, Clone)]
struct OrderedStackNode {
    branch: String,
    parent_branch: Option<String>,
}

fn discover_ordered_stack(
    repo: &OzoneRepo,
    root_base: &str,
    explicit_branches: &[String],
) -> Result<Vec<OrderedStackNode>> {
    let report = stack_status(repo, Some(root_base))?;
    let by_name: HashMap<String, &StackBranchStatus> = report
        .branches
        .iter()
        .map(|b| (b.branch.clone(), b))
        .collect();

    if !explicit_branches.is_empty() {
        let requested_set: HashSet<String> = explicit_branches
            .iter()
            .map(|b| normalize_short_branch(b))
            .filter(|b| !b.is_empty() && b != root_base)
            .collect();

        // Keep topological order from `stack_status` for known unmerged branches, and verify all exist.
        let gix_repo = open_gix_unrestricted(&repo.repo_root)?;
        for req in &requested_set {
            let full = format!("refs/heads/{req}");
            if read_ref_oid(&gix_repo, &full)?.is_none() {
                return Err(OzError::InvalidUsage {
                    message: format!("stack branch '{req}' does not exist locally"),
                    recovery: vec!["oz stack status --json".to_string()],
                });
            }
        }

        let mut ordered = Vec::new();
        for status_item in &report.branches {
            if requested_set.contains(&status_item.branch) {
                // Walk nearest ancestor present in `requested_set` (or `None` -> `root_base`).
                let mut cur_parent = status_item.parent_branch.clone();
                while let Some(ref p) = cur_parent {
                    if requested_set.contains(p) {
                        break;
                    }
                    cur_parent = by_name.get(p).and_then(|info| info.parent_branch.clone());
                }
                ordered.push(OrderedStackNode {
                    branch: status_item.branch.clone(),
                    parent_branch: cur_parent,
                });
            }
        }
        return Ok(ordered);
    }

    if report.branches.is_empty() {
        return Ok(Vec::new());
    }

    // If currently on a stacked branch `active` != `root_base`, collect its ancestor chain up to `root_base`
    // AND any linear/topological descendants stacked on top of `active`!
    if let Some(ref active) = report.active_branch
        && active != root_base
        && by_name.contains_key(active)
    {
        let mut selected: HashSet<String> = HashSet::new();
        let mut cur = Some(active.clone());
        while let Some(name) = cur {
            selected.insert(name.clone());
            cur = by_name.get(&name).and_then(|b| b.parent_branch.clone());
        }
        // Include descendants in topological order whose `parent_branch` is already in `selected`.
        for item in &report.branches {
            if let Some(ref parent) = item.parent_branch
                && selected.contains(parent)
            {
                selected.insert(item.branch.clone());
            }
        }

        return Ok(report
            .branches
            .iter()
            .filter(|b| selected.contains(&b.branch))
            .map(|b| OrderedStackNode {
                branch: b.branch.clone(),
                parent_branch: b.parent_branch.clone().filter(|p| selected.contains(p)),
            })
            .collect());
    }

    // When invoked from `main` (or another base), include all stacked branches discovered above `root_base`.
    Ok(report
        .branches
        .iter()
        .map(|b| OrderedStackNode {
            branch: b.branch.clone(),
            parent_branch: b.parent_branch.clone(),
        })
        .collect())
}

fn find_open_pr_for_head(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    head_branch: &str,
) -> Result<Option<GitHubPullRequest>> {
    let head_filter = format!("{owner}:{head_branch}");
    let candidates =
        client.list_pull_requests(owner, repo, Some("open"), Some(&head_filter), None)?;
    Ok(candidates
        .into_iter()
        .find(|pr| pr.head.ref_name == head_branch))
}

fn branch_needs_lease_push(
    gix_repo: &gix::Repository,
    remote_name: &str,
    branch: &str,
) -> Result<bool> {
    let local_ref = format!("refs/heads/{branch}");
    let tracking_ref = format!("refs/remotes/{remote_name}/{branch}");
    let Some(local_oid) = read_ref_oid(gix_repo, &local_ref)? else {
        return Ok(false);
    };
    let Some(tracking_oid) = read_ref_oid(gix_repo, &tracking_ref)? else {
        return Ok(false);
    };
    if local_oid == tracking_oid {
        return Ok(false);
    }
    Ok(!is_ancestor_commit(gix_repo, tracking_oid, local_oid))
}

/// Inspects commits in `(base_branch..head_branch]` (oldest-first) to infer PR title, body, and `Change-Id`s.
pub(crate) fn infer_title_body_and_change_ids(
    gix_repo: &gix::Repository,
    base_branch: &str,
    head_branch: &str,
) -> Result<(String, String, Vec<String>)> {
    let head_ref = format!("refs/heads/{head_branch}");
    let head_oid = read_ref_oid(gix_repo, &head_ref)?.ok_or_else(|| OzError::InvalidUsage {
        message: format!("local branch '{head_branch}' does not exist"),
        recovery: vec!["oz status --json".to_string()],
    })?;

    let base_oid = read_ref_oid(gix_repo, &format!("refs/heads/{base_branch}"))?.or_else(|| {
        read_ref_oid(gix_repo, &format!("refs/remotes/origin/{base_branch}"))
            .ok()
            .flatten()
    });

    let stop_oid = match base_oid {
        Some(b_id) => gix_repo
            .merge_base(b_id, head_oid)
            .ok()
            .flatten()
            .map(gix::Id::detach),
        None => None,
    };

    let commits_oldest_first = collect_first_parent_commits(gix_repo, head_oid, stop_oid)?;
    let mut subjects = Vec::new();
    let mut bodies = Vec::new();
    let mut change_ids = Vec::new();

    for cid in &commits_oldest_first {
        let commit = gix_repo.find_commit(*cid).map_err(OzError::git_source)?;
        let raw_msg = commit.message_raw_sloppy().to_str_lossy().into_owned();
        let (subj, body_without_trailers, cids) = split_commit_message(&raw_msg);
        if !subj.is_empty() {
            subjects.push(subj);
        }
        if !body_without_trailers.is_empty() {
            bodies.push(body_without_trailers);
        }
        for id in cids {
            if !change_ids.contains(&id) {
                change_ids.push(id);
            }
        }
    }

    let title = if subjects.len() == 1 {
        subjects[0].clone()
    } else if let Some(last) = subjects.last() {
        if subjects.len() <= 1 {
            last.clone()
        } else {
            subjects[0].clone()
        }
    } else {
        humanize_branch_title(head_branch)
    };

    let body = if commits_oldest_first.len() <= 1 {
        bodies.into_iter().next().unwrap_or_default()
    } else {
        let mut summary = String::new();
        if let Some(first_body) = bodies.first()
            && !first_body.is_empty()
        {
            summary.push_str(first_body);
            summary.push_str("\n\n");
        }
        summary.push_str("### Commits\n");
        for s in &subjects {
            let _ = writeln!(summary, "- {s}");
        }
        summary.trim_end().to_string()
    };

    Ok((title, body, change_ids))
}

pub(crate) fn extract_change_ids_from_text(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("Change-Id:") {
            let cid = val.trim().trim_matches('`').to_string();
            if !cid.is_empty() && !ids.contains(&cid) {
                ids.push(cid);
            }
        }
    }
    ids
}

fn split_commit_message(raw: &str) -> (String, String, Vec<String>) {
    let mut lines = raw.lines();
    let subject = lines.next().unwrap_or_default().trim().to_string();
    let mut body_lines = Vec::new();
    let mut change_ids = Vec::new();

    for line in lines {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("Change-Id:") {
            let cid = val.trim().to_string();
            if !cid.is_empty() && !change_ids.contains(&cid) {
                change_ids.push(cid);
            }
            continue;
        }
        body_lines.push(line);
    }

    let body = body_lines.join("\n").trim().to_string();
    (subject, body, change_ids)
}

fn attach_change_ids_to_body(body: &str, change_ids: &[String]) -> String {
    if change_ids.is_empty() {
        return body.trim().to_string();
    }
    let existing = extract_change_ids_from_text(body);
    let missing: Vec<&String> = change_ids
        .iter()
        .filter(|cid| !existing.contains(cid))
        .collect();
    if missing.is_empty() {
        return body.trim().to_string();
    }

    let mut out = body.trim().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    for (idx, cid) in missing.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let _ = write!(out, "Change-Id: {cid}");
    }
    out
}

fn collect_first_parent_commits(
    gix_repo: &gix::Repository,
    tip: ObjectId,
    stop_exclusive: Option<ObjectId>,
) -> Result<Vec<ObjectId>> {
    let mut out = Vec::new();
    let mut cur = Some(tip);
    let mut guard = 0usize;
    while let Some(cid) = cur {
        if Some(cid) == stop_exclusive || guard >= 512 {
            break;
        }
        guard += 1;
        out.push(cid);
        let commit = gix_repo.find_commit(cid).map_err(OzError::git_source)?;
        cur = commit.parent_ids().next().map(gix::Id::detach);
    }
    out.reverse();
    Ok(out)
}

fn humanize_branch_title(branch: &str) -> String {
    let stripped = branch
        .rsplit('/')
        .next()
        .unwrap_or(branch)
        .replace(['-', '_'], " ");
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        branch.to_string()
    } else {
        trimmed.to_string()
    }
}

pub(crate) fn normalize_short_branch(raw: &str) -> String {
    let t = raw.trim();
    t.strip_prefix("refs/heads/").unwrap_or(t).to_string()
}

// ============================================================================
// PR View, Line-Numbered Diff, Head File Read & Validated Inline Review
// ============================================================================

/// Options controlling [`pr_view`].
#[derive(Debug, Clone, Default)]
pub struct PrViewOptions {
    pub number: u64,
    /// When `true`, also fetches inline review threads and PR conversation comments (`--comments`).
    pub include_comments: bool,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

/// Compact file summary inside [`PrViewOutcome`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrViewFileSummary {
    pub id: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
}

/// Compact review summary inside [`PrViewOutcome`] and [`PrCommentsOutcome`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrViewReviewSummary {
    pub id: u64,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// Single comment inside an inline PR review thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrReviewThreadComment {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
}

/// Inline review comment thread grouped by `path:line` with resolved/outdated status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrReviewThread {
    /// Short deterministic handle (`t1`, `t2`, ...) usable with `oz pr reply` and `oz pr resolve`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub root_comment_id: u64,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    pub side: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_resolved: Option<bool>,
    pub is_outdated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_hunk: Option<String>,
    pub comments: Vec<PrReviewThreadComment>,
}

/// Top-level conversation comment on a pull request or issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrConversationComment {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
}

/// Options controlling [`pr_comments`] (`oz pr comments <number>`).
#[derive(Debug, Clone, Default)]
pub struct PrCommentsOptions {
    pub number: u64,
    /// When `true`, filters inline threads to exclude resolved threads (`--unresolved`).
    pub unresolved_only: bool,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

/// Structured PR reviews, inline review threads (grouped by `path:line`), and conversation comments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrCommentsOutcome {
    pub owner: String,
    pub repo: String,
    pub pr_number: u64,
    pub title: String,
    pub state: String,
    pub head_sha: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub html_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<String>,
    pub total_reviews: usize,
    pub total_threads: usize,
    pub unresolved_threads: usize,
    pub outdated_threads: usize,
    pub total_issue_comments: usize,
    pub reviews: Vec<PrViewReviewSummary>,
    pub threads: Vec<PrReviewThread>,
    pub issue_comments: Vec<PrConversationComment>,
}

/// Result returned by [`pr_view`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrViewOutcome {
    pub owner: String,
    pub repo: String,
    pub pull_request: GitHubPullRequest,
    pub files: Vec<PrViewFileSummary>,
    pub reviews: Vec<PrViewReviewSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<PrReviewThread>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issue_comments: Vec<PrConversationComment>,
    pub total_additions: u32,
    pub total_deletions: u32,
}

/// Options controlling [`pr_diff`].
#[derive(Debug, Clone, Default)]
pub struct PrDiffOptions {
    pub number: u64,
    pub targets: Vec<String>,
    pub excludes: Vec<String>,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

/// Result returned by [`pr_diff`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrDiffOutcome {
    pub owner: String,
    pub repo: String,
    pub pr_number: u64,
    pub title: String,
    pub head_branch: String,
    pub base_branch: String,
    pub head_sha: String,
    pub base_sha: String,
    pub report: DiffReport,
}

/// Options controlling [`pr_read`].
#[derive(Debug, Clone, Default)]
pub struct PrReadOptions {
    pub number: u64,
    pub path: String,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

/// Result returned by [`pr_read`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrReadOutcome {
    pub owner: String,
    pub repo: String,
    pub pr_number: u64,
    pub head_branch: String,
    pub source: String,
    pub blob: ShowBlobReport,
}

/// Review decision event for [`pr_review`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PrReviewEvent {
    Approve,
    RequestChanges,
    Comment,
}

impl PrReviewEvent {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "APPROVE",
            Self::RequestChanges => "REQUEST_CHANGES",
            Self::Comment => "COMMENT",
        }
    }
}

/// Diff side for an inline PR review comment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PrCommentSide {
    Left,
    #[default]
    Right,
}

impl PrCommentSide {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Left => "LEFT",
            Self::Right => "RIGHT",
        }
    }
}

/// Single inline review comment input for [`pr_review`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrInlineCommentInput {
    pub path: String,
    pub line: u32,
    #[serde(default)]
    pub side: PrCommentSide,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_side: Option<PrCommentSide>,
    pub body: String,
}

/// Options controlling [`pr_review`].
#[derive(Debug, Clone)]
pub struct PrReviewOptions {
    pub number: u64,
    pub event: PrReviewEvent,
    pub body: Option<String>,
    pub comments: Vec<PrInlineCommentInput>,
    pub dry_run: bool,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

/// Result returned by [`pr_review`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrReviewOutcome {
    pub owner: String,
    pub repo: String,
    pub pr_number: u64,
    pub commit_id: String,
    pub event: String,
    pub dry_run: bool,
    pub validated_comments: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<GitHubPullRequestReview>,
}

/// Parses an inline CLI comment spec:
/// `path/to/file.ts:401=Comment body`
/// `path/to/file.ts:401:LEFT=Comment on deleted line`
/// `path/to/file.ts:398-401:RIGHT=Multi-line comment`
pub fn parse_inline_comment_spec(raw: &str) -> Result<PrInlineCommentInput> {
    let Some((loc_part, body_part)) = raw.split_once('=') else {
        return Err(OzError::InvalidUsage {
            message: format!(
                "invalid inline comment '{raw}': expected `<path>:<line>[:LEFT|RIGHT]=<body>`"
            ),
            recovery: vec![
                "Example: `--inline src/index.ts:401=Handle empty payload`.".to_string(),
            ],
        });
    };
    let body = body_part.trim().to_string();
    if body.is_empty() {
        return Err(OzError::InvalidUsage {
            message: format!("empty inline comment body in '{raw}'"),
            recovery: vec!["Provide a non-empty comment after `=`.".to_string()],
        });
    }

    let parts: Vec<&str> = loc_part.split(':').collect();
    if parts.len() < 2 {
        return Err(OzError::InvalidUsage {
            message: format!(
                "invalid inline comment location '{loc_part}': expected `<path>:<line>[:LEFT|RIGHT]`"
            ),
            recovery: vec!["Example: `--inline src/index.ts:401=Check null`.".to_string()],
        });
    }

    let (path_segments, line_token, side) = if parts.len() >= 3 {
        let maybe_side = parts[parts.len() - 1].trim().to_ascii_uppercase();
        if maybe_side == "LEFT" || maybe_side == "RIGHT" {
            let side = if maybe_side == "LEFT" {
                PrCommentSide::Left
            } else {
                PrCommentSide::Right
            };
            (&parts[..parts.len() - 2], parts[parts.len() - 2], side)
        } else {
            (
                &parts[..parts.len() - 1],
                parts[parts.len() - 1],
                PrCommentSide::Right,
            )
        }
    } else {
        (&parts[..1], parts[1], PrCommentSide::Right)
    };

    let path = path_segments
        .join(":")
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();
    if path.is_empty() {
        return Err(OzError::InvalidUsage {
            message: format!("empty file path in inline comment '{raw}'"),
            recovery: Vec::new(),
        });
    }

    let (start_line, line) = if let Some((a, b)) = line_token.split_once('-') {
        let s: u32 = a.trim().parse().map_err(|_| OzError::InvalidUsage {
            message: format!("invalid start line '{a}' in '{raw}'"),
            recovery: Vec::new(),
        })?;
        let e: u32 = b.trim().parse().map_err(|_| OzError::InvalidUsage {
            message: format!("invalid end line '{b}' in '{raw}'"),
            recovery: Vec::new(),
        })?;
        if s == 0 || e == 0 || s > e {
            return Err(OzError::InvalidUsage {
                message: format!("invalid line range '{line_token}' in '{raw}'"),
                recovery: Vec::new(),
            });
        }
        (if s == e { None } else { Some(s) }, e)
    } else {
        let l: u32 = line_token
            .trim()
            .parse()
            .map_err(|_| OzError::InvalidUsage {
                message: format!("invalid line number '{line_token}' in '{raw}'"),
                recovery: Vec::new(),
            })?;
        if l == 0 {
            return Err(OzError::InvalidUsage {
                message: format!("line numbers must be >= 1 in '{raw}'"),
                recovery: Vec::new(),
            });
        }
        (None, l)
    };

    Ok(PrInlineCommentInput {
        path,
        line,
        side,
        start_line,
        start_side: start_line.map(|_| side),
        body,
    })
}

fn parse_hunk_header_bounds(header: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = header.strip_prefix("@@ ")?;
    let (ranges_part, _) = rest.split_once(" @@")?;
    let mut parts = ranges_part.split_whitespace();
    let old_tok = parts.next()?.strip_prefix('-')?;
    let new_tok = parts.next()?.strip_prefix('+')?;

    let parse_pair = |tok: &str| -> Option<(u32, u32)> {
        if let Some((a, b)) = tok.split_once(',') {
            Some((a.parse().ok()?, b.parse().ok()?))
        } else {
            Some((tok.parse().ok()?, 1))
        }
    };
    let (old_start, old_lines) = parse_pair(old_tok)?;
    let (new_start, new_lines) = parse_pair(new_tok)?;
    Some((old_start, old_lines, new_start, new_lines))
}

/// Parses a GitHub unified `patch` string into structured [`DiffHunk`]s with deterministic
/// hunk IDs (`h1a`, `h1b`, ...) and exact 1-based `old_line` / `new_line` numbers.
#[must_use]
pub(crate) fn parse_github_patch_to_hunks(
    file_id_str: &str,
    file_num: usize,
    file_path: &str,
    patch_text: &str,
) -> Vec<DiffHunk> {
    struct PendingHunk {
        old_start: u32,
        old_lines: u32,
        new_start: u32,
        new_lines: u32,
        header: String,
        old_cur: u32,
        new_cur: u32,
        lines: Vec<DiffLine>,
    }

    let mut hunks = Vec::new();
    let mut current: Option<PendingHunk> = None;

    for raw_line in patch_text.split('\n') {
        if raw_line.starts_with("@@ ")
            && let Some((old_start, old_lines, new_start, new_lines)) =
                parse_hunk_header_bounds(raw_line)
        {
            if let Some(p) = current.take() {
                let idx = hunks.len();
                hunks.push(DiffHunk {
                    id: hunk_id(file_num, idx),
                    file_id: file_id_str.to_string(),
                    path: file_path.to_string(),
                    old_start: p.old_start,
                    old_lines: p.old_lines,
                    new_start: p.new_start,
                    new_lines: p.new_lines,
                    header: p.header,
                    lines: p.lines,
                });
            }
            current = Some(PendingHunk {
                old_start,
                old_lines,
                new_start,
                new_lines,
                header: raw_line.to_string(),
                old_cur: old_start,
                new_cur: new_start,
                lines: Vec::new(),
            });
            continue;
        }

        let Some(ref mut cur) = current else {
            continue;
        };

        if raw_line.starts_with("\\ No newline at end of file") {
            continue;
        }
        if let Some(body) = raw_line.strip_prefix('+') {
            cur.lines.push(DiffLine {
                kind: DiffLineKind::Add,
                old_line: None,
                new_line: Some(cur.new_cur),
                content: format!("{body}\n"),
            });
            cur.new_cur += 1;
        } else if let Some(body) = raw_line.strip_prefix('-') {
            cur.lines.push(DiffLine {
                kind: DiffLineKind::Delete,
                old_line: Some(cur.old_cur),
                new_line: None,
                content: format!("{body}\n"),
            });
            cur.old_cur += 1;
        } else if let Some(body) = raw_line.strip_prefix(' ') {
            cur.lines.push(DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(cur.old_cur),
                new_line: Some(cur.new_cur),
                content: format!("{body}\n"),
            });
            cur.old_cur += 1;
            cur.new_cur += 1;
        }
    }

    if let Some(p) = current.take() {
        let idx = hunks.len();
        hunks.push(DiffHunk {
            id: hunk_id(file_num, idx),
            file_id: file_id_str.to_string(),
            path: file_path.to_string(),
            old_start: p.old_start,
            old_lines: p.old_lines,
            new_start: p.new_start,
            new_lines: p.new_lines,
            header: p.header,
            lines: p.lines,
        });
    }

    hunks
}

/// Converts GitHub PR file entries into Ozone [`FileChange`] structs with deterministic IDs
/// and line-numbered [`DiffHunk`]s.
#[must_use]
pub(crate) fn pr_files_to_changes(files: &[GitHubPullRequestFile]) -> Vec<FileChange> {
    files
        .iter()
        .enumerate()
        .map(|(idx, f)| {
            let num = idx + 1;
            let fid = file_id(num);
            let kind = match f.status.as_str() {
                "added" => ChangeKind::Added,
                "removed" => ChangeKind::Deleted,
                "renamed" => ChangeKind::Renamed,
                "copied" => ChangeKind::Copied,
                _ => ChangeKind::Modified,
            };
            let hunks = f
                .patch
                .as_deref()
                .map(|p| parse_github_patch_to_hunks(&fid, num, &f.filename, p))
                .unwrap_or_default();
            let is_binary = f.patch.is_none() && f.additions == 0 && f.deletions == 0;
            FileChange {
                id: fid,
                path: f.filename.clone(),
                old_path: f.previous_filename.clone(),
                kind,
                additions: f.additions,
                deletions: f.deletions,
                is_binary,
                pinned: false,
                out_of_scope: false,
                hunks,
            }
        })
        .collect()
}

fn format_contiguous_ranges(lines: &BTreeSet<u32>) -> String {
    let mut iter = lines.iter().copied();
    let Some(mut start) = iter.next() else {
        return "<none>".to_string();
    };
    let mut ranges = Vec::new();
    let mut prev = start;
    for n in iter {
        if n == prev + 1 {
            prev = n;
        } else {
            if start == prev {
                ranges.push(format!("{start}"));
            } else {
                ranges.push(format!("{start}..={prev}"));
            }
            start = n;
            prev = n;
        }
    }
    if start == prev {
        ranges.push(format!("{start}"));
    } else {
        ranges.push(format!("{start}..={prev}"));
    }
    ranges.join(", ")
}

/// Pre-validates inline review comments against parsed PR diff hunks and converts them
/// to GitHub REST payloads. Fails locally with valid line-range hints on any mismatch.
pub(crate) fn validate_pr_review_comments(
    pr_number: u64,
    pr_changes: &[FileChange],
    comments: &[PrInlineCommentInput],
) -> Result<Vec<GitHubReviewCommentPayload>> {
    let mut by_path: HashMap<&str, &FileChange> = HashMap::new();
    for f in pr_changes {
        by_path.insert(f.path.as_str(), f);
    }

    let mut payloads = Vec::with_capacity(comments.len());
    for c in comments {
        let norm_path = c
            .path
            .replace('\\', "/")
            .trim_start_matches("./")
            .trim_start_matches('/')
            .to_string();
        let Some(file) = by_path.get(norm_path.as_str()) else {
            let sample_paths: Vec<String> =
                pr_changes.iter().take(10).map(|f| f.path.clone()).collect();
            return Err(OzError::InvalidUsage {
                message: format!(
                    "inline comment path '{norm_path}' is not in PR #{pr_number} changed files"
                ),
                recovery: vec![format!(
                    "Changed files in PR #{pr_number}: {}",
                    sample_paths.join(", ")
                )],
            });
        };

        let mut valid_right = BTreeSet::new();
        let mut valid_left = BTreeSet::new();
        for hunk in &file.hunks {
            for dl in &hunk.lines {
                match dl.kind {
                    DiffLineKind::Context => {
                        if let Some(l) = dl.old_line {
                            valid_left.insert(l);
                        }
                        if let Some(r) = dl.new_line {
                            valid_right.insert(r);
                        }
                    }
                    DiffLineKind::Add => {
                        if let Some(r) = dl.new_line {
                            valid_right.insert(r);
                        }
                    }
                    DiffLineKind::Delete => {
                        if let Some(l) = dl.old_line {
                            valid_left.insert(l);
                        }
                    }
                }
            }
        }

        let valid_for_side = match c.side {
            PrCommentSide::Right => &valid_right,
            PrCommentSide::Left => &valid_left,
        };
        if !valid_for_side.contains(&c.line) {
            return Err(OzError::InvalidUsage {
                message: format!(
                    "inline comment line {} on {} of '{norm_path}' is outside PR #{pr_number} diff hunks (valid {} lines: {})",
                    c.line,
                    c.side.as_str(),
                    c.side.as_str(),
                    format_contiguous_ranges(valid_for_side)
                ),
                recovery: vec![format!(
                    "Run `oz pr diff {pr_number} --line-numbers {norm_path}` to see exact commentable line numbers."
                )],
            });
        }

        if let Some(sl) = c.start_line {
            let start_side = c.start_side.unwrap_or(c.side);
            let valid_for_start = match start_side {
                PrCommentSide::Right => &valid_right,
                PrCommentSide::Left => &valid_left,
            };
            if sl > c.line {
                return Err(OzError::InvalidUsage {
                    message: format!(
                        "inline comment start_line {sl} > line {} for '{norm_path}'",
                        c.line
                    ),
                    recovery: Vec::new(),
                });
            }
            if !valid_for_start.contains(&sl) {
                return Err(OzError::InvalidUsage {
                    message: format!(
                        "inline comment start_line {sl} on {} of '{norm_path}' is outside PR #{pr_number} diff hunks (valid {} lines: {})",
                        start_side.as_str(),
                        start_side.as_str(),
                        format_contiguous_ranges(valid_for_start)
                    ),
                    recovery: vec![format!(
                        "Run `oz pr diff {pr_number} --line-numbers {norm_path}` to inspect valid lines."
                    )],
                });
            }
        }

        payloads.push(GitHubReviewCommentPayload {
            path: norm_path,
            line: c.line,
            side: c.side.as_str().to_string(),
            start_line: c.start_line,
            start_side: c
                .start_line
                .map(|_| c.start_side.unwrap_or(c.side).as_str().to_string()),
            body: c.body.clone(),
        });
    }

    Ok(payloads)
}

fn resolve_github_pr_client(
    repo: &OzoneRepo,
    remote_opt: Option<&str>,
    auth_token: Option<&str>,
    api_url: Option<&str>,
) -> Result<(GitHubClient, String, String)> {
    let (client, remote) = GitHubClient::for_repo(repo, remote_opt, auth_token, api_url)?;
    let owner = remote.owner.ok_or_else(|| OzError::InvalidUsage {
        message: format!(
            "remote '{}' does not contain a GitHub owner/repo slug",
            remote.sanitized_url
        ),
        recovery: vec!["Verify `git remote -v` points to a GitHub repository.".to_string()],
    })?;
    let repo_name = remote.repo.ok_or_else(|| OzError::InvalidUsage {
        message: format!(
            "remote '{}' does not contain a GitHub repository name",
            remote.sanitized_url
        ),
        recovery: vec!["Verify `git remote -v` points to a GitHub repository.".to_string()],
    })?;
    Ok((client, owner, repo_name))
}

/// Fetches PR metadata, changed-file summary, review states, and optionally inline review threads
/// and conversation comments (`oz pr view <number> [--comments]`).
pub fn pr_view(repo: &OzoneRepo, opts: &PrViewOptions) -> Result<PrViewOutcome> {
    let (client, owner, repo_name) = resolve_github_pr_client(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let pull_request = client.get_pull_request(&owner, &repo_name, opts.number)?;
    let pr_files = client.list_pull_request_files(&owner, &repo_name, opts.number)?;
    let pr_reviews = client
        .list_pull_request_reviews(&owner, &repo_name, opts.number)
        .unwrap_or_default();

    let total_additions: u32 = pr_files.iter().map(|f| f.additions).sum();
    let total_deletions: u32 = pr_files.iter().map(|f| f.deletions).sum();

    let files = pr_files
        .into_iter()
        .enumerate()
        .map(|(idx, f)| PrViewFileSummary {
            id: file_id(idx + 1),
            path: f.filename,
            old_path: f.previous_filename,
            status: f.status,
            additions: f.additions,
            deletions: f.deletions,
        })
        .collect();

    let reviews = summarize_pr_reviews(pr_reviews);
    let review_decision = derive_review_decision(&reviews);

    let (threads, issue_comments) = if opts.include_comments {
        let raw_inline = client
            .list_pull_request_comments(&owner, &repo_name, opts.number)
            .unwrap_or_default();
        let gql_meta = client
            .list_pull_request_review_threads_graphql(&owner, &repo_name, opts.number)
            .unwrap_or_default();
        let raw_issue = client
            .list_issue_comments(&owner, &repo_name, opts.number)
            .unwrap_or_default();
        (
            group_review_threads(&raw_inline, &gql_meta),
            map_issue_comments(raw_issue),
        )
    } else {
        (Vec::new(), Vec::new())
    };

    Ok(PrViewOutcome {
        owner,
        repo: repo_name,
        pull_request,
        files,
        reviews,
        review_decision,
        threads,
        issue_comments,
        total_additions,
        total_deletions,
    })
}

/// Fetches structured PR reviews, inline review threads grouped by `path:line` with resolved/outdated
/// metadata, and top-level conversation comments (`oz pr comments <number>`).
pub fn pr_comments(repo: &OzoneRepo, opts: &PrCommentsOptions) -> Result<PrCommentsOutcome> {
    let (client, owner, repo_name) = resolve_github_pr_client(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let pr = client.get_pull_request(&owner, &repo_name, opts.number)?;
    let pr_reviews = client
        .list_pull_request_reviews(&owner, &repo_name, opts.number)
        .unwrap_or_default();
    let raw_inline = client
        .list_pull_request_comments(&owner, &repo_name, opts.number)
        .unwrap_or_default();
    let gql_meta = client
        .list_pull_request_review_threads_graphql(&owner, &repo_name, opts.number)
        .unwrap_or_default();
    let raw_issue = client
        .list_issue_comments(&owner, &repo_name, opts.number)
        .unwrap_or_default();

    let reviews = summarize_pr_reviews(pr_reviews);
    let review_decision = derive_review_decision(&reviews);
    let mut threads = group_review_threads(&raw_inline, &gql_meta);
    if opts.unresolved_only {
        threads.retain(|t| t.is_resolved != Some(true));
    }
    let issue_comments = map_issue_comments(raw_issue);

    let total_reviews = reviews.len();
    let total_threads = threads.len();
    let unresolved_threads = threads
        .iter()
        .filter(|t| t.is_resolved != Some(true))
        .count();
    let outdated_threads = threads.iter().filter(|t| t.is_outdated).count();
    let total_issue_comments = issue_comments.len();

    let state = format_pr_status(&pr);
    Ok(PrCommentsOutcome {
        owner,
        repo: repo_name,
        pr_number: pr.number,
        title: pr.title,
        state,
        head_sha: pr.head.sha,
        html_url: pr.html_url,
        review_decision,
        total_reviews,
        total_threads,
        unresolved_threads,
        outdated_threads,
        total_issue_comments,
        reviews,
        threads,
        issue_comments,
    })
}

fn summarize_pr_reviews(pr_reviews: Vec<GitHubPullRequestReview>) -> Vec<PrViewReviewSummary> {
    pr_reviews
        .into_iter()
        .map(|r| PrViewReviewSummary {
            id: r.id,
            state: r.state,
            author: r.user.map(|u| u.login),
            submitted_at: r.submitted_at,
            body: r.body.filter(|b| !b.trim().is_empty()),
        })
        .collect()
}

pub(crate) fn derive_review_decision(reviews: &[PrViewReviewSummary]) -> Option<String> {
    let mut latest_by_author: HashMap<&str, &str> = HashMap::new();
    for r in reviews {
        let state = r.state.as_str();
        if matches!(state, "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED") {
            let author = r.author.as_deref().unwrap_or("");
            latest_by_author.insert(author, state);
        }
    }
    if latest_by_author.values().any(|&s| s == "CHANGES_REQUESTED") {
        Some("CHANGES_REQUESTED".to_string())
    } else if latest_by_author.values().any(|&s| s == "APPROVED") {
        Some("APPROVED".to_string())
    } else {
        None
    }
}

pub(crate) fn map_issue_comments(raw: Vec<GitHubIssueComment>) -> Vec<PrConversationComment> {
    raw.into_iter()
        .map(|c| PrConversationComment {
            id: c.id,
            author: c.user.map(|u| u.login),
            body: c.body,
            created_at: c.created_at,
            html_url: c.html_url,
        })
        .collect()
}

/// Groups flat REST PR review comments into `path:line` threads and enriches with optional GraphQL
/// `isResolved`/`isOutdated` metadata.
#[must_use]
pub(crate) fn group_review_threads(
    raw_comments: &[GitHubPullRequestComment],
    graphql_threads: &[GitHubReviewThreadMeta],
) -> Vec<PrReviewThread> {
    let mut parent_map: HashMap<u64, u64> = HashMap::new();
    for c in raw_comments {
        if let Some(parent_id) = c.in_reply_to_id {
            parent_map.insert(c.id, parent_id);
        }
    }

    let resolve_root = |mut id: u64| -> u64 {
        let mut hops = 0usize;
        while let Some(&parent) = parent_map.get(&id) {
            if parent == id || hops >= 64 {
                break;
            }
            id = parent;
            hops += 1;
        }
        id
    };

    let mut meta_by_comment: HashMap<u64, &GitHubReviewThreadMeta> = HashMap::new();
    for meta in graphql_threads {
        for &cid in &meta.comment_ids {
            meta_by_comment.insert(cid, meta);
        }
    }

    let mut grouped: HashMap<u64, Vec<&GitHubPullRequestComment>> = HashMap::new();
    for c in raw_comments {
        let root_id = resolve_root(c.id);
        grouped.entry(root_id).or_default().push(c);
    }

    let mut threads: Vec<PrReviewThread> = Vec::with_capacity(grouped.len());
    for (root_id, mut items) in grouped {
        items.sort_by(|a, b| {
            a.created_at
                .as_deref()
                .cmp(&b.created_at.as_deref())
                .then_with(|| a.id.cmp(&b.id))
        });
        let root = items
            .iter()
            .copied()
            .find(|c| c.id == root_id)
            .unwrap_or(items[0]);

        let gql_meta = items
            .iter()
            .find_map(|c| meta_by_comment.get(&c.id).copied());
        let is_outdated = gql_meta.map_or_else(
            || root.line.is_none() && root.position.is_none(),
            |m| m.is_outdated,
        );

        let thread_comments = items
            .into_iter()
            .map(|c| PrReviewThreadComment {
                id: c.id,
                review_id: c.pull_request_review_id,
                author: c.user.as_ref().map(|u| u.login.clone()),
                body: c.body.clone(),
                created_at: c.created_at.clone(),
                html_url: c.html_url.clone(),
            })
            .collect();

        threads.push(PrReviewThread {
            id: String::new(),
            thread_id: gql_meta.map(|m| m.thread_id.clone()),
            root_comment_id: root.id,
            path: root.path.clone(),
            line: root.line,
            original_line: root.original_line,
            start_line: root.start_line.or(root.original_start_line),
            side: root.side.clone().unwrap_or_else(|| "RIGHT".to_string()),
            is_resolved: gql_meta.map(|m| m.is_resolved),
            is_outdated,
            diff_hunk: root.diff_hunk.clone(),
            comments: thread_comments,
        });
    }

    threads.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| {
                a.line
                    .or(a.original_line)
                    .unwrap_or(u32::MAX)
                    .cmp(&b.line.or(b.original_line).unwrap_or(u32::MAX))
            })
            .then_with(|| a.root_comment_id.cmp(&b.root_comment_id))
    });
    for (idx, t) in threads.iter_mut().enumerate() {
        t.id = format!("t{}", idx + 1);
    }
    threads
}

/// Fetches a PR's diff with deterministic file/hunk IDs and exact `old_line`/`new_line`
/// numbers (`oz pr diff <number>`).
pub fn pr_diff(repo: &OzoneRepo, opts: &PrDiffOptions) -> Result<PrDiffOutcome> {
    let (client, owner, repo_name) = resolve_github_pr_client(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let pr = client.get_pull_request(&owner, &repo_name, opts.number)?;
    let pr_files = client.list_pull_request_files(&owner, &repo_name, opts.number)?;
    let all_changes = pr_files_to_changes(&pr_files);
    let files = if opts.targets.is_empty() && opts.excludes.is_empty() {
        all_changes
    } else {
        filter_changes_by_targets_and_excludes(&all_changes, &opts.targets, &opts.excludes)?
    };
    let total_additions = files.iter().map(|f| f.additions).sum();
    let total_deletions = files.iter().map(|f| f.deletions).sum();

    let report = DiffReport {
        workspace: repo.workspace.clone(),
        branch: Some(pr.head.ref_name.clone()),
        head_oid: Some(pr.head.sha.clone()),
        base_oid: Some(pr.base.sha.clone()),
        target_oid: Some(pr.head.sha.clone()),
        files,
        total_additions,
        total_deletions,
    };

    Ok(PrDiffOutcome {
        owner,
        repo: repo_name,
        pr_number: pr.number,
        title: pr.title,
        head_branch: pr.head.ref_name,
        base_branch: pr.base.ref_name,
        head_sha: pr.head.sha,
        base_sha: pr.base.sha,
        report,
    })
}

/// Reads full file contents (or a 1-based line slice) at the exact PR head commit without
/// mutating the local working tree (`oz pr read <number> <path>`).
pub fn pr_read(repo: &OzoneRepo, opts: &PrReadOptions) -> Result<PrReadOutcome> {
    let (client, owner, repo_name) = resolve_github_pr_client(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let pr = client.get_pull_request(&owner, &repo_name, opts.number)?;
    let norm_path = opts
        .path
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();

    // Fast path: if the PR head commit is already present locally in `.git`, read zero-network via `gix`.
    if let Ok(blob) = show_blob(
        repo,
        &ShowBlobOptions {
            rev: pr.head.sha.clone(),
            path: norm_path.clone(),
            start_line: opts.start_line,
            end_line: opts.end_line,
        },
    ) {
        return Ok(PrReadOutcome {
            owner,
            repo: repo_name,
            pr_number: pr.number,
            head_branch: pr.head.ref_name,
            source: "local_git".to_string(),
            blob,
        });
    }

    // Remote fallback: fetch raw file bytes at exact `pr.head.sha` via GitHub Contents API.
    let bytes = client.get_file_contents_at_ref(&owner, &repo_name, &norm_path, &pr.head.sha)?;
    let size_bytes = bytes.len();
    let short_oid = pr.head.sha[..pr.head.sha.len().min(7)].to_string();
    let blob = if is_binary_bytes(&bytes) {
        ShowBlobReport {
            rev: pr.head.sha.clone(),
            commit_oid: pr.head.sha.clone(),
            short_oid,
            path: norm_path,
            blob_oid: String::new(),
            size_bytes,
            is_binary: true,
            total_lines: 0,
            start_line: 0,
            end_line: 0,
            truncated: false,
            content: String::new(),
        }
    } else {
        let (total_lines, start_line, end_line, truncated, content) =
            slice_text_lines(&bytes, opts.start_line, opts.end_line);
        ShowBlobReport {
            rev: pr.head.sha.clone(),
            commit_oid: pr.head.sha.clone(),
            short_oid,
            path: norm_path,
            blob_oid: String::new(),
            size_bytes,
            is_binary: false,
            total_lines,
            start_line,
            end_line,
            truncated,
            content,
        }
    };

    Ok(PrReadOutcome {
        owner,
        repo: repo_name,
        pr_number: pr.number,
        head_branch: pr.head.ref_name,
        source: "github_api".to_string(),
        blob,
    })
}

/// Validates inline comments against PR diff hunks, auto-populates `commit_id` from the PR head,
/// and submits a pull request review (`oz pr review <number>`).
pub fn pr_review(repo: &OzoneRepo, opts: &PrReviewOptions) -> Result<PrReviewOutcome> {
    let (client, owner, repo_name) = resolve_github_pr_client(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    if !opts.dry_run {
        client.require_authenticated()?;
    }

    let pr = client.get_pull_request(&owner, &repo_name, opts.number)?;
    let validated_payloads = if opts.comments.is_empty() {
        Vec::new()
    } else {
        let pr_files = client.list_pull_request_files(&owner, &repo_name, opts.number)?;
        let pr_changes = pr_files_to_changes(&pr_files);
        validate_pr_review_comments(opts.number, &pr_changes, &opts.comments)?
    };

    if opts.event == PrReviewEvent::Comment
        && opts.body.as_deref().is_none_or(|b| b.trim().is_empty())
        && validated_payloads.is_empty()
    {
        return Err(OzError::InvalidUsage {
            message:
                "`oz pr review --comment` requires `--body` and/or at least one inline comment"
                    .to_string(),
            recovery: vec![format!(
                "oz pr review {} --comment --body \"LGTM\" or `--inline path:line=note`",
                opts.number
            )],
        });
    }

    if opts.dry_run {
        return Ok(PrReviewOutcome {
            owner,
            repo: repo_name,
            pr_number: pr.number,
            commit_id: pr.head.sha,
            event: opts.event.as_str().to_string(),
            dry_run: true,
            validated_comments: validated_payloads.len(),
            review: None,
        });
    }

    let payload = CreatePullRequestReviewPayload {
        commit_id: pr.head.sha.clone(),
        body: opts.body.clone().filter(|b| !b.trim().is_empty()),
        event: opts.event.as_str().to_string(),
        comments: validated_payloads,
    };
    let validated_comments = payload.comments.len();
    let review = client.create_pull_request_review(&owner, &repo_name, opts.number, &payload)?;

    Ok(PrReviewOutcome {
        owner,
        repo: repo_name,
        pr_number: pr.number,
        commit_id: pr.head.sha,
        event: opts.event.as_str().to_string(),
        dry_run: false,
        validated_comments,
        review: Some(review),
    })
}

// ============================================================================
// PR List, Edit, Reply, Resolve & Multi-PR Status
// ============================================================================

/// Options controlling [`pr_list`] (`oz pr list`).
#[derive(Debug, Clone)]
pub struct PrListOptions {
    /// PR state filter (`open`, `closed`, `merged`, `all`; defaults to `open`).
    pub state: Option<String>,
    /// Optional GitHub search query text (`--search`).
    pub search: Option<String>,
    /// Optional author login filter (`--author`).
    pub author: Option<String>,
    /// Optional base branch filter (`--base`).
    pub base: Option<String>,
    /// Optional head branch filter (`--head`).
    pub head: Option<String>,
    /// Optional label filters (`--label`).
    pub labels: Vec<String>,
    /// Maximum number of pull requests to return (defaults to `20`).
    pub limit: usize,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

impl Default for PrListOptions {
    fn default() -> Self {
        Self {
            state: None,
            search: None,
            author: None,
            base: None,
            head: None,
            labels: Vec::new(),
            limit: 20,
            remote: None,
            auth_token: None,
            api_url: None,
        }
    }
}

/// Compact pull request summary returned by [`pr_list`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrListItem {
    pub number: u64,
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_commit_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub html_url: String,
}

/// Result returned by [`pr_list`] (`oz pr list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrListOutcome {
    pub owner: String,
    pub repo: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    pub total_returned: usize,
    pub pull_requests: Vec<PrListItem>,
}

/// Lists or searches GitHub pull requests via REST (`oz pr list`).
pub fn pr_list(repo: &OzoneRepo, opts: &PrListOptions) -> Result<PrListOutcome> {
    let (client, owner, repo_name) = resolve_github_pr_client(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let state = opts
        .state
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "open".to_string());
    if !matches!(state.as_str(), "open" | "closed" | "merged" | "all") {
        return Err(OzError::InvalidUsage {
            message: format!("invalid PR state '{state}': expected open, closed, merged, or all"),
            recovery: vec!["oz pr list --state open".to_string()],
        });
    }
    let limit = if opts.limit == 0 {
        20
    } else {
        opts.limit.min(100)
    };

    let use_search = opts.search.as_deref().is_some_and(|s| !s.trim().is_empty())
        || opts.author.as_deref().is_some_and(|s| !s.trim().is_empty())
        || !opts.labels.is_empty()
        || state == "merged";

    let pull_requests: Vec<PrListItem> = if use_search {
        let mut q_parts = vec![format!("repo:{owner}/{repo_name}"), "is:pr".to_string()];
        match state.as_str() {
            "open" => q_parts.push("is:open".to_string()),
            "closed" => q_parts.push("is:closed".to_string()),
            "merged" => q_parts.push("is:merged".to_string()),
            _ => {}
        }
        if let Some(ref author) = opts.author {
            let trimmed = author.trim().trim_start_matches('@');
            if !trimmed.is_empty() {
                q_parts.push(format!("author:{trimmed}"));
            }
        }
        if let Some(ref base) = opts.base {
            let trimmed = normalize_short_branch(base);
            if !trimmed.is_empty() {
                q_parts.push(format!("base:{trimmed}"));
            }
        }
        if let Some(ref head) = opts.head {
            let trimmed = normalize_short_branch(head);
            if !trimmed.is_empty() {
                q_parts.push(format!("head:{trimmed}"));
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
            .into_iter()
            .take(limit)
            .map(|item| {
                let pr_state = if state == "merged" {
                    "MERGED".to_string()
                } else {
                    item.state.to_ascii_uppercase()
                };
                PrListItem {
                    number: item.number,
                    state: pr_state,
                    draft: false,
                    title: item.title,
                    author: item.user.map(|u| u.login),
                    head_branch: None,
                    base_branch: None,
                    head_sha: None,
                    merge_commit_sha: None,
                    labels: item.labels.into_iter().map(|l| l.name).collect(),
                    updated_at: item.updated_at,
                    html_url: item.html_url,
                }
            })
            .collect()
    } else {
        let head_param = opts.head.as_deref().map(|h| {
            let trimmed = normalize_short_branch(h);
            if trimmed.contains(':') {
                trimmed
            } else {
                format!("{owner}:{trimmed}")
            }
        });
        let base_param = opts.base.as_deref().map(normalize_short_branch);
        let raw = client.list_pull_requests_limited(
            &owner,
            &repo_name,
            Some(&state),
            head_param.as_deref(),
            base_param.as_deref(),
            limit,
        )?;
        raw.into_iter()
            .take(limit)
            .map(|pr| PrListItem {
                number: pr.number,
                state: format_pr_status(&pr),
                draft: pr.draft,
                title: pr.title,
                author: pr.user.map(|u| u.login),
                head_branch: Some(pr.head.ref_name),
                base_branch: Some(pr.base.ref_name),
                head_sha: Some(pr.head.sha),
                merge_commit_sha: pr.merge_commit_sha,
                labels: Vec::new(),
                updated_at: pr.updated_at,
                html_url: pr.html_url,
            })
            .collect()
    };

    let total_returned = pull_requests.len();
    Ok(PrListOutcome {
        owner,
        repo: repo_name,
        state,
        search: opts.search.clone(),
        total_returned,
        pull_requests,
    })
}

/// Options controlling [`pr_edit`] (`oz pr edit <number>`).
#[derive(Debug, Clone, Default)]
pub struct PrEditOptions {
    pub number: u64,
    pub title: Option<String>,
    pub body: Option<String>,
    pub base: Option<String>,
    pub state: Option<String>,
    pub draft: Option<bool>,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

/// Result returned by [`pr_edit`] (`oz pr edit <number>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrEditOutcome {
    pub owner: String,
    pub repo: String,
    pub pull_request: GitHubPullRequest,
    pub updated_fields: Vec<String>,
}

/// Updates an existing GitHub pull request's title, body, base branch, state, or draft flag.
pub fn pr_edit(repo: &OzoneRepo, opts: &PrEditOptions) -> Result<PrEditOutcome> {
    if opts.number == 0 {
        return Err(OzError::InvalidUsage {
            message: "pull request number must be positive".to_string(),
            recovery: vec!["oz pr edit 3340 --title \"...\"".to_string()],
        });
    }
    if opts.title.is_none()
        && opts.body.is_none()
        && opts.base.is_none()
        && opts.state.is_none()
        && opts.draft.is_none()
    {
        return Err(OzError::InvalidUsage {
            message: "no fields specified to edit; pass at least one of --title, --body, --body-file, --base, --state, --draft, or --ready".to_string(),
            recovery: vec![
                format!("oz pr edit {} --title \"New title\"", opts.number),
                format!("oz pr edit {} --body-file -", opts.number),
            ],
        });
    }
    if let Some(ref st) = opts.state
        && !matches!(st.trim().to_ascii_lowercase().as_str(), "open" | "closed")
    {
        return Err(OzError::InvalidUsage {
            message: format!("invalid PR state '{st}': expected open or closed"),
            recovery: vec![format!("oz pr edit {} --state closed", opts.number)],
        });
    }

    let (client, owner, repo_name) = resolve_github_pr_client(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let _ = client.require_authenticated()?;

    let mut updated_fields = Vec::new();
    let mut patch = UpdatePullRequest::default();
    if let Some(ref title) = opts.title {
        patch.title = Some(title.clone());
        updated_fields.push("title".to_string());
    }
    if let Some(ref body) = opts.body {
        // Preserve any managed stack navigation table already present in the PR body.
        let existing_stack = client
            .get_pull_request(&owner, &repo_name, opts.number)
            .ok()
            .and_then(|pr| pr.body)
            .and_then(|b| extract_existing_stack_block(&b));
        let final_body = match existing_stack {
            Some(block) if !body.contains(OZ_STACK_START) => {
                upsert_stack_navigation_block(body, &block)
            }
            _ => body.clone(),
        };
        patch.body = Some(final_body);
        updated_fields.push("body".to_string());
    }
    if let Some(ref base) = opts.base {
        patch.base = Some(normalize_short_branch(base));
        updated_fields.push("base".to_string());
    }
    if let Some(ref state) = opts.state {
        patch.state = Some(state.trim().to_ascii_lowercase());
        updated_fields.push("state".to_string());
    }

    let mut pull_request = if patch.title.is_some()
        || patch.body.is_some()
        || patch.base.is_some()
        || patch.state.is_some()
    {
        client.update_pull_request(&owner, &repo_name, opts.number, &patch)?
    } else {
        client.get_pull_request(&owner, &repo_name, opts.number)?
    };

    if let Some(draft_flag) = opts.draft {
        let is_draft =
            client.set_pull_request_draft_graphql(&owner, &repo_name, opts.number, draft_flag)?;
        pull_request.draft = is_draft;
        updated_fields.push(if draft_flag { "draft" } else { "ready" }.to_string());
    }

    Ok(PrEditOutcome {
        owner,
        repo: repo_name,
        pull_request,
        updated_fields,
    })
}

/// Options controlling [`pr_reply`] (`oz pr reply <number> <target> --body "..."`).
#[derive(Debug, Clone, Default)]
pub struct PrReplyOptions {
    pub number: u64,
    /// Thread handle (`t1` / `[t1]`), numeric comment ID (`4694921942` / `#4694921942`), or GraphQL thread ID (`PRRT_...`).
    pub target: String,
    pub body: String,
    /// Also mark the thread resolved via GraphQL after replying (`--resolve`).
    pub resolve: bool,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

/// Result returned by [`pr_reply`] (`oz pr reply`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrReplyOutcome {
    pub owner: String,
    pub repo: String,
    pub pr_number: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub in_reply_to_comment_id: u64,
    pub reply: PrReviewThreadComment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<bool>,
}

fn resolve_thread_target(
    threads: &[PrReviewThread],
    raw_target: &str,
) -> Option<(Option<String>, Option<String>, u64)> {
    let cleaned = raw_target
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if let Some(num_str) = cleaned.strip_prefix('#')
        && let Ok(cid) = num_str.parse::<u64>()
    {
        if let Some(t) = threads
            .iter()
            .find(|t| t.root_comment_id == cid || t.comments.iter().any(|c| c.id == cid))
        {
            return Some((Some(t.id.clone()), t.thread_id.clone(), t.root_comment_id));
        }
        return Some((None, None, cid));
    }
    if let Ok(cid) = cleaned.parse::<u64>() {
        if let Some(t) = threads
            .iter()
            .find(|t| t.root_comment_id == cid || t.comments.iter().any(|c| c.id == cid))
        {
            return Some((Some(t.id.clone()), t.thread_id.clone(), t.root_comment_id));
        }
        return Some((None, None, cid));
    }
    if let Some(t) = threads
        .iter()
        .find(|t| t.id.eq_ignore_ascii_case(cleaned) || t.thread_id.as_deref() == Some(cleaned))
    {
        return Some((Some(t.id.clone()), t.thread_id.clone(), t.root_comment_id));
    }
    None
}

/// Posts an inline reply to a PR review thread identified by `[t1]` handle, numeric comment ID, or GraphQL thread ID.
pub fn pr_reply(repo: &OzoneRepo, opts: &PrReplyOptions) -> Result<PrReplyOutcome> {
    if opts.number == 0 {
        return Err(OzError::InvalidUsage {
            message: "pull request number must be positive".to_string(),
            recovery: vec!["oz pr reply 3347 t1 -m \"Fixed in latest commit\"".to_string()],
        });
    }
    if opts.body.trim().is_empty() {
        return Err(OzError::InvalidUsage {
            message: "reply body cannot be empty".to_string(),
            recovery: vec![format!(
                "oz pr reply {} {} -m \"...\"",
                opts.number, opts.target
            )],
        });
    }
    let (client, owner, repo_name) = resolve_github_pr_client(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let _ = client.require_authenticated()?;

    let cleaned = opts
        .target
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    let direct_comment_id = cleaned
        .strip_prefix('#')
        .unwrap_or(cleaned)
        .parse::<u64>()
        .ok();

    let (thread_handle, thread_id, root_comment_id) = if let Some(cid) = direct_comment_id
        && !opts.resolve
    {
        (None, None, cid)
    } else {
        let raw_inline = client.list_pull_request_comments(&owner, &repo_name, opts.number)?;
        let gql_meta = client
            .list_pull_request_review_threads_graphql(&owner, &repo_name, opts.number)
            .unwrap_or_default();
        let threads = group_review_threads(&raw_inline, &gql_meta);
        resolve_thread_target(&threads, &opts.target).ok_or_else(|| OzError::InvalidUsage {
            message: format!(
                "review thread '{}' not found on PR #{}",
                opts.target, opts.number
            ),
            recovery: vec![format!("oz pr comments {} --unresolved", opts.number)],
        })?
    };

    let created = client.reply_to_pull_request_comment(
        &owner,
        &repo_name,
        opts.number,
        root_comment_id,
        &opts.body,
    )?;

    let resolved = if opts.resolve {
        let tid = thread_id.as_deref().ok_or_else(|| OzError::CheckFailed {
            message: format!(
                "reply posted (#{}), but GraphQL thread ID could not be resolved for '{}'",
                created.id, opts.target
            ),
            recovery: vec![format!("oz pr comments {} --json", opts.number)],
        })?;
        Some(client.set_pull_request_review_thread_resolved_graphql(tid, true)?)
    } else {
        None
    };

    Ok(PrReplyOutcome {
        owner,
        repo: repo_name,
        pr_number: opts.number,
        thread_handle,
        thread_id,
        in_reply_to_comment_id: root_comment_id,
        reply: PrReviewThreadComment {
            id: created.id,
            review_id: created.pull_request_review_id,
            author: created.user.map(|u| u.login),
            body: created.body,
            created_at: created.created_at,
            html_url: created.html_url,
        },
        resolved,
    })
}

/// Options controlling [`pr_resolve`] (`oz pr resolve <number> <target>`).
#[derive(Debug, Clone, Default)]
pub struct PrResolveOptions {
    pub number: u64,
    /// Thread handle (`t1` / `[t1]`), GraphQL thread ID (`PRRT_...`), or numeric comment ID (`#4694921942`).
    pub target: String,
    /// When `true`, marks the thread unresolved instead of resolved (`--unresolve`).
    pub unresolve: bool,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

/// Result returned by [`pr_resolve`] (`oz pr resolve`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrResolveOutcome {
    pub owner: String,
    pub repo: String,
    pub pr_number: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_handle: Option<String>,
    pub thread_id: String,
    pub is_resolved: bool,
}

/// Resolves or unresolves an inline PR review thread via GitHub GraphQL.
pub fn pr_resolve(repo: &OzoneRepo, opts: &PrResolveOptions) -> Result<PrResolveOutcome> {
    if opts.number == 0 {
        return Err(OzError::InvalidUsage {
            message: "pull request number must be positive".to_string(),
            recovery: vec!["oz pr resolve 3347 t1".to_string()],
        });
    }
    let (client, owner, repo_name) = resolve_github_pr_client(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let _ = client.require_authenticated()?;

    let cleaned = opts
        .target
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();

    let (thread_handle, thread_id) = if cleaned.starts_with("PRRT_") {
        (None, cleaned.to_string())
    } else {
        let raw_inline = client.list_pull_request_comments(&owner, &repo_name, opts.number)?;
        let gql_meta =
            client.list_pull_request_review_threads_graphql(&owner, &repo_name, opts.number)?;
        let threads = group_review_threads(&raw_inline, &gql_meta);
        let (handle, tid_opt, _) =
            resolve_thread_target(&threads, cleaned).ok_or_else(|| OzError::InvalidUsage {
                message: format!(
                    "review thread '{}' not found on PR #{}",
                    opts.target, opts.number
                ),
                recovery: vec![format!("oz pr comments {} --unresolved", opts.number)],
            })?;
        let tid = tid_opt.ok_or_else(|| OzError::CheckFailed {
            message: format!(
                "no GraphQL thread ID available for review thread '{}' on PR #{}",
                opts.target, opts.number
            ),
            recovery: vec![format!("oz pr comments {} --json", opts.number)],
        })?;
        (handle, tid)
    };

    let is_resolved =
        client.set_pull_request_review_thread_resolved_graphql(&thread_id, !opts.unresolve)?;
    Ok(PrResolveOutcome {
        owner,
        repo: repo_name,
        pr_number: opts.number,
        thread_handle,
        thread_id,
        is_resolved,
    })
}

/// Options controlling [`pr_status`] (`oz pr status [--pr 3338,3339,3340]`).
#[derive(Debug, Clone, Default)]
pub struct PrStatusOptions {
    /// Pull request numbers to summarize; if empty, inspects the open PR for the current branch.
    pub numbers: Vec<u64>,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

/// One-line multi-PR status summary item returned by [`pr_status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStatusItem {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub head_branch: String,
    pub base_branch: String,
    pub head_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<String>,
    pub unresolved_threads: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mergeable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mergeable_state: Option<String>,
    pub ci_status: String,
    pub ci_passing: usize,
    pub ci_failing: usize,
    pub ci_pending: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub html_url: String,
}

/// Compact batch status report returned by [`pr_status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStatusOutcome {
    pub owner: String,
    pub repo: String,
    pub pr_count: usize,
    pub prs: Vec<PrStatusItem>,
}

/// Fetches compact one-line status (state, review verdict, unresolved threads, mergeability, CI summary)
/// across one or more pull requests (`oz pr status --pr 3338,3339,3340`).
pub fn pr_status(repo: &OzoneRepo, opts: &PrStatusOptions) -> Result<PrStatusOutcome> {
    let (client, owner, repo_name) = resolve_github_pr_client(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;

    let mut target_numbers = Vec::new();
    let mut seen = HashSet::new();
    for &n in &opts.numbers {
        if n > 0 && seen.insert(n) {
            target_numbers.push(n);
        }
    }
    if target_numbers.is_empty() {
        let branch = repo
            .branch
            .as_deref()
            .ok_or_else(|| OzError::InvalidUsage {
                message: "no PR numbers specified and HEAD is detached; pass `--pr <num,...>`"
                    .to_string(),
                recovery: vec!["oz pr status --pr 3338,3339 --json".to_string()],
            })?;
        let pr = find_open_pr_for_head(&client, &owner, &repo_name, branch)?.ok_or_else(|| {
            OzError::CheckFailed {
                message: format!("no open pull request found for branch '{branch}'"),
                recovery: vec!["oz pr status --pr <number>".to_string()],
            }
        })?;
        target_numbers.push(pr.number);
    }

    let mut prs = Vec::with_capacity(target_numbers.len());
    for num in target_numbers {
        let pr = client.get_pull_request(&owner, &repo_name, num)?;
        let reviews = summarize_pr_reviews(
            client
                .list_pull_request_reviews(&owner, &repo_name, num)
                .unwrap_or_default(),
        );
        let review_decision = derive_review_decision(&reviews);
        let raw_inline = client
            .list_pull_request_comments(&owner, &repo_name, num)
            .unwrap_or_default();
        let gql_meta = client
            .list_pull_request_review_threads_graphql(&owner, &repo_name, num)
            .unwrap_or_default();
        let threads = group_review_threads(&raw_inline, &gql_meta);
        let unresolved_threads = threads
            .iter()
            .filter(|t| t.is_resolved != Some(true))
            .count();

        let check_runs = client
            .list_check_runs_for_ref(&owner, &repo_name, &pr.head.sha)
            .unwrap_or_default();
        let mut ci_passing = 0usize;
        let mut ci_failing = 0usize;
        let mut ci_pending = 0usize;
        for cr in &check_runs {
            if !cr.status.eq_ignore_ascii_case("completed") {
                ci_pending += 1;
            } else if matches!(
                cr.conclusion
                    .as_deref()
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("success" | "neutral" | "skipped")
            ) {
                ci_passing += 1;
            } else {
                ci_failing += 1;
            }
        }
        let ci_status = if check_runs.is_empty() {
            "no_checks"
        } else if ci_failing > 0 {
            "failing"
        } else if ci_pending > 0 {
            "pending"
        } else {
            "passing"
        }
        .to_string();

        prs.push(PrStatusItem {
            number: pr.number,
            title: pr.title.clone(),
            state: format_pr_status(&pr),
            head_branch: pr.head.ref_name.clone(),
            base_branch: pr.base.ref_name.clone(),
            head_sha: pr.head.sha.clone(),
            review_decision,
            unresolved_threads,
            mergeable: pr.mergeable,
            mergeable_state: pr.mergeable_state.clone(),
            ci_status,
            ci_passing,
            ci_failing,
            ci_pending,
            html_url: pr.html_url,
        });
    }

    let pr_count = prs.len();
    Ok(PrStatusOutcome {
        owner,
        repo: repo_name,
        pr_count,
        prs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_navigation_block_renders_and_upserts_idempotently() {
        let items = vec![
            StackPrItem {
                number: 10,
                html_url: "https://github.com/acme/repo/pull/10".to_string(),
                title: "Part 1".to_string(),
                head_branch: "stack-1".to_string(),
                base_branch: "main".to_string(),
                status: "Open".to_string(),
                created: true,
                change_ids: vec!["I111".to_string()],
            },
            StackPrItem {
                number: 11,
                html_url: "https://github.com/acme/repo/pull/11".to_string(),
                title: "Part 2".to_string(),
                head_branch: "stack-2".to_string(),
                base_branch: "stack-1".to_string(),
                status: "Draft".to_string(),
                created: true,
                change_ids: vec!["I222".to_string()],
            },
        ];

        let block_10 = render_stack_navigation_block("main", &items, 10);
        assert!(block_10.contains(
            "| 👉 | [#10](https://github.com/acme/repo/pull/10) | `stack-1` | `main` | Open |"
        ));
        assert!(block_10.contains(
            "|  | [#11](https://github.com/acme/repo/pull/11) | `stack-2` | `stack-1` | Draft |"
        ));

        let body1 =
            upsert_stack_navigation_block("Initial description\n\nChange-Id: I111", &block_10);
        let block_10_updated = render_stack_navigation_block("main", &items, 11);
        let body2 = upsert_stack_navigation_block(&body1, &block_10_updated);

        assert_eq!(body2.matches(OZ_STACK_START).count(), 1);
        assert_eq!(body2.matches(OZ_STACK_END).count(), 1);
        assert!(body2.starts_with("Initial description\n\nChange-Id: I111"));
        assert!(body2.contains("| 👉 | [#11]"));
    }

    #[test]
    fn parse_github_patch_extracts_exact_line_numbers_and_hunk_ids() {
        let patch = "\
@@ -396,4 +396,5 @@ export function flush() {
 const a = 1;
-const b = 2;
+const b = 3;
+const c = 4;
 return a + b;
\\ No newline at end of file";
        let hunks = parse_github_patch_to_hunks("f1", 1, "src/index.ts", patch);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.id, "h1a");
        assert_eq!(h.old_start, 396);
        assert_eq!(h.new_start, 396);
        assert_eq!(h.lines.len(), 5);
        assert_eq!(
            (h.lines[0].old_line, h.lines[0].new_line),
            (Some(396), Some(396))
        );
        assert_eq!(
            (h.lines[1].old_line, h.lines[1].new_line),
            (Some(397), None)
        );
        assert_eq!(
            (h.lines[2].old_line, h.lines[2].new_line),
            (None, Some(397))
        );
        assert_eq!(
            (h.lines[3].old_line, h.lines[3].new_line),
            (None, Some(398))
        );
        assert_eq!(
            (h.lines[4].old_line, h.lines[4].new_line),
            (Some(398), Some(399))
        );
    }

    #[test]
    fn validate_pr_review_comments_validates_hunk_bounds_and_sides() {
        let pr_files = vec![GitHubPullRequestFile {
            filename: "src/index.ts".to_string(),
            previous_filename: None,
            status: "modified".to_string(),
            additions: 2,
            deletions: 1,
            changes: 3,
            sha: None,
            patch: Some("@@ -10,3 +10,4 @@\n ctx1\n-old2\n+new2\n+new3\n ctx4".to_string()),
        }];
        let changes = pr_files_to_changes(&pr_files);

        // Valid RIGHT and LEFT comments succeed
        let ok = validate_pr_review_comments(
            42,
            &changes,
            &[
                parse_inline_comment_spec("src/index.ts:12=Check added line").unwrap(),
                parse_inline_comment_spec("src/index.ts:11:LEFT=Why remove old2?").unwrap(),
                parse_inline_comment_spec("src/index.ts:11-12:RIGHT=Multi-line note").unwrap(),
            ],
        )
        .expect("valid comments");
        assert_eq!(ok.len(), 3);
        assert_eq!(ok[0].side, "RIGHT");
        assert_eq!(ok[1].side, "LEFT");
        assert_eq!(ok[2].start_line, Some(11));
        assert_eq!(ok[2].line, 12);

        // Out-of-hunk line fails locally with valid line-range hint
        let err = validate_pr_review_comments(
            42,
            &changes,
            &[parse_inline_comment_spec("src/index.ts:99=Out of range").unwrap()],
        )
        .expect_err("out of range must fail");
        let msg = err.to_string();
        assert!(msg.contains("outside PR #42 diff hunks"));
        assert!(msg.contains("10..=13"));

        // Unknown file path fails locally
        let err2 = validate_pr_review_comments(
            42,
            &changes,
            &[parse_inline_comment_spec("src/other.ts:10=Wrong file").unwrap()],
        )
        .expect_err("unknown path must fail");
        assert!(err2.to_string().contains("not in PR #42 changed files"));
    }
}
