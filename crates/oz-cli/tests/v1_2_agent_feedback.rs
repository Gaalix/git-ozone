// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end tests covering long-running agent session feedback:
//! - `oz pr comments <n> [--unresolved]` & `oz pr view <n> --comments` (reviews + inline threads + GraphQL resolved/outdated + conversation comments)
//! - `oz issue view <n|url> [--comments]` (REST-only, avoids deprecated classic `projectCards` GraphQL)
//! - `oz ci --pr 3320,3321 --json` (multi-PR CI snapshot) & `oz ci --workflow <file|name> [--watch]`
//! - `oz merge <rev>` (`--no-ff`, `--ff-only`, linked worktree sync, and `oz undo` reversibility)
//! - `oz push origin b1 b2 b3` (multi-refspec push without checking out each branch)
//! - `oz branch -a` / `-r` (local + remote-tracking branch SHAs)
//! - `oz grep <pattern> [<rev>] [-- <paths...>]` (rev-scoped tree grep)

#![allow(clippy::too_many_lines)]

use clap::Parser;
use oz_cli::Cli;
use oz_github::{
    GitHubActionsJob, GitHubCheckRun, GitHubCheckRunOutput, GitHubIssue, GitHubIssueComment,
    GitHubJobStep, GitHubLabel, GitHubPrRef, GitHubPullRequest, GitHubPullRequestComment,
    GitHubPullRequestFile, GitHubPullRequestReview, GitHubUser, GitHubWorkflow, GitHubWorkflowRun,
};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn oz_bin() -> &'static str {
    env!("CARGO_BIN_EXE_oz")
}

fn run_oz(dir: &Path, args: &[&str]) -> Output {
    Command::new(oz_bin())
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("OZ_AGENT", "1")
        .output()
        .expect("run oz")
}

fn run_oz_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(oz_bin());
    cmd.arg("-C").arg(dir).args(args).env("OZ_AGENT", "1");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("run oz")
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_sample_repo() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("tvva-ui")).unwrap();
    fs::create_dir_all(repo.join("src")).unwrap();
    git_ok(&repo, &["init", "-b", "main"]);
    git_ok(&repo, &["config", "user.name", "Test Agent"]);
    git_ok(&repo, &["config", "user.email", "agent@example.com"]);
    git_ok(&repo, &["config", "core.autocrlf", "false"]);

    fs::write(
        repo.join("tvva-ui/app.ts"),
        "export const STAGE = 'consultation';\nexport function boot() { return true; }\n",
    )
    .unwrap();
    fs::write(repo.join("src/lib.rs"), "pub fn alpha() -> i32 { 1 }\n").unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "chore: initial"]);

    (tmp, repo)
}

#[test]
fn cli_parses_all_new_agent_workflow_commands() {
    assert!(
        Cli::try_parse_from(["oz", "pr", "comments", "3320", "--unresolved", "--json"]).is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "pr", "view", "3320", "--comments", "--json"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "issue", "view", "3311", "--comments", "--json"]).is_ok());
    assert!(
        Cli::try_parse_from([
            "oz",
            "issue",
            "view",
            "https://github.com/acme/demo/issues/3311"
        ])
        .is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "ci", "--pr", "3320,3321,3322,3323", "--json"]).is_ok());
    assert!(
        Cli::try_parse_from([
            "oz",
            "ci",
            "--workflow",
            "sentry-release.yaml",
            "--branch",
            "main",
            "--limit",
            "3",
            "--json"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from(["oz", "merge", "origin/main", "--no-ff", "-m", "merge main"]).is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "merge", "--ff-only", "origin/main"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "merge", "--continue"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "merge", "--abort"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "branch", "-a", "--json"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "branch", "-r", "--json"]).is_ok());
    assert!(
        Cli::try_parse_from([
            "oz",
            "grep",
            "consultation",
            "origin/main",
            "--",
            "tvva-ui/"
        ])
        .is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "push", "origin", "b1", "b2", "b3"]).is_ok());
}

