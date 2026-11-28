// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! GitHub Releases workflow (`oz release list|view|create|edit`) backed by the pure-Rust
//! [`GitHubClient`] REST API.

use crate::api::{
    CreateReleasePayload, GitHubClient, GitHubRelease, GitHubReleaseAsset, UpdateReleasePayload,
};
use oz_core::{OzError, OzoneRepo, Result};
use serde::{Deserialize, Serialize};

/// Compact release asset metadata returned in release outcomes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseAssetSummary {
    pub id: u64,
    pub name: String,
    pub size: u64,
    pub download_count: u64,
    pub content_type: String,
    pub browser_download_url: String,
}

impl From<GitHubReleaseAsset> for ReleaseAssetSummary {
    fn from(asset: GitHubReleaseAsset) -> Self {
        Self {
            id: asset.id,
            name: asset.name,
            size: asset.size,
            download_count: asset.download_count,
            content_type: asset.content_type,
            browser_download_url: asset.browser_download_url,
        }
    }
}

/// Options controlling [`release_list`] (`oz release list`).
#[derive(Debug, Clone)]
pub struct ReleaseListOptions {
    /// Maximum number of releases to return (defaults to `20`, capped at `100`).
    pub limit: usize,
    /// Exclude draft releases from results.
    pub exclude_drafts: bool,
    /// Exclude prereleases from results.
    pub exclude_prereleases: bool,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

impl Default for ReleaseListOptions {
    fn default() -> Self {
        Self {
            limit: 20,
            exclude_drafts: false,
            exclude_prereleases: false,
            remote: None,
            auth_token: None,
            api_url: None,
        }
    }
}

/// Compact release summary entry returned by [`release_list`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseListItem {
    pub id: u64,
    pub tag_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub html_url: String,
    pub draft: bool,
    pub prerelease: bool,
    pub is_latest: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    pub asset_count: usize,
}

/// Result returned by [`release_list`] (`oz release list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseListOutcome {
    pub owner: String,
    pub repo: String,
    pub total_returned: usize,
    pub releases: Vec<ReleaseListItem>,
}

/// Options controlling [`release_view`] (`oz release view [<TAG>]`).
#[derive(Debug, Clone, Default)]
pub struct ReleaseViewOptions {
    /// Specific release tag to fetch. When `None` (or `latest == true`), fetches `/releases/latest`.
    pub tag: Option<String>,
    /// Explicitly fetch the latest published release.
    pub latest: bool,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

/// Structured result returned by [`release_view`] (`oz release view`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseViewOutcome {
    pub owner: String,
    pub repo: String,
    pub id: u64,
    pub tag_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub html_url: String,
    pub draft: bool,
    pub prerelease: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_commitish: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    pub assets: Vec<ReleaseAssetSummary>,
}

/// Options controlling [`release_create`] (`oz release create <TAG>`).
#[derive(Debug, Clone, Default)]
pub struct ReleaseCreateOptions {
    pub tag: String,
    pub title: Option<String>,
    pub notes: Option<String>,
    pub generate_notes: bool,
    pub target: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub latest: bool,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

/// Structured result returned by [`release_create`] (`oz release create`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseCreateOutcome {
    pub owner: String,
    pub repo: String,
    pub id: u64,
    pub tag_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub html_url: String,
    pub draft: bool,
    pub prerelease: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_commitish: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    pub assets: Vec<ReleaseAssetSummary>,
}

/// Options controlling [`release_edit`] (`oz release edit <TAG>`).
#[derive(Debug, Clone, Default)]
pub struct ReleaseEditOptions {
    /// Existing release tag to locate (supports draft releases via listing fallback).
    pub tag: String,
    /// Optional new tag name (`--tag`).
    pub new_tag: Option<String>,
    /// Updated release title (`--title`).
    pub title: Option<String>,
    /// Updated release notes markdown (`--notes` / `--notes-file`).
    pub notes: Option<String>,
    /// Updated draft flag (`Some(true)` for `--draft`, `Some(false)` for `--publish`).
    pub draft: Option<bool>,
    /// Updated prerelease flag (`Some(true)` for `--prerelease`, `Some(false)` for `--no-prerelease`).
    pub prerelease: Option<bool>,
    /// Explicitly promote this release to latest (`--latest`).
    pub latest: bool,
    /// Updated target commitish (`--target`).
    pub target: Option<String>,
    pub remote: Option<String>,
    pub auth_token: Option<String>,
    pub api_url: Option<String>,
}

