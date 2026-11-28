// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! CLI definitions, handlers, and compact human / `oz.v1` JSON rendering for `oz release`.

use crate::commands::print_json_envelope;
use crate::recovery::discover_and_recover_repo;
use clap::{Args, Subcommand};
use oz_core::{Envelope, ExitCode, InteractionMode, OzError, Result};
use oz_github::{
    ReleaseCreateOptions, ReleaseCreateOutcome, ReleaseEditOptions, ReleaseEditOutcome,
    ReleaseListOptions, ReleaseListOutcome, ReleaseViewOptions, ReleaseViewOutcome, release_create,
    release_edit, release_list, release_view,
};
use serde::Serialize;
use std::path::Path;

/// Manage and inspect GitHub Releases (`list`, `view`, `create`, `edit`).
#[derive(Debug, Subcommand)]
pub enum ReleaseCommand {
    /// List GitHub Releases (`--limit`, `--exclude-drafts`, `--exclude-prereleases`).
    List(ReleaseListArgs),
    /// Inspect a GitHub Release by tag or `--latest` (`--no-body`, `--no-assets`).
    View(ReleaseViewArgs),
    /// Create a GitHub Release (`--title`, `--notes`, `--notes-file`, `--generate-notes`, `--draft`, `--prerelease`, `--latest`).
    Create(ReleaseCreateArgs),
    /// Edit an existing GitHub Release (supports draft releases; `--title`, `--notes`, `--publish`, `--draft`, `--prerelease`, `--latest`).
    Edit(ReleaseEditArgs),
}

#[derive(Debug, Args)]
pub struct ReleaseListArgs {
    /// Maximum number of releases to return (default: 20).
    #[arg(short = 'L', long = "limit", default_value_t = 20, value_name = "N")]
    pub limit: usize,
    /// Exclude draft releases from the output.
    #[arg(long = "exclude-drafts")]
    pub exclude_drafts: bool,
    /// Exclude prereleases from the output.
    #[arg(long = "exclude-prereleases", alias = "exclude-pre-releases")]
    pub exclude_prereleases: bool,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct ReleaseViewArgs {
    /// Release tag to inspect (omit or pass `--latest` for the latest published release).
    #[arg(value_name = "TAG", conflicts_with = "latest")]
    pub tag: Option<String>,
    /// Fetch the latest published release (`/releases/latest`).
    #[arg(long = "latest", conflicts_with = "tag")]
    pub latest: bool,
    /// Omit the release notes body from output.
    #[arg(long = "no-body")]
    pub no_body: bool,
    /// Omit attached release assets from output.
    #[arg(long = "no-assets")]
    pub no_assets: bool,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct ReleaseCreateArgs {
    /// Git tag name for the new release (e.g. `v1.4.5`).
    #[arg(value_name = "TAG")]
    pub tag: String,
    /// Release title.
    #[arg(short = 't', long = "title", value_name = "TITLE")]
    pub title: Option<String>,
    /// Release notes markdown text.
    #[arg(
        short = 'n',
        long = "notes",
        visible_alias = "body",
        value_name = "NOTES",
        conflicts_with = "notes_file"
    )]
    pub notes: Option<String>,
    /// Read release notes from a file path or `-` for stdin.
    #[arg(
        short = 'F',
        long = "notes-file",
        visible_alias = "body-file",
        value_name = "PATH",
        conflicts_with = "notes"
    )]
    pub notes_file: Option<String>,
    /// Ask GitHub to auto-generate release notes (`generate_release_notes: true`).
    #[arg(long = "generate-notes")]
    pub generate_notes: bool,
    /// Target branch or commit SHA for the release tag (`target_commitish`).
    #[arg(long = "target", value_name = "REV")]
    pub target: Option<String>,
    /// Create the release as a draft (`draft: true`).
    #[arg(short = 'd', long = "draft")]
    pub draft: bool,
    /// Mark the release as a prerelease (`prerelease: true`).
    #[arg(short = 'p', long = "prerelease")]
    pub prerelease: bool,
    /// Explicitly mark this release as the repository's latest release (`make_latest: "true"`).
    #[arg(long = "latest")]
    pub latest: bool,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

