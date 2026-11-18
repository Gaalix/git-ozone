// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `oz ci` check-run & GitHub Actions job log inspector, stripping ANSI codes, ISO-8601
//! runner timestamps, and boilerplate noise to produce low-token error excerpts for agent
//! self-healing loops. Supports both plain-text logs and PKZIP job log archives.

use crate::CiCheckSummary;
use crate::api::{GitHubActionsJob, GitHubCheckRun, GitHubClient};
use crate::pr::normalize_short_branch;
use crate::wire::{open_gix_unrestricted, read_ref_oid};
use flate2::read::DeflateDecoder;
use oz_core::{OzError, OzoneRepo, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;

/// Default maximum lines included in each failing check's `log_excerpt`.
pub const DEFAULT_MAX_EXCERPT_LINES: usize = 40;
const MAX_EXCERPT_CHARS: usize = 4096;

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
            auth_token: None,
            api_url: None,
        }
    }
}

/// Complete CI health & failure excerpt report returned by [`query_ci_status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiReport {
    pub ref_name: String,
    pub commit_sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    /// Aggregate state: `"passing"`, `"failing"`, `"pending"`, or `"no_checks"`.
    pub overall_status: String,
    pub all_passed: bool,
    pub total_count: usize,
    pub failing_count: usize,
    pub pending_count: usize,
    pub checks: Vec<CiCheckSummary>,
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

    let (ref_name, commit_sha, resolved_pr_num) =
        resolve_ci_target(repo, &client, owner, gh_repo, opts)?;

    let query_ref = if !commit_sha.is_empty() {
        commit_sha.as_str()
    } else {
        ref_name.as_str()
    };

    let check_runs = client
        .list_check_runs_for_ref(owner, gh_repo, query_ref)
        .unwrap_or_default();

    // Also fetch Actions workflow runs + jobs for `commit_sha` / `ref_name` so step failure info
    // and job IDs are available even when check-runs omits step details.
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

    let mut summaries: Vec<CiCheckSummary> = Vec::new();
    let mut covered_job_ids: HashSet<u64> = HashSet::new();

    for cr in &check_runs {
        let matched_job = extract_actions_job_id_from_url(cr.details_url.as_deref())
            .or_else(|| extract_actions_job_id_from_url(cr.html_url.as_deref()))
            .and_then(|jid| {
                covered_job_ids.insert(jid);
                jobs_by_id
                    .get(&jid)
                    .cloned()
                    .or_else(|| client.get_actions_job(owner, gh_repo, jid).ok())
            })
            .or_else(|| {
                let j = jobs_by_name.get(&cr.name)?.clone();
                covered_job_ids.insert(j.id);
                Some(j)
            });

        summaries.push(build_check_summary(
            &client,
            owner,
            gh_repo,
            Some(cr),
            matched_job.as_ref(),
            max_lines,
        ));
    }

    // Append any Actions jobs not already represented in `check_runs`.
    let mut extra_jobs: Vec<&GitHubActionsJob> = jobs_by_id
        .values()
        .filter(|j| !covered_job_ids.contains(&j.id))
        .collect();
    extra_jobs.sort_by_key(|j| j.id);
    for job in extra_jobs {
        summaries.push(build_check_summary(
            &client,
            owner,
            gh_repo,
            None,
            Some(job),
            max_lines,
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

    Ok(CiReport {
        ref_name,
        commit_sha,
        pr_number: resolved_pr_num,
        overall_status,
        all_passed,
        total_count,
        failing_count,
        pending_count,
        checks: summaries,
    })
}

fn resolve_ci_target(
    repo: &OzoneRepo,
    client: &GitHubClient,
    owner: &str,
    gh_repo: &str,
    opts: &CiQueryOptions,
) -> Result<(String, String, Option<u64>)> {
    if let Some(pr_num) = opts.pr_number {
        let pr = client.get_pull_request(owner, gh_repo, pr_num)?;
        return Ok((pr.head.ref_name, pr.head.sha, Some(pr_num)));
    }

    let gix_repo = open_gix_unrestricted(&repo.repo_root)?;
    if let Some(ref explicit) = opts.target_ref {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            let short = normalize_short_branch(trimmed);
            if let Some(oid) = read_ref_oid(&gix_repo, &format!("refs/heads/{short}"))? {
                return Ok((short, oid.to_hex().to_string(), None));
            }
            if let Ok(oid) = gix_repo.rev_parse_single(trimmed) {
                return Ok((trimmed.to_string(), oid.detach().to_hex().to_string(), None));
            }
            return Ok((trimmed.to_string(), String::new(), None));
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
            .map(|p| p.number)
    } else {
        None
    };

    Ok((branch, sha, matched_pr))
}

fn build_check_summary(
    client: &GitHubClient,
    owner: &str,
    gh_repo: &str,
    check_run: Option<&GitHubCheckRun>,
    job: Option<&GitHubActionsJob>,
    max_lines: usize,
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

    if !is_failing_conclusion(conclusion.as_deref()) {
        return CiCheckSummary {
            name,
            status,
            conclusion,
            failing_step: None,
            log_excerpt: None,
        };
    }

    let step_from_job = job.and_then(|j| {
        j.steps
            .iter()
            .find(|st| is_failing_conclusion(st.conclusion.as_deref()))
            .map(|st| st.name.clone())
    });

    let job_id_opt = job
        .map(|j| j.id)
        .or_else(|| check_run.and_then(|c| extract_actions_job_id_from_url(c.details_url.as_deref())))
        .or_else(|| check_run.and_then(|c| extract_actions_job_id_from_url(c.html_url.as_deref())));

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

    CiCheckSummary {
        name,
        status,
        conclusion,
        failing_step: step_from_job.or(parsed_step),
        log_excerpt: excerpt,
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
    // Matches `.../actions/runs/<run_id>/job/<job_id>` or `.../runs/<job_id>`
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

    let mut excerpt = excerpt_lines.join("\n").trim().to_string();
    if excerpt.len() > MAX_EXCERPT_CHARS {
        let mut cut = MAX_EXCERPT_CHARS;
        while !excerpt.is_char_boundary(cut) {
            cut -= 1;
        }
        excerpt.truncate(cut);
        excerpt.push_str("\n... [truncated]");
    }

    let step = known_step.map(str::to_string).or(inferred_step);
    (step, (!excerpt.is_empty()).then_some(excerpt))
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
}