#[test]
fn merge_branch_remotes_grep_and_multi_refspec_push_work_end_to_end() {
    let (tmp, repo) = init_sample_repo();
    let bare_remote = tmp.path().join("origin.git");
    git_ok(
        tmp.path(),
        &["init", "--bare", bare_remote.to_str().unwrap()],
    );
    git_ok(
        &repo,
        &["remote", "add", "origin", bare_remote.to_str().unwrap()],
    );
    git_ok(&repo, &["push", "-u", "origin", "main"]);

    // Create feature branch `feat/pr-1`, diverge from `main`, and advance `origin/main`.
    git_ok(&repo, &["checkout", "-b", "feat/pr-1"]);
    fs::write(repo.join("src/feat.rs"), "pub fn feat() -> i32 { 42 }\n").unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "feat: add feat.rs"]);
    let pre_merge_feat_oid = git_ok(&repo, &["rev-parse", "HEAD"]);

    // Advance `main` and push to `origin/main` so `origin/main` has a commit `feat/pr-1` lacks.
    git_ok(&repo, &["checkout", "main"]);
    fs::write(
        repo.join("tvva-ui/app.ts"),
        "export const STAGE = 'consultation_v2';\nexport function boot() { return true; }\n",
    )
    .unwrap();
    fs::write(repo.join("src/main_extra.rs"), "pub fn main_only() {}\n").unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "chore: advance main"]);
    let main_oid = git_ok(&repo, &["rev-parse", "HEAD"]);
    git_ok(&repo, &["push", "origin", "main"]);

    // Switch back to `feat/pr-1` and verify `oz grep` reads `origin/main` without switching branches!
    git_ok(&repo, &["checkout", "feat/pr-1"]);
    let grep_rev = run_oz(
        &repo,
        &[
            "grep",
            "consultation_v2",
            "origin/main",
            "--json",
            "--",
            "tvva-ui/",
        ],
    );
    assert!(
        grep_rev.status.success(),
        "oz grep failed: {}",
        String::from_utf8_lossy(&grep_rev.stderr)
    );
    let grep_json: Value = serde_json::from_slice(&grep_rev.stdout).unwrap();
    assert_eq!(grep_json["data"]["total_matches"], 1);
    assert_eq!(grep_json["data"]["matches"][0]["path"], "tvva-ui/app.ts");
    assert_eq!(grep_json["data"]["matches"][0]["line_number"], 1);
    assert_eq!(grep_json["data"]["commit_oid"], main_oid);

    // Local working tree on `feat/pr-1` still has old `consultation` (not `consultation_v2`).
    let grep_wt = run_oz(&repo, &["grep", "consultation_v2", "--json"]);
    assert!(grep_wt.status.success());
    let grep_wt_json: Value = serde_json::from_slice(&grep_wt.stdout).unwrap();
    assert_eq!(grep_wt_json["data"]["total_matches"], 0);

    // `oz branch -a --json` and `-r --json` expose both local and remote-tracking SHAs.
    let br_all = run_oz(&repo, &["branch", "-a", "--json"]);
    assert!(br_all.status.success());
    let br_all_json: Value = serde_json::from_slice(&br_all.stdout).unwrap();
    let branches = br_all_json["data"]["branches"].as_array().unwrap();
    assert!(
        branches
            .iter()
            .any(|b| b["name"] == "feat/pr-1" && b["is_remote"] == false)
    );
    assert!(branches.iter().any(|b| b["name"] == "origin/main"
        && b["is_remote"] == true
        && b["head_oid"] == main_oid));

    let br_rem = run_oz(&repo, &["branch", "-r", "--json"]);
    assert!(br_rem.status.success());
    let br_rem_json: Value = serde_json::from_slice(&br_rem.stdout).unwrap();
    let rem_branches = br_rem_json["data"]["branches"].as_array().unwrap();
    assert!(rem_branches.iter().all(|b| b["is_remote"] == true));
    assert!(rem_branches.iter().any(|b| b["name"] == "origin/main"));

    // `oz merge --ff-only origin/main` must refuse diverged branches.
    let ff_fail = run_oz(&repo, &["merge", "--ff-only", "origin/main", "--json"]);
    assert!(
        !ff_fail.status.success(),
        "--ff-only must fail on diverged branch"
    );

    // `oz merge origin/main` creates a 2-parent append-only merge commit and is OpLog-reversible!
    let merge_out = run_oz(&repo, &["merge", "origin/main", "--json"]);
    assert!(
        merge_out.status.success(),
        "oz merge failed: stdout={} stderr={}",
        String::from_utf8_lossy(&merge_out.stdout),
        String::from_utf8_lossy(&merge_out.stderr)
    );
    let merge_json: Value = serde_json::from_slice(&merge_out.stdout).unwrap();
    assert_eq!(merge_json["ok"], true);
    assert_eq!(merge_json["data"]["into_branch"], "feat/pr-1");
    assert_eq!(merge_json["data"]["fast_forwarded"], false);
    assert!(repo.join("src/main_extra.rs").is_file());
    let parents = git_ok(&repo, &["rev-list", "--parents", "-n", "1", "HEAD"]);
    assert_eq!(
        parents.split_whitespace().count(),
        3,
        "merge commit must have 2 parents (got: {parents})"
    );

    // `oz undo` reverses `oz merge` cleanly!
    let undo_out = run_oz(&repo, &["undo", "--json"]);
    assert!(
        undo_out.status.success(),
        "oz undo failed: stdout={} stderr={}",
        String::from_utf8_lossy(&undo_out.stdout),
        String::from_utf8_lossy(&undo_out.stderr)
    );
    assert_eq!(git_ok(&repo, &["rev-parse", "HEAD"]), pre_merge_feat_oid);
    assert!(!repo.join("src/main_extra.rs").exists());

    // `oz -C <plain-linked-worktree> merge origin/main` also syncs plain `git worktree add` checkouts!
    let linked_wt = tmp.path().join("wt-pr-2");
    git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feat/pr-2",
            linked_wt.to_str().unwrap(),
            "feat/pr-1",
        ],
    );
    let wt_merge = run_oz(&linked_wt, &["merge", "origin/main", "--json"]);
    assert!(
        wt_merge.status.success(),
        "linked worktree oz merge failed: stdout={} stderr={}",
        String::from_utf8_lossy(&wt_merge.stdout),
        String::from_utf8_lossy(&wt_merge.stderr)
    );
    assert!(linked_wt.join("src/main_extra.rs").is_file());
    assert!(
        git_ok(&linked_wt, &["status", "--porcelain"]).is_empty(),
        "linked worktree index must be clean after oz merge"
    );

    // Multi-refspec push: `oz push origin main feat/pr-1 feat/pr-2` pushes all 3 without checking them out!
    git_ok(&repo, &["branch", "feat/pr-3", "feat/pr-1"]);
    let push_multi = run_oz(
        &repo,
        &[
            "push",
            "origin",
            "feat/pr-1",
            "feat/pr-2",
            "feat/pr-3",
            "--json",
        ],
    );
    assert!(
        push_multi.status.success(),
        "multi-refspec oz push failed: stdout={} stderr={}",
        String::from_utf8_lossy(&push_multi.stdout),
        String::from_utf8_lossy(&push_multi.stderr)
    );
    let push_json: Value = serde_json::from_slice(&push_multi.stdout).unwrap();
    assert_eq!(
        push_json["data"]["pushed_refs"].as_array().unwrap().len(),
        3
    );
    assert_eq!(
        git_ok(&bare_remote, &["rev-parse", "refs/heads/feat/pr-1"]),
        pre_merge_feat_oid
    );
    assert_eq!(
        git_ok(&bare_remote, &["rev-parse", "refs/heads/feat/pr-3"]),
        pre_merge_feat_oid
    );
    assert_eq!(
        git_ok(&bare_remote, &["rev-parse", "refs/heads/feat/pr-2"]),
        git_ok(&linked_wt, &["rev-parse", "HEAD"])
    );
}

