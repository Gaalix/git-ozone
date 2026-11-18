// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pull request creation (`oz pr create`) and stacked PR orchestration (`oz pr stack`)
//! with `Change-Id:` preservation, atomic multi-branch pushes, and deterministic
//! `<!-- oz-stack-start --> ... <!-- oz-stack-end -->` cross-link navigation tables.

use crate::PullRequestSpec;
use crate::api::{GitHubClient, GitHubPullRequest, UpdatePullRequest};
use crate::wire::{
    HttpAuthScheme, PushOptions, is_ancestor_commit, open_gix_unrestricted, push_refs, read_ref_oid,
};
use gix::bstr::ByteSlice;
use gix::hash::ObjectId;
use oz_core::{OzError, OzoneRepo, Result};
use oz_integrate::{StackBranchStatus, stack_status};
use oz_oplog::ActorInfo;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

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
            recovery: vec!["Spawn or switch to a feature branch before running `oz pr create`.".to_string()],
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
        .map(str::to_string)
        .unwrap_or(inferred_title);

    let raw_body = opts
        .body
        .clone()
        .unwrap_or(inferred_body);
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
        open_by_head
            .entry(pr.head.ref_name.clone())
            .or_insert(pr);
    }

    struct DraftedStackPr {
        pr: GitHubPullRequest,
        base_branch: String,
        head_branch: String,
        user_body: String,
        created: bool,
        change_ids: Vec<String>,
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
            let existing_clean = strip_stack_navigation_block(
                existing.body.as_deref().unwrap_or_default(),
            );
            let base_prose = if existing_clean.trim().is_empty() {
                inferred_body
            } else {
                existing_clean
            };
            let user_body = attach_change_ids_to_body(&base_prose, &change_ids);
            let pr = if existing.base.ref_name != expected_base {
                client.update_pull_request(
                    owner,
                    gh_repo,
                    existing.number,
                    &UpdatePullRequest {
                        base: Some(expected_base.clone()),
                        ..Default::default()
                    },
                )?
            } else {
                existing
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
pub fn render_stack_navigation_block(
    root_base: &str,
    items: &[StackPrItem],
    current_pr_number: u64,
) -> String {
    let mut out = String::new();
    out.push_str(OZ_STACK_START);
    out.push('\n');
    out.push_str(&format!("### Stacked PRs (base: `{root_base}`)\n\n"));
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
        out.push_str(&format!(
            "| {} | {} | `{}` | `{}` | {} |\n",
            marker, pr_link, item.head_branch, item.base_branch, item.status
        ));
    }
    out.push_str(OZ_STACK_END);
    out
}

/// Inserts or replaces the `<!-- oz-stack-start --> ... <!-- oz-stack-end -->` section in `body` idempotently.
#[must_use]
pub fn upsert_stack_navigation_block(body: &str, block: &str) -> String {
    if let (Some(start_idx), Some(end_rel)) = (
        body.find(OZ_STACK_START),
        body.find(OZ_STACK_END),
    ) && end_rel >= start_idx
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
pub fn strip_stack_navigation_block(body: &str) -> String {
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
                    cur_parent = by_name.get(p).and_then(| info| info.parent_branch.clone());
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
                parent_branch: b
                    .parent_branch
                    .clone()
                    .filter(|p| selected.contains(p)),
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
    Ok(!is_ancestor_commit(gix_repo, tracking_oid, local_oid)?)
}

/// Inspects commits in `(base_branch..head_branch]` (oldest-first) to infer PR title, body, and `Change-Id`s.
pub fn infer_title_body_and_change_ids(
    gix_repo: &gix::Repository,
    base_branch: &str,
    head_branch: &str,
) -> Result<(String, String, Vec<String>)> {
    let head_ref = format!("refs/heads/{head_branch}");
    let head_oid = read_ref_oid(gix_repo, &head_ref)?.ok_or_else(|| OzError::InvalidUsage {
        message: format!("local branch '{head_branch}' does not exist"),
        recovery: vec!["oz status --json".to_string()],
    })?;

    let base_oid = read_ref_oid(gix_repo, &format!("refs/heads/{base_branch}"))?
        .or_else(|| {
            read_ref_oid(gix_repo, &format!("refs/remotes/origin/{base_branch}"))
                .ok()
                .flatten()
        });

    let stop_oid = match base_oid {
        Some(b_id) => gix_repo
            .merge_base(b_id, head_oid)
            .ok()
            .flatten()
            .map(|id| id.detach()),
        None => None,
    };

    let commits_oldest_first = collect_first_parent_commits(gix_repo, head_oid, stop_oid)?;
    let mut subjects = Vec::new();
    let mut bodies = Vec::new();
    let mut change_ids = Vec::new();

    for cid in &commits_oldest_first {
        let commit = gix_repo
            .find_commit(*cid)
            .map_err(|err| OzError::Git(err.to_string()))?;
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
            summary.push_str(&format!("- {s}\n"));
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
        out.push_str(&format!("Change-Id: {cid}"));
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
        let commit = gix_repo
            .find_commit(cid)
            .map_err(|err| OzError::Git(err.to_string()))?;
        cur = commit.parent_ids().next().map(|id| id.detach());
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
    t.strip_prefix("refs/heads/")
        .unwrap_or(t)
        .to_string()
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
        assert!(block_10.contains("| 👉 | [#10](https://github.com/acme/repo/pull/10) | `stack-1` | `main` | Open |"));
        assert!(block_10.contains("|  | [#11](https://github.com/acme/repo/pull/11) | `stack-2` | `stack-1` | Draft |"));

        let body1 = upsert_stack_navigation_block("Initial description\n\nChange-Id: I111", &block_10);
        let block_10_updated = render_stack_navigation_block("main", &items, 11);
        let body2 = upsert_stack_navigation_block(&body1, &block_10_updated);

        assert_eq!(body2.matches(OZ_STACK_START).count(), 1);
        assert_eq!(body2.matches(OZ_STACK_END).count(), 1);
        assert!(body2.starts_with("Initial description\n\nChange-Id: I111"));
        assert!(body2.contains("| 👉 | [#11]"));
    }
}