#[derive(Debug, Args)]
pub struct ReleaseEditArgs {
    /// Existing release tag to update.
    #[arg(value_name = "TAG")]
    pub tag: String,
    /// Rename the release's tag (`tag_name`).
    #[arg(long = "tag", value_name = "NEW_TAG")]
    pub new_tag: Option<String>,
    /// Updated release title.
    #[arg(short = 't', long = "title", value_name = "TITLE")]
    pub title: Option<String>,
    /// Updated release notes markdown text.
    #[arg(
        short = 'n',
        long = "notes",
        visible_alias = "body",
        value_name = "NOTES",
        conflicts_with = "notes_file"
    )]
    pub notes: Option<String>,
    /// Read updated release notes from a file path or `-` for stdin.
    #[arg(
        short = 'F',
        long = "notes-file",
        visible_alias = "body-file",
        value_name = "PATH",
        conflicts_with = "notes"
    )]
    pub notes_file: Option<String>,
    /// Convert the release to a draft (`draft: true`).
    #[arg(short = 'd', long = "draft", conflicts_with = "publish")]
    pub draft: bool,
    /// Publish a draft release (`draft: false`).
    #[arg(long = "publish", conflicts_with = "draft")]
    pub publish: bool,
    /// Mark the release as a prerelease (`prerelease: true`).
    #[arg(short = 'p', long = "prerelease", conflicts_with = "no_prerelease")]
    pub prerelease: bool,
    /// Mark the release as a full release (`prerelease: false`).
    #[arg(long = "no-prerelease", conflicts_with = "prerelease")]
    pub no_prerelease: bool,
    /// Explicitly promote this release to latest (`make_latest: "true"`).
    #[arg(long = "latest")]
    pub latest: bool,
    /// Updated target branch or commit SHA (`target_commitish`).
    #[arg(long = "target", value_name = "REV")]
    pub target: Option<String>,
    /// Remote name (defaults to `origin`).
    #[arg(long = "remote", value_name = "REMOTE")]
    pub remote: Option<String>,
}

fn read_notes_or_file(notes: Option<&str>, notes_file: Option<&str>) -> Result<Option<String>> {
    if let Some(file_spec) = notes_file {
        let text = if file_spec == "-" {
            use std::io::Read as _;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|err| OzError::Other(format!("failed reading notes from stdin: {err}")))?;
            buf
        } else {
            std::fs::read_to_string(file_spec).map_err(|err| {
                OzError::Other(format!("failed reading notes file '{file_spec}': {err}"))
            })?
        };
        return Ok(Some(text));
    }
    Ok(notes.map(str::to_string))
}

fn render_release_result<T: Serialize, F>(
    mode: InteractionMode,
    res: Result<Envelope<T>>,
    human_render: F,
) -> ExitCode
where
    F: FnOnce(&Envelope<T>),
{
    match res {
        Ok(env) => {
            if mode.json {
                print_json_envelope(&env);
            } else {
                human_render(&env);
            }
            ExitCode::Success
        }
        Err(err) => {
            if mode.json {
                let env = Envelope::from_error(&err);
                print_json_envelope(&env);
            } else {
                eprintln!("error: {err}");
                for cmd in err.recovery_commands() {
                    eprintln!("hint: {cmd}");
                }
            }
            err.exit_code()
        }
    }
}

fn release_status_label(draft: bool, prerelease: bool) -> &'static str {
    match (draft, prerelease) {
        (true, true) => "DRAFT, PRERELEASE",
        (true, false) => "DRAFT",
        (false, true) => "PRERELEASE",
        (false, false) => "PUBLISHED",
    }
}