/// Structured result returned by [`release_edit`] (`oz release edit`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseEditOutcome {
    pub owner: String,
    pub repo: String,
    pub id: u64,
    pub tag_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub html_url: String,
    pub draft: bool,
    pub prerelease: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_commitish: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    pub assets: Vec<ReleaseAssetSummary>,
    pub updated_fields: Vec<String>,
}

fn is_not_found_error(err: &OzError) -> bool {
    matches!(err, OzError::CheckFailed { message, .. } if message.contains("(404)"))
}

fn non_empty_trimmed(opt: Option<&str>) -> Option<String> {
    opt.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn resolve_release_by_tag_with_draft_fallback(
    client: &GitHubClient,
    owner: &str,
    repo_name: &str,
    tag: &str,
) -> Result<GitHubRelease> {
    match client.get_release_by_tag(owner, repo_name, tag) {
        Ok(rel) => Ok(rel),
        Err(err) if is_not_found_error(&err) => {
            // GitHub's `GET /releases/tags/{tag}` only returns published releases and returns 404
            // for draft releases. Fall back to listing releases to locate draft releases by tag.
            for page in 1u32..=5 {
                let Ok(batch) = client.list_releases(owner, repo_name, 100, page) else {
                    break;
                };
                let count = batch.len();
                if let Some(found) = batch.into_iter().find(|r| r.tag_name == tag) {
                    return Ok(found);
                }
                if count < 100 {
                    break;
                }
            }
            Err(OzError::CheckFailed {
                message: format!("release '{tag}' not found in {owner}/{repo_name}"),
                recovery: vec!["oz release list".to_string()],
            })
        }
        Err(err) => Err(err),
    }
}

/// Lists GitHub Releases for the repository (`oz release list`).
pub fn release_list(repo: &OzoneRepo, opts: &ReleaseListOptions) -> Result<ReleaseListOutcome> {
    let (client, remote) = GitHubClient::for_repo(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let (owner, repo_name) = remote.require_owner_repo()?;
    let limit = if opts.limit == 0 {
        20
    } else {
        opts.limit.min(100)
    };
    let fetch_per_page = if opts.exclude_drafts || opts.exclude_prereleases {
        limit.saturating_mul(2).clamp(limit, 100)
    } else {
        limit
    };

    let raw = client.list_releases(owner, repo_name, fetch_per_page, 1)?;
    let latest_id = raw.iter().find(|r| !r.draft && !r.prerelease).map(|r| r.id);

    let releases: Vec<ReleaseListItem> = raw
        .into_iter()
        .filter(|r| {
            (!opts.exclude_drafts || !r.draft) && (!opts.exclude_prereleases || !r.prerelease)
        })
        .take(limit)
        .map(|r| {
            let is_latest = latest_id == Some(r.id);
            ReleaseListItem {
                id: r.id,
                tag_name: r.tag_name,
                name: r.name.filter(|n| !n.trim().is_empty()),
                html_url: r.html_url,
                draft: r.draft,
                prerelease: r.prerelease,
                is_latest,
                author: r.author.map(|u| u.login),
                created_at: r.created_at,
                published_at: r.published_at,
                asset_count: r.assets.len(),
            }
        })
        .collect();
    let total_returned = releases.len();

    Ok(ReleaseListOutcome {
        owner: owner.to_string(),
        repo: repo_name.to_string(),
        total_returned,
        releases,
    })
}

/// Fetches a GitHub Release by tag or `/releases/latest` (`oz release view`).
pub fn release_view(repo: &OzoneRepo, opts: &ReleaseViewOptions) -> Result<ReleaseViewOutcome> {
    let tag = non_empty_trimmed(opts.tag.as_deref());
    if opts.latest && tag.is_some() {
        return Err(OzError::InvalidUsage {
            message: "cannot pass both a release <TAG> and `--latest`".to_string(),
            recovery: vec![
                "oz release view --latest".to_string(),
                "oz release view <TAG>".to_string(),
            ],
        });
    }

    let (client, remote) = GitHubClient::for_repo(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let (owner, repo_name) = remote.require_owner_repo()?;

    let rel = if let Some(ref tag_name) = tag {
        resolve_release_by_tag_with_draft_fallback(&client, owner, repo_name, tag_name)?
    } else {
        match client.get_latest_release(owner, repo_name) {
            Ok(rel) => rel,
            Err(err) if is_not_found_error(&err) => {
                return Err(OzError::CheckFailed {
                    message: format!(
                        "no latest published release found in {owner}/{repo_name} (drafts and prereleases are excluded from `/releases/latest`)"
                    ),
                    recovery: vec![
                        "oz release list".to_string(),
                        "oz release view <TAG>".to_string(),
                    ],
                });
            }
            Err(err) => return Err(err),
        }
    };

    Ok(ReleaseViewOutcome {
        owner: owner.to_string(),
        repo: repo_name.to_string(),
        id: rel.id,
        tag_name: rel.tag_name,
        name: rel.name.filter(|n| !n.trim().is_empty()),
        html_url: rel.html_url,
        draft: rel.draft,
        prerelease: rel.prerelease,
        target_commitish: rel.target_commitish,
        author: rel.author.map(|u| u.login),
        created_at: rel.created_at,
        published_at: rel.published_at,
        body: rel.body,
        assets: rel
            .assets
            .into_iter()
            .map(ReleaseAssetSummary::from)
            .collect(),
    })
}

/// Creates a new GitHub Release (`oz release create <TAG>`).
pub fn release_create(
    repo: &OzoneRepo,
    opts: &ReleaseCreateOptions,
) -> Result<ReleaseCreateOutcome> {
    let tag = opts.tag.trim();
    if tag.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "release tag cannot be empty".to_string(),
            recovery: vec!["oz release create v1.0.0 --generate-notes".to_string()],
        });
    }

    let (client, remote) = GitHubClient::for_repo(
        repo,
        opts.remote.as_deref(),
        opts.auth_token.as_deref(),
        opts.api_url.as_deref(),
    )?;
    let _ = client.require_authenticated()?;
    let (owner, repo_name) = remote.require_owner_repo()?;

    let payload = CreateReleasePayload {
        tag_name: tag.to_string(),
        target_commitish: non_empty_trimmed(opts.target.as_deref()),
        name: opts.title.clone(),
        body: opts.notes.clone(),
        draft: Some(opts.draft),
        prerelease: Some(opts.prerelease),
        generate_release_notes: opts.generate_notes.then_some(true),
        make_latest: opts.latest.then(|| "true".to_string()),
    };

    let created = client.create_release(owner, repo_name, &payload)?;

    Ok(ReleaseCreateOutcome {
        owner: owner.to_string(),
        repo: repo_name.to_string(),
        id: created.id,
        tag_name: created.tag_name,
        name: created.name.filter(|n| !n.trim().is_empty()),
        html_url: created.html_url,
        draft: created.draft,
        prerelease: created.prerelease,
        target_commitish: created.target_commitish,
        author: created.author.map(|u| u.login),
        created_at: created.created_at,
        published_at: created.published_at,
        body: created.body,
        assets: created
            .assets
            .into_iter()
            .map(ReleaseAssetSummary::from)
            .collect(),
    })
}