#[test]
fn pr_comments_issue_view_multi_pr_and_workflow_ci_against_mock_github() {
    let (_tmp, repo) = init_sample_repo();
    git_ok(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/demo.git",
        ],
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let api_base = format!("http://{addr}");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        while !stop_bg.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let _ = stream.set_nonblocking(false);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                continue;
            }
            let parts: Vec<&str> = request_line.split_whitespace().collect();
            let method = parts.first().copied().unwrap_or("GET");
            let raw_target = parts.get(1).copied().unwrap_or("/");
            let path = raw_target.split('?').next().unwrap_or(raw_target);

            let mut content_len: usize = 0;
            loop {
                let mut hdr = String::new();
                if reader.read_line(&mut hdr).unwrap_or(0) == 0 || hdr == "\r\n" || hdr == "\n" {
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

            let sha_3320 = "a".repeat(40);
            let sha_3321 = "b".repeat(40);

            let (status, content_type, resp_body): (&str, &str, Vec<u8>) = match (method, path) {
                ("GET", "/repos/acme/demo/pulls/3320") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&GitHubPullRequest {
                        number: 3320,
                        html_url: "https://github.com/acme/demo/pull/3320".to_string(),
                        state: "open".to_string(),
                        title: "fix: opentelemetry decorator".to_string(),
                        body: Some("Fix duplicate decorator".to_string()),
                        head: GitHubPrRef {
                            ref_name: "fix/otel".to_string(),
                            sha: sha_3320.clone(),
                            label: None,
                        },
                        base: GitHubPrRef {
                            ref_name: "main".to_string(),
                            sha: "0".repeat(40),
                            label: None,
                        },
                        user: Some(GitHubUser {
                            login: "agent".to_string(),
                        }),
                        ..Default::default()
                    })
                    .unwrap(),
                ),
                ("GET", "/repos/acme/demo/pulls/3321") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&GitHubPullRequest {
                        number: 3321,
                        html_url: "https://github.com/acme/demo/pull/3321".to_string(),
                        state: "open".to_string(),
                        title: "feat: video worker".to_string(),
                        head: GitHubPrRef {
                            ref_name: "feat/video".to_string(),
                            sha: sha_3321.clone(),
                            label: None,
                        },
                        base: GitHubPrRef {
                            ref_name: "main".to_string(),
                            sha: "0".repeat(40),
                            label: None,
                        },
                        ..Default::default()
                    })
                    .unwrap(),
                ),
                ("GET", "/repos/acme/demo/pulls/3320/files") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&vec![GitHubPullRequestFile {
                        filename: "src/otel.ts".to_string(),
                        previous_filename: None,
                        status: "modified".to_string(),
                        additions: 3,
                        deletions: 1,
                        changes: 4,
                        sha: None,
                        patch: Some("@@ -10,3 +10,5 @@\n ctx\n+fix\n".to_string()),
                    }])
                    .unwrap(),
                ),
                ("GET", "/repos/acme/demo/pulls/3320/reviews") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&vec![GitHubPullRequestReview {
                        id: 501,
                        state: "CHANGES_REQUESTED".to_string(),
                        body: Some("Please guard duplicate decorator registration.".to_string()),
                        html_url: None,
                        commit_id: Some(sha_3320.clone()),
                        submitted_at: Some("2026-11-25T08:00:00Z".to_string()),
                        user: Some(GitHubUser {
                            login: "foreman-bot".to_string(),
                        }),
                    }])
                    .unwrap(),
                ),
                ("GET", "/repos/acme/demo/pulls/3320/comments") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&vec![
                        GitHubPullRequestComment {
                            id: 101,
                            pull_request_review_id: Some(501),
                            in_reply_to_id: None,
                            path: "src/otel.ts".to_string(),
                            line: Some(12),
                            original_line: Some(12),
                            side: Some("RIGHT".to_string()),
                            body: "Guard with hasDecorator('opentelemetry')".to_string(),
                            user: Some(GitHubUser {
                                login: "foreman-bot".to_string(),
                            }),
                            created_at: Some("2026-11-25T08:00:01Z".to_string()),
                            ..Default::default()
                        },
                        GitHubPullRequestComment {
                            id: 102,
                            pull_request_review_id: Some(502),
                            in_reply_to_id: Some(101),
                            path: "src/otel.ts".to_string(),
                            line: Some(12),
                            original_line: Some(12),
                            side: Some("RIGHT".to_string()),
                            body: "Fixed in latest commit.".to_string(),
                            user: Some(GitHubUser {
                                login: "agent".to_string(),
                            }),
                            created_at: Some("2026-11-25T08:05:00Z".to_string()),
                            ..Default::default()
                        },
                        GitHubPullRequestComment {
                            id: 201,
                            pull_request_review_id: Some(501),
                            in_reply_to_id: None,
                            path: "src/app.ts".to_string(),
                            line: None,
                            original_line: Some(44),
                            side: Some("RIGHT".to_string()),
                            body: "Nit: rename helper".to_string(),
                            user: Some(GitHubUser {
                                login: "foreman-bot".to_string(),
                            }),
                            created_at: Some("2026-11-25T08:00:02Z".to_string()),
                            ..Default::default()
                        },
                    ])
                    .unwrap(),
                ),
                ("POST", "/graphql") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&json!({
                        "data": {
                            "repository": {
                                "pullRequest": {
                                    "reviewThreads": {
                                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                                        "nodes": [
                                            {
                                                "id": "PRRT_unresolved",
                                                "isResolved": false,
                                                "isOutdated": false,
                                                "comments": { "nodes": [ { "databaseId": 101 }, { "databaseId": 102 } ] }
                                            },
                                            {
                                                "id": "PRRT_resolved",
                                                "isResolved": true,
                                                "isOutdated": true,
                                                "comments": { "nodes": [ { "databaseId": 201 } ] }
                                            }
                                        ]
                                    }
                                }
                            }
                        }
                    }))
                    .unwrap(),
                ),
                ("GET", "/repos/acme/demo/issues/3320/comments") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&vec![GitHubIssueComment {
                        id: 901,
                        body: "Deploy preview ready.".to_string(),
                        user: Some(GitHubUser {
                            login: "ci-bot".to_string(),
                        }),
                        created_at: Some("2026-11-25T08:01:00Z".to_string()),
                        ..Default::default()
                    }])
                    .unwrap(),
                ),
                ("GET", "/repos/acme/demo/issues/3311") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&GitHubIssue {
                        number: 3311,
                        html_url: "https://github.com/acme/demo/issues/3311".to_string(),
                        state: "open".to_string(),
                        title: "Bug: duplicate otel decorator".to_string(),
                        body: Some("Repro steps here".to_string()),
                        user: Some(GitHubUser {
                            login: "alice".to_string(),
                        }),
                        labels: vec![GitHubLabel {
                            name: "bug".to_string(),
                            ..Default::default()
                        }],
                        comments: 1,
                        ..Default::default()
                    })
                    .unwrap(),
                ),
                ("GET", "/repos/acme/demo/issues/3311/comments") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&vec![GitHubIssueComment {
                        id: 77,
                        body: "Confirmed on staging.".to_string(),
                        user: Some(GitHubUser {
                            login: "bob".to_string(),
                        }),
                        ..Default::default()
                    }])
                    .unwrap(),
                ),
                ("GET", p) if p == format!("/repos/acme/demo/commits/{sha_3320}/check-runs") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&json!({
                        "check_runs": [GitHubCheckRun {
                            id: 1,
                            name: "lint-tests-types".to_string(),
                            status: "completed".to_string(),
                            conclusion: Some("success".to_string()),
                            details_url: None,
                            html_url: None,
                            output: GitHubCheckRunOutput::default(),
                        }]
                    }))
                    .unwrap(),
                ),
                ("GET", p) if p == format!("/repos/acme/demo/commits/{sha_3321}/check-runs") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&json!({
                        "check_runs": [GitHubCheckRun {
                            id: 2,
                            name: "video-tests".to_string(),
                            status: "completed".to_string(),
                            conclusion: Some("failure".to_string()),
                            details_url: Some("https://github.com/acme/demo/actions/runs/888/job/999".to_string()),
                            html_url: None,
                            output: GitHubCheckRunOutput::default(),
                        }]
                    }))
                    .unwrap(),
                ),
                ("GET", "/repos/acme/demo/actions/runs") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&json!({ "workflow_runs": [] })).unwrap(),
                ),
                ("GET", "/repos/acme/demo/actions/workflows") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&json!({
                        "workflows": [GitHubWorkflow {
                            id: 4242,
                            name: "Sentry Release".to_string(),
                            path: ".github/workflows/sentry-release.yaml".to_string(),
                            state: "active".to_string(),
                        }]
                    }))
                    .unwrap(),
                ),
                (
                    "GET",
                    "/repos/acme/demo/actions/workflows/sentry-release.yaml/runs"
                    | "/repos/acme/demo/actions/workflows/4242/runs",
                ) => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&json!({
                        "workflow_runs": [GitHubWorkflowRun {
                            id: 888,
                            name: Some("Sentry Release".to_string()),
                            head_branch: Some("main".to_string()),
                            head_sha: sha_3320.clone(),
                            status: "completed".to_string(),
                            conclusion: Some("failure".to_string()),
                            event: Some("push".to_string()),
                            ..Default::default()
                        }]
                    }))
                    .unwrap(),
                ),
                ("GET", "/repos/acme/demo/actions/runs/888/jobs") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&json!({
                        "jobs": [GitHubActionsJob {
                            id: 999,
                            run_id: Some(888),
                            name: "publish-sentry".to_string(),
                            status: "completed".to_string(),
                            conclusion: Some("failure".to_string()),
                            html_url: None,
                            steps: vec![GitHubJobStep {
                                name: "Upload sourcemaps".to_string(),
                                status: "completed".to_string(),
                                conclusion: Some("failure".to_string()),
                                number: 4,
                            }],
                        }]
                    }))
                    .unwrap(),
                ),
                ("GET", "/repos/acme/demo/actions/jobs/999") => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&GitHubActionsJob {
                        id: 999,
                        run_id: Some(888),
                        name: "video-tests".to_string(),
                        status: "completed".to_string(),
                        conclusion: Some("failure".to_string()),
                        html_url: None,
                        steps: vec![GitHubJobStep {
                            name: "Run jest".to_string(),
                            status: "completed".to_string(),
                            conclusion: Some("failure".to_string()),
                            number: 3,
                        }],
                    })
                    .unwrap(),
                ),
                ("GET", "/repos/acme/demo/actions/jobs/999/logs") => (
                    "200 OK",
                    "text/plain",
                    b"2026-11-25T08:00:00Z ##[error]SENTRY_AUTH_TOKEN missing\n".to_vec(),
                ),
                _ => ("404 Not Found", "application/json", b"{}".to_vec()),
            };

            let hdr = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                resp_body.len()
            );
            let _ = stream.write_all(hdr.as_bytes());
            let _ = stream.write_all(&resp_body);
            let _ = stream.flush();
        }
    });

    let envs = [
        ("OZ_GITHUB_API_URL", api_base.as_str()),
        ("GITHUB_TOKEN", "tok"),
    ];

    // 1. `oz pr comments 3320 --json` returns review verdict, grouped threads with resolved/outdated, and issue comments.
    let pr_c = run_oz_env(&repo, &["pr", "comments", "3320", "--json"], &envs);
    assert!(
        pr_c.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&pr_c.stderr)
    );
    let v: Value = serde_json::from_slice(&pr_c.stdout).unwrap();
    assert_eq!(v["data"]["review_decision"], "CHANGES_REQUESTED");
    assert_eq!(v["data"]["total_threads"], 2);
    assert_eq!(v["data"]["unresolved_threads"], 1);
    assert_eq!(v["data"]["outdated_threads"], 1);
    assert_eq!(v["data"]["total_issue_comments"], 1);
    let threads = v["data"]["threads"].as_array().unwrap();
    let otel_thread = threads.iter().find(|t| t["path"] == "src/otel.ts").unwrap();
    assert_eq!(otel_thread["is_resolved"], false);
    assert_eq!(otel_thread["comments"].as_array().unwrap().len(), 2);

    // 2. `oz pr comments 3320 --unresolved --json` filters out resolved threads.
    let pr_u = run_oz_env(
        &repo,
        &["pr", "comments", "3320", "--unresolved", "--json"],
        &envs,
    );
    assert!(pr_u.status.success());
    let vu: Value = serde_json::from_slice(&pr_u.stdout).unwrap();
    assert_eq!(vu["data"]["total_threads"], 1);
    assert_eq!(vu["data"]["threads"][0]["path"], "src/otel.ts");

    // 3. `oz pr view 3320 --comments --json` includes threads & conversation comments too.
    let pr_v = run_oz_env(
        &repo,
        &["pr", "view", "3320", "--comments", "--json"],
        &envs,
    );
    assert!(pr_v.status.success());
    let vv: Value = serde_json::from_slice(&pr_v.stdout).unwrap();
    assert_eq!(vv["data"]["review_decision"], "CHANGES_REQUESTED");
    assert_eq!(vv["data"]["threads"].as_array().unwrap().len(), 2);
    assert_eq!(vv["data"]["issue_comments"].as_array().unwrap().len(), 1);

    // 4. `oz issue view` supports both number and pasted GitHub URL via REST.
    let iss = run_oz_env(
        &repo,
        &[
            "issue",
            "view",
            "https://github.com/acme/demo/issues/3311",
            "--comments",
            "--json",
        ],
        &envs,
    );
    assert!(
        iss.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&iss.stderr)
    );
    let vi: Value = serde_json::from_slice(&iss.stdout).unwrap();
    assert_eq!(vi["data"]["issue"]["number"], 3311);
    assert_eq!(
        vi["data"]["issue"]["title"],
        "Bug: duplicate otel decorator"
    );
    assert_eq!(vi["data"]["comments"][0]["body"], "Confirmed on staging.");

    // 5. `oz ci --pr 3320,3321 --json` aggregates multiple PRs in one call.
    let multi = run_oz_env(&repo, &["ci", "--pr", "3320,3321", "--json"], &envs);
    // Exits non-zero because PR #3321 has a failing check, while returning full JSON envelope.
    assert!(!multi.status.success());
    let vm: Value = serde_json::from_slice(&multi.stdout).unwrap();
    assert_eq!(vm["data"]["pr_count"], 2);
    assert_eq!(vm["data"]["passing_prs"], 1);
    assert_eq!(vm["data"]["failing_prs"], 1);
    assert_eq!(vm["data"]["overall_status"], "failing");

    // 6. `oz ci --workflow sentry-release.yaml --json` and display-name lookup `"Sentry Release"`.
    let wf = run_oz_env(
        &repo,
        &["ci", "--workflow", "sentry-release.yaml", "--json"],
        &envs,
    );
    assert!(!wf.status.success());
    let vw: Value = serde_json::from_slice(&wf.stdout).unwrap();
    assert_eq!(vw["data"]["overall_status"], "failing");
    assert_eq!(vw["data"]["runs"][0]["run_id"], 888);
    assert_eq!(
        vw["data"]["runs"][0]["jobs"][0]["failing_step"],
        "Upload sourcemaps"
    );
    assert!(
        vw["data"]["runs"][0]["jobs"][0]["log_excerpt"]
            .as_str()
            .unwrap()
            .contains("SENTRY_AUTH_TOKEN missing")
    );

    let wf_by_name = run_oz_env(
        &repo,
        &["ci", "--workflow", "Sentry Release", "--json"],
        &envs,
    );
    let vwn: Value = serde_json::from_slice(&wf_by_name.stdout).unwrap();
    assert_eq!(vwn["data"]["workflow_id"], "4242");
    assert_eq!(vwn["data"]["runs"][0]["run_id"], 888);

    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
}