fn format_compact_date(ts: Option<&str>) -> &str {
    match ts.map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) if raw.len() >= 10 && raw.is_char_boundary(10) => &raw[..10],
        Some(raw) => raw,
        None => "-",
    }
}

fn format_human_bytes(bytes: u64) -> String {
    const KIB: u128 = 1024;
    const MIB: u128 = 1024 * 1024;
    const GIB: u128 = 1024 * 1024 * 1024;
    let b = u128::from(bytes);
    if b < KIB {
        format!("{bytes} B")
    } else if b < MIB {
        let tenths = (b * 10 + KIB / 2) / KIB;
        format!("{}.{} KiB", tenths / 10, tenths % 10)
    } else if b < GIB {
        let tenths = (b * 10 + MIB / 2) / MIB;
        format!("{}.{} MiB", tenths / 10, tenths % 10)
    } else {
        let tenths = (b * 10 + GIB / 2) / GIB;
        format!("{}.{} GiB", tenths / 10, tenths % 10)
    }
}

pub(crate) fn handle_release(
    cmd: ReleaseCommand,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    match cmd {
        ReleaseCommand::List(args) => handle_release_list(&args, directory, mode),
        ReleaseCommand::View(args) => handle_release_view(&args, directory, mode),
        ReleaseCommand::Create(args) => handle_release_create(&args, directory, mode),
        ReleaseCommand::Edit(args) => handle_release_edit(&args, directory, mode),
    }
}