/// Updates an existing GitHub Release by tag (`oz release edit <TAG>`), falling back to
/// listing releases when `GET /releases/tags/{tag}` returns 404 (e.g. for draft releases).
pub fn release_edit(repo: &OzoneRepo, opts: &ReleaseEditOptions) -> Result<ReleaseEditOutcome> {
    let tag = opts.tag.trim();
    if tag.is_empty() {
        return Err(OzError::InvalidUsage {
            message: "release tag cannot be empty".to_string(),
            recovery: vec!["oz release edit v1.0.0 --title \"...\"".to_string()],
        });
    }

    let new_tag = non_empty_trimmed(opts.new_tag.as_deref());
    let target = non_empty_trimmed(opts.target.as_deref());

    if new_tag.is_none()
        && opts.title.is_none()
        && opts.notes.is_none()
        && opts.draft.is_none()
        && opts.prerelease.is_none()
        && !opts.latest
        && target.is_none()
    {
        return Err(OzError::InvalidUsage {
            message: "no fields specified to edit; pass at least one of --tag, --title, --notes, --notes-file, --draft, --publish, --prerelease, --no-prerelease, --latest, or --target".to_string(),
            recovery: vec![
                format!("oz release edit {tag} --title \"Updated title\""),
                format!("oz release edit {tag} --publish"),
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
    let (owner, repo_name) = remote.require_owner_repo()?;

    let existing = resolve_release_by_tag_with_draft_fallback(&client, owner, repo_name, tag)?;

    let mut updated_fields = Vec::new();
    let mut patch = UpdateReleasePayload::default();

    if let Some(nt) = new_tag {
        patch.tag_name = Some(nt);
        updated_fields.push("tag".to_string());
    }
    if let Some(ref title) = opts.title {
        patch.name = Some(title.clone());
        updated_fields.push("title".to_string());
    }
    if let Some(ref notes) = opts.notes {
        patch.body = Some(notes.clone());
        updated_fields.push("notes".to_string());
    }
    if let Some(draft_val) = opts.draft {
        patch.draft = Some(draft_val);
        updated_fields.push(if draft_val { "draft" } else { "publish" }.to_string());
    }
    if let Some(pre_val) = opts.prerelease {
        patch.prerelease = Some(pre_val);
        updated_fields.push(
            if pre_val {
                "prerelease"
            } else {
                "no_prerelease"
            }
            .to_string(),
        );
    }
    if opts.latest {
        patch.make_latest = Some("true".to_string());
        updated_fields.push("latest".to_string());
    }
    if let Some(tgt) = target {
        patch.target_commitish = Some(tgt);
        updated_fields.push("target".to_string());
    }

    let updated = client.update_release(owner, repo_name, existing.id, &patch)?;

    Ok(ReleaseEditOutcome {
        owner: owner.to_string(),
        repo: repo_name.to_string(),
        id: updated.id,
        tag_name: updated.tag_name,
        name: updated.name.filter(|n| !n.trim().is_empty()),
        html_url: updated.html_url,
        draft: updated.draft,
        prerelease: updated.prerelease,
        target_commitish: updated.target_commitish,
        author: updated.author.map(|u| u.login),
        created_at: updated.created_at,
        published_at: updated.published_at,
        body: updated.body,
        assets: updated
            .assets
            .into_iter()
            .map(ReleaseAssetSummary::from)
            .collect(),
        updated_fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::GitHubUser;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    fn init_repo_with_github_remote() -> (TempDir, OzoneRepo) {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        let st = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .status()
            .expect("git init");
        assert!(st.success());
        let st2 = std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/acme/demo.git",
            ])
            .current_dir(&dir)
            .status()
            .expect("git remote add");
        assert!(st2.success());
        let repo = OzoneRepo::discover(Some(&dir)).expect("discover");
        (tmp, repo)
    }

    fn spawn_mock_release_server() -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            let rel_latest = GitHubRelease {
                id: 101,
                tag_name: "v1.4.5".to_string(),
                target_commitish: Some("main".to_string()),
                name: Some("Ozone v1.4.5".to_string()),
                body: Some("## Highlights\n- Added `oz release`".to_string()),
                draft: false,
                prerelease: false,
                html_url: "https://github.com/acme/demo/releases/tag/v1.4.5".to_string(),
                author: Some(GitHubUser {
                    login: "dev".to_string(),
                }),
                created_at: Some("2026-11-28T10:00:00Z".to_string()),
                published_at: Some("2026-11-28T12:00:00Z".to_string()),
                assets: vec![GitHubReleaseAsset {
                    id: 501,
                    name: "oz-linux-x86_64.tar.gz".to_string(),
                    content_type: "application/gzip".to_string(),
                    size: 1_572_864,
                    download_count: 17,
                    browser_download_url:
                        "https://github.com/acme/demo/releases/download/v1.4.5/oz-linux-x86_64.tar.gz"
                            .to_string(),
                    ..Default::default()
                }],
            };
            let rel_draft = GitHubRelease {
                id: 102,
                tag_name: "v1.4.6-draft".to_string(),
                target_commitish: Some("main".to_string()),
                name: Some("Draft 1.4.6".to_string()),
                body: Some("Draft notes".to_string()),
                draft: true,
                prerelease: false,
                html_url: "https://github.com/acme/demo/releases/tag/untagged-draft".to_string(),
                author: Some(GitHubUser {
                    login: "dev".to_string(),
                }),
                created_at: Some("2026-11-28T13:00:00Z".to_string()),
                published_at: None,
                assets: Vec::new(),
            };
            let rel_pre = GitHubRelease {
                id: 103,
                tag_name: "pkg/v1.5.0-rc.1".to_string(),
                target_commitish: Some("next".to_string()),
                name: Some("RC 1".to_string()),
                body: Some("RC notes".to_string()),
                draft: false,
                prerelease: true,
                html_url: "https://github.com/acme/demo/releases/tag/pkg%2Fv1.5.0-rc.1".to_string(),
                author: Some(GitHubUser {
                    login: "dev".to_string(),
                }),
                created_at: Some("2026-11-27T09:00:00Z".to_string()),
                published_at: Some("2026-11-27T09:30:00Z".to_string()),
                assets: Vec::new(),
            };

            while !stop_bg.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let _ = stream.set_nonblocking(false);
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                    continue;
                }
                let parts: Vec<&str> = req_line.split_whitespace().collect();
                let method = parts.first().copied().unwrap_or("GET");
                let target = parts.get(1).copied().unwrap_or("/");
                let (path, _query) = target.split_once('?').unwrap_or((target, ""));

                let mut content_len = 0usize;
                loop {
                    let mut hdr = String::new();
                    if reader.read_line(&mut hdr).unwrap_or(0) == 0 || hdr == "\r\n" || hdr == "\n"
                    {
                        break;
                    }
                    if let Some((k, v)) = hdr.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body_bytes = vec![0u8; content_len];
                if content_len > 0 {
                    let _ = reader.read_exact(&mut body_bytes);
                }
                let body_str = String::from_utf8_lossy(&body_bytes);

                let (status, resp_body): (&str, Vec<u8>) = match (method, path) {
                    ("GET", "/repos/acme/demo/releases") => (
                        "200 OK",
                        serde_json::to_vec(&vec![
                            rel_draft.clone(),
                            rel_latest.clone(),
                            rel_pre.clone(),
                        ])
                        .unwrap(),
                    ),
                    ("GET", "/repos/acme/demo/releases/latest") => {
                        ("200 OK", serde_json::to_vec(&rel_latest).unwrap())
                    }
                    ("GET", "/repos/acme/demo/releases/tags/v1.4.5") => {
                        ("200 OK", serde_json::to_vec(&rel_latest).unwrap())
                    }
                    ("GET", "/repos/acme/demo/releases/tags/pkg%2Fv1.5.0-rc.1") => {
                        ("200 OK", serde_json::to_vec(&rel_pre).unwrap())
                    }
                    ("POST", "/repos/acme/demo/releases") => {
                        let req: serde_json::Value =
                            serde_json::from_str(&body_str).unwrap_or_default();
                        let tag = req["tag_name"].as_str().unwrap_or("v0.0.0").to_string();
                        let created = GitHubRelease {
                            id: 200,
                            tag_name: tag.clone(),
                            target_commitish: req["target_commitish"].as_str().map(str::to_string),
                            name: req["name"].as_str().map(str::to_string),
                            body: req["body"].as_str().map(str::to_string),
                            draft: req["draft"].as_bool().unwrap_or(false),
                            prerelease: req["prerelease"].as_bool().unwrap_or(false),
                            html_url: format!("https://github.com/acme/demo/releases/tag/{tag}"),
                            author: Some(GitHubUser {
                                login: "dev".to_string(),
                            }),
                            created_at: Some("2026-11-28T14:00:00Z".to_string()),
                            published_at: None,
                            assets: Vec::new(),
                        };
                        ("201 Created", serde_json::to_vec(&created).unwrap())
                    }
                    ("PATCH", "/repos/acme/demo/releases/102") => {
                        let req: serde_json::Value =
                            serde_json::from_str(&body_str).unwrap_or_default();
                        let mut updated = rel_draft.clone();
                        if let Some(t) = req["tag_name"].as_str() {
                            updated.tag_name = t.to_string();
                        }
                        if let Some(n) = req["name"].as_str() {
                            updated.name = Some(n.to_string());
                        }
                        if let Some(b) = req["body"].as_str() {
                            updated.body = Some(b.to_string());
                        }
                        if let Some(d) = req["draft"].as_bool() {
                            updated.draft = d;
                        }
                        if let Some(p) = req["prerelease"].as_bool() {
                            updated.prerelease = p;
                        }
                        updated.html_url = format!(
                            "https://github.com/acme/demo/releases/tag/{}",
                            updated.tag_name
                        );
                        ("200 OK", serde_json::to_vec(&updated).unwrap())
                    }
                    _ => ("404 Not Found", b"{\"message\":\"Not Found\"}".to_vec()),
                };

                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    resp_body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&resp_body);
                let _ = stream.flush();
            }
        });

        (format!("http://{addr}"), stop, handle)
    }

    #[test]
    fn release_list_view_create_and_draft_edit_fallback() {
        let (_tmp, repo) = init_repo_with_github_remote();
        let (api_url, stop, server) = spawn_mock_release_server();

        // 1. List with and without filters
        let all = release_list(
            &repo,
            &ReleaseListOptions {
                limit: 10,
                api_url: Some(api_url.clone()),
                auth_token: Some("tok".to_string()),
                ..Default::default()
            },
        )
        .expect("list all");
        assert_eq!(all.total_returned, 3);
        assert!(all.releases[0].draft);
        assert!(!all.releases[0].is_latest);
        assert!(all.releases[1].is_latest);
        assert_eq!(all.releases[1].tag_name, "v1.4.5");
        assert_eq!(all.releases[1].asset_count, 1);

        let filtered = release_list(
            &repo,
            &ReleaseListOptions {
                limit: 10,
                exclude_drafts: true,
                exclude_prereleases: true,
                api_url: Some(api_url.clone()),
                auth_token: Some("tok".to_string()),
                ..Default::default()
            },
        )
        .expect("list filtered");
        assert_eq!(filtered.total_returned, 1);
        assert_eq!(filtered.releases[0].tag_name, "v1.4.5");

        // 2. View latest & URL-encoded tag
        let latest = release_view(
            &repo,
            &ReleaseViewOptions {
                latest: true,
                api_url: Some(api_url.clone()),
                auth_token: Some("tok".to_string()),
                ..Default::default()
            },
        )
        .expect("view latest");
        assert_eq!(latest.id, 101);
        assert_eq!(latest.assets.len(), 1);
        assert_eq!(latest.assets[0].name, "oz-linux-x86_64.tar.gz");

        let encoded = release_view(
            &repo,
            &ReleaseViewOptions {
                tag: Some("pkg/v1.5.0-rc.1".to_string()),
                api_url: Some(api_url.clone()),
                auth_token: Some("tok".to_string()),
                ..Default::default()
            },
        )
        .expect("view encoded tag");
        assert_eq!(encoded.id, 103);
        assert!(encoded.prerelease);

        // 3. Create release
        let created = release_create(
            &repo,
            &ReleaseCreateOptions {
                tag: "v1.5.0".to_string(),
                title: Some("Ozone v1.5.0".to_string()),
                notes: Some("Release body".to_string()),
                generate_notes: true,
                draft: true,
                latest: true,
                api_url: Some(api_url.clone()),
                auth_token: Some("tok".to_string()),
                ..Default::default()
            },
        )
        .expect("create");
        assert_eq!(created.id, 200);
        assert_eq!(created.tag_name, "v1.5.0");
        assert!(created.draft);

        // 4. Edit draft release via 404 -> list fallback -> PATCH /releases/102
        let edited = release_edit(
            &repo,
            &ReleaseEditOptions {
                tag: "v1.4.6-draft".to_string(),
                new_tag: Some("v1.4.6".to_string()),
                title: Some("Ozone v1.4.6".to_string()),
                draft: Some(false),
                api_url: Some(api_url),
                auth_token: Some("tok".to_string()),
                ..Default::default()
            },
        )
        .expect("edit draft");
        assert_eq!(edited.id, 102);
        assert_eq!(edited.tag_name, "v1.4.6");
        assert!(!edited.draft);
        assert_eq!(edited.updated_fields, vec!["tag", "title", "publish"]);

        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();
    }
}