fn handle_release_list(
    args: &ReleaseListArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<ReleaseListOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let outcome = release_list(
            &repo,
            &ReleaseListOptions {
                limit: args.limit,
                exclude_drafts: args.exclude_drafts,
                exclude_prereleases: args.exclude_prereleases,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_release_result(mode, res, |env| {
        let d = &env.data;
        if d.releases.is_empty() {
            println!("No releases found in {}/{}", d.owner, d.repo);
            return;
        }
        println!(
            "Releases in {}/{} ({} shown):",
            d.owner, d.repo, d.total_returned
        );
        for item in &d.releases {
            let mut parts: Vec<String> = vec![item.tag_name.clone()];
            if item.is_latest {
                parts.push("[latest]".to_string());
            }
            if item.draft {
                parts.push("[draft]".to_string());
            }
            if item.prerelease {
                parts.push("[prerelease]".to_string());
            }
            if let Some(title) = item
                .name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != item.tag_name)
            {
                parts.push(title.to_string());
            }
            let date =
                format_compact_date(item.published_at.as_deref().or(item.created_at.as_deref()));
            parts.push(date.to_string());
            let assets_label = if item.asset_count == 1 {
                "1 asset".to_string()
            } else {
                format!("{} assets", item.asset_count)
            };
            parts.push(assets_label);
            if !item.html_url.is_empty() {
                parts.push(item.html_url.clone());
            }
            println!("  {}", parts.join("  "));
        }
    })
}

fn handle_release_view(
    args: &ReleaseViewArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<ReleaseViewOutcome>> {
        let repo = discover_and_recover_repo(directory, false)?;
        let mut outcome = release_view(
            &repo,
            &ReleaseViewOptions {
                tag: args.tag.clone(),
                latest: args.latest,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        if args.no_body {
            outcome.body = None;
        }
        if args.no_assets {
            outcome.assets.clear();
        }
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_release_result(mode, res, |env| {
        let d = &env.data;
        let status = release_status_label(d.draft, d.prerelease);
        let title_suffix = d
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != d.tag_name)
            .map_or_default(|t| format!(" {t}"));
        println!("Release {} [{status}]{title_suffix}", d.tag_name);

        let mut meta = Vec::new();
        if let Some(ref author) = d.author {
            meta.push(format!("author={author}"));
        }
        if let Some(ref target) = d.target_commitish
            && !target.is_empty()
        {
            meta.push(format!("target={target}"));
        }
        if let Some(ref published) = d.published_at {
            meta.push(format!("published={published}"));
        } else if let Some(ref created) = d.created_at {
            meta.push(format!("created={created}"));
        }
        if !args.no_assets {
            meta.push(format!("assets={}", d.assets.len()));
        }
        if !meta.is_empty() {
            println!("  {}", meta.join("  "));
        }
        if !d.html_url.is_empty() {
            println!("  url={}", d.html_url);
        }

        if !d.assets.is_empty() {
            println!("\nAssets ({}):", d.assets.len());
            for asset in &d.assets {
                let dl_label = if asset.download_count == 1 {
                    "1 download".to_string()
                } else {
                    format!("{} downloads", asset.download_count)
                };
                println!(
                    "  {} ({}, {dl_label}) -> {}",
                    asset.name,
                    format_human_bytes(asset.size),
                    asset.browser_download_url
                );
            }
        }

        if let Some(ref body) = d.body
            && !body.trim().is_empty()
        {
            println!("\n{}", body.trim());
        }
    })
}

fn handle_release_create(
    args: &ReleaseCreateArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<ReleaseCreateOutcome>> {
        let notes = read_notes_or_file(args.notes.as_deref(), args.notes_file.as_deref())?;
        let repo = discover_and_recover_repo(directory, false)?;
        let outcome = release_create(
            &repo,
            &ReleaseCreateOptions {
                tag: args.tag.clone(),
                title: args.title.clone(),
                notes,
                generate_notes: args.generate_notes,
                target: args.target.clone(),
                draft: args.draft,
                prerelease: args.prerelease,
                latest: args.latest,
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_release_result(mode, res, |env| {
        let d = &env.data;
        let status = release_status_label(d.draft, d.prerelease);
        let title_suffix = d
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != d.tag_name)
            .map_or_default(|t| format!(" {t}"));
        let url_suffix = if d.html_url.is_empty() {
            String::new()
        } else {
            format!(" -> {}", d.html_url)
        };
        println!(
            "Created release {} [{status}]{title_suffix}{url_suffix}",
            d.tag_name
        );
    })
}

fn handle_release_edit(
    args: &ReleaseEditArgs,
    directory: Option<&Path>,
    mode: InteractionMode,
) -> ExitCode {
    let res = (|| -> Result<Envelope<ReleaseEditOutcome>> {
        let notes = read_notes_or_file(args.notes.as_deref(), args.notes_file.as_deref())?;
        let draft = if args.draft {
            Some(true)
        } else if args.publish {
            Some(false)
        } else {
            None
        };
        let prerelease = if args.prerelease {
            Some(true)
        } else if args.no_prerelease {
            Some(false)
        } else {
            None
        };
        let repo = discover_and_recover_repo(directory, false)?;
        let outcome = release_edit(
            &repo,
            &ReleaseEditOptions {
                tag: args.tag.clone(),
                new_tag: args.new_tag.clone(),
                title: args.title.clone(),
                notes,
                draft,
                prerelease,
                latest: args.latest,
                target: args.target.clone(),
                remote: args.remote.clone(),
                auth_token: None,
                api_url: None,
            },
        )?;
        Ok(Envelope::success(outcome)
            .with_workspace(repo.workspace)
            .with_branch(repo.branch)
            .with_head_oid(repo.head_oid))
    })();

    render_release_result(mode, res, |env| {
        let d = &env.data;
        let status = release_status_label(d.draft, d.prerelease);
        let fields_suffix = if d.updated_fields.is_empty() {
            String::new()
        } else {
            format!(" ({})", d.updated_fields.join(", "))
        };
        let url_suffix = if d.html_url.is_empty() {
            String::new()
        } else {
            format!(" -> {}", d.html_url)
        };
        println!(
            "Updated release {} [{status}]{fields_suffix}{url_suffix}",
            d.tag_name
        );
    })
}
