// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end v1.3 agent feedback tests:
//! - Workspace pnpm monorepo writable metadata under `cas_hardlink` (`node_modules/.modules.yaml`,
//!   `.pnpm-workspace-state-v1.json`, `node_modules/.bin/*`, nested workspace `node_modules`)
//! - `oz pr list`, `oz pr edit`, `oz pr reply` (`t1` / `#id` + `--resolve`), `oz pr resolve`,
//!   `oz pr status --pr ...`, `oz pr view --brief`/`--no-body`/`--no-files`, `--body-file`
//! - `oz issue list`, `oz issue comment`
//! - `oz ci --json` compact passing checks (default) vs `--verbose`, null omission, accurate
//!   top-level `branch`/`head_oid` envelope, and cross-workflow `oz ci --runs`
//! - `oz grep` (`-n` no-op, `--exclude`, `:!`, `--max-line-len`, `\|` BRE-dialect hint)
//! - `oz log` (multi-rev, `A...B --left-right`, `-i`, `--no-notes`, `--format fuller`)
//! - `oz show <missing-hex-sha>` hinting `oz fetch`

#![allow(clippy::too_many_lines)]

use clap::Parser;
use oz_cli::Cli;
use oz_github::{
    GitHubCheckRun, GitHubCheckRunOutput, GitHubIssue, GitHubIssueComment, GitHubLabel,
    GitHubPrRef, GitHubPullRequest, GitHubPullRequestComment, GitHubPullRequestFile,
    GitHubPullRequestReview, GitHubUser, GitHubWorkflowRun,
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
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("snapshots")).unwrap();
    git_ok(&repo, &["init", "-b", "main"]);
    git_ok(&repo, &["config", "user.name", "Gaalix Dev"]);
    git_ok(&repo, &["config", "user.email", "dev@example.com"]);
    git_ok(&repo, &["config", "core.autocrlf", "false"]);

    fs::write(
        repo.join("src/app.ts"),
        "export const CLEOS_MODE = 'alpha';\nexport const BETA = 'beta';\n",
    )
    .unwrap();
    let giant_json = format!(
        "{{\"workflow\":\"alpha\",\"blob\":\"{}\"}}",
        "x".repeat(1200)
    );
    fs::write(repo.join("snapshots/huge.json"), giant_json).unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "chore: initial Cleos setup"]);
    (tmp, repo)
}

#[test]
fn cli_parses_all_v1_3_commands_and_flags() {
    assert!(
        Cli::try_parse_from([
            "oz", "pr", "list", "--state", "merged", "--search", "cleos", "--limit", "15", "--json"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from([
            "oz",
            "pr",
            "edit",
            "3340",
            "--title",
            "Updated",
            "--body-file",
            "-",
            "--base",
            "main",
            "--ready"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from([
            "oz",
            "pr",
            "reply",
            "3347",
            "t1",
            "-m",
            "Fixed",
            "--resolve"
        ])
        .is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "pr", "resolve", "3347", "t1", "--unresolve"]).is_ok());
    assert!(
        Cli::try_parse_from(["oz", "pr", "status", "--pr", "3338,3339,3340", "--json"]).is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "pr", "view", "3338", "--brief", "--json"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "pr", "view", "3338", "--no-body", "--no-files"]).is_ok());
    assert!(
        Cli::try_parse_from([
            "oz",
            "pr",
            "create",
            "--title",
            "T",
            "--body-file",
            "-",
            "--no-push"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from([
            "oz", "issue", "list", "--state", "open", "--search", "cleos", "--label", "bug",
            "--limit", "10"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from(["oz", "issue", "comment", "3344", "--body", "42 open deals"]).is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "issue", "comment", "3344", "--body-file", "-"]).is_ok());
    assert!(
        Cli::try_parse_from([
            "oz", "ci", "--runs", "--branch", "main", "--limit", "5", "--json"
        ])
        .is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "ci", "--pr", "3338,3339", "--verbose", "--json"]).is_ok());
    assert!(
        Cli::try_parse_from([
            "oz",
            "grep",
            "-n",
            "alpha|beta",
            "--exclude",
            "*.json",
            "--max-line-len",
            "80"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from(["oz", "log", "-n", "3", "--oneline", "HEAD", "origin/main"]).is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "log", "--oneline", "--left-right", "main...HEAD"]).is_ok());
    assert!(
        Cli::try_parse_from([
            "oz",
            "log",
            "--grep",
            "cleos",
            "-i",
            "--no-notes",
            "--format",
            "fuller"
        ])
        .is_ok()
    );
}

#[test]
fn grep_log_and_show_ergonomics_work_end_to_end() {
    let (_tmp, repo) = init_sample_repo();

    // 1. `oz grep -n` (no-op), `--exclude`, `--max-line-len`, and `\|` BRE hint
    let grep_n = run_oz(
        &repo,
        &["grep", "-n", "alpha|beta", "--exclude", "*.json", "--json"],
    );
    assert!(
        grep_n.status.success(),
        "oz grep -n failed: {}",
        String::from_utf8_lossy(&grep_n.stderr)
    );
    let grep_n_json: Value = serde_json::from_slice(&grep_n.stdout).unwrap();
    assert_eq!(grep_n_json["data"]["total_matches"], 2);
    assert!(
        grep_n_json["data"]["matched_files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v != "snapshots/huge.json"),
        "--exclude '*.json' must skip snapshots/huge.json"
    );

    // Without --exclude, giant JSON line is truncated to --max-line-len 80
    let grep_trunc = run_oz(
        &repo,
        &["grep", "workflow", "--max-line-len", "80", "--json"],
    );
    assert!(grep_trunc.status.success());
    let trunc_json: Value = serde_json::from_slice(&grep_trunc.stdout).unwrap();
    assert_eq!(trunc_json["data"]["lines_truncated"], 1);
    let line = trunc_json["data"]["matches"][0]["line"].as_str().unwrap();
    assert!(line.contains("chars truncated"), "got line: {line}");
    assert!(line.len() < 160);

    // BRE `\|` returning 0 hits emits an explicit warning mentioning Rust/ERE `a|b`
    let grep_bre = run_oz(&repo, &["grep", r"alpha\|beta", "--json"]);
    assert!(grep_bre.status.success());
    let bre_json: Value = serde_json::from_slice(&grep_bre.stdout).unwrap();
    assert_eq!(bre_json["data"]["total_matches"], 0);
    let warnings = bre_json["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().is_some_and(|s| s.contains("Rust/ERE"))),
        "expected Rust/ERE warning, got: {warnings:?}"
    );

    // 2. Multi-rev `oz log`, symmetric `A...B --left-right`, `-i`, `--no-notes`, `--format fuller`
    git_ok(&repo, &["checkout", "-b", "feat/side"]);
    fs::write(repo.join("src/side.ts"), "export const SIDE = 1;\n").unwrap();
    let commit_out = run_oz(
        &repo,
        &["commit", "-m", "feat(cleos): add side module", "--json"],
    );
    assert!(
        commit_out.status.success(),
        "oz commit failed: {}",
        String::from_utf8_lossy(&commit_out.stderr)
    );

    git_ok(&repo, &["checkout", "main"]);
    fs::write(repo.join("src/main_only.ts"), "export const MAIN = 1;\n").unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "chore: advance main"]);

    // Multi-rev log accepts two branches!
    let log_multi = run_oz(
        &repo,
        &["log", "-n", "5", "--oneline", "HEAD", "feat/side", "--json"],
    );
    assert!(
        log_multi.status.success(),
        "multi-rev oz log failed: {}",
        String::from_utf8_lossy(&log_multi.stderr)
    );
    let log_multi_json: Value = serde_json::from_slice(&log_multi.stdout).unwrap();
    let subjects: Vec<&str> = log_multi_json["data"]["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["subject"].as_str().unwrap())
        .collect();
    assert!(subjects.iter().any(|s| s.contains("add side module")));
    assert!(subjects.iter().any(|s| s.contains("advance main")));

    // Symmetric diff `main...feat/side --left-right`
    let log_sym = run_oz(
        &repo,
        &[
            "log",
            "--oneline",
            "--left-right",
            "main...feat/side",
            "--json",
        ],
    );
    assert!(
        log_sym.status.success(),
        "symmetric oz log failed: {}",
        String::from_utf8_lossy(&log_sym.stderr)
    );
    let sym_json: Value = serde_json::from_slice(&log_sym.stdout).unwrap();
    let sym_commits = sym_json["data"]["commits"].as_array().unwrap();
    assert_eq!(sym_commits.len(), 2);
    assert!(
        sym_commits
            .iter()
            .any(|c| c["side"] == "left" && c["subject"] == "chore: advance main")
    );
    assert!(
        sym_commits
            .iter()
            .any(|c| c["side"] == "right" && c["subject"] == "feat(cleos): add side module")
    );

    // `-i` case-insensitive grep + `--no-notes` hides AI provenance + `--format fuller`
    let log_i = run_oz(
        &repo,
        &[
            "log",
            "feat/side",
            "--grep",
            "CLEOS",
            "-i",
            "--no-notes",
            "--oneline",
        ],
    );
    assert!(log_i.status.success());
    let log_i_txt = String::from_utf8_lossy(&log_i.stdout);
    assert!(log_i_txt.contains("feat(cleos): add side module"));
    assert!(
        !log_i_txt.contains("[AI:"),
        "--no-notes must suppress [AI:...] badge: {log_i_txt}"
    );

    let log_fuller = run_oz(
        &repo,
        &["log", "feat/side", "-n", "1", "--format", "fuller"],
    );
    assert!(log_fuller.status.success());
    let fuller_txt = String::from_utf8_lossy(&log_fuller.stdout);
    assert!(fuller_txt.contains("AuthorDate:"));
    assert!(fuller_txt.contains("CommitDate:"));
    assert!(!fuller_txt.contains("[AI:"));

    // 3. `oz show <missing-hex-sha>` hints `oz fetch`
    let show_miss = run_oz(&repo, &["show", "7e411af23a9876543210", "--json"]);
    assert!(!show_miss.status.success());
    let show_err: Value = serde_json::from_slice(&show_miss.stdout).unwrap();
    let rec = show_err["recovery_commands"].as_array().unwrap();
    assert!(
        rec.iter().any(|r| r == "oz fetch"),
        "missing hex SHA should hint `oz fetch`, got: {rec:?}"
    );
}

#[test]
fn cas_hardlink_keeps_pnpm_metadata_writable_and_seeds_nested_workspaces() {
    let (_tmp, repo) = init_sample_repo();
    fs::write(
        repo.join("package.json"),
        "{\"name\":\"monorepo\",\"private\":true}\n",
    )
    .unwrap();
    fs::write(
        repo.join("pnpm-workspace.yaml"),
        "packages:\n  - 'packages/*'\n",
    )
    .unwrap();
    fs::create_dir_all(repo.join("packages/api")).unwrap();
    fs::write(
        repo.join("packages/api/package.json"),
        "{\"name\":\"@mono/api\"}\n",
    )
    .unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "chore: add pnpm monorepo files"]);

    // Populate warm pnpm node_modules (root + nested workspace package)
    fs::create_dir_all(repo.join("node_modules/.bin")).unwrap();
    fs::create_dir_all(repo.join("node_modules/.pnpm")).unwrap();
    fs::create_dir_all(repo.join("node_modules/lodash")).unwrap();
    fs::create_dir_all(repo.join("packages/api/node_modules/.bin")).unwrap();

    fs::write(
        repo.join("node_modules/.modules.yaml"),
        "storeDir: /store\n",
    )
    .unwrap();
    fs::write(
        repo.join("node_modules/.pnpm-workspace-state-v1.json"),
        "{\"version\":1}\n",
    )
    .unwrap();
    fs::write(
        repo.join("node_modules/.pnpm/lock.yaml"),
        "lockfileVersion: '9.0'\n",
    )
    .unwrap();
    fs::write(
        repo.join("node_modules/.bin/jest"),
        "#!/bin/sh\necho jest\n",
    )
    .unwrap();
    fs::write(
        repo.join("node_modules/lodash/index.js"),
        "module.exports = {};\n",
    )
    .unwrap();
    fs::write(
        repo.join("packages/api/node_modules/.modules.yaml"),
        "hoistPattern: []\n",
    )
    .unwrap();

    let spawn = run_oz(
        &repo,
        &[
            "ws",
            "spawn",
            "pnpm-ws",
            "--backend",
            "cas_hardlink",
            "--json",
        ],
    );
    assert!(
        spawn.status.success(),
        "oz ws spawn failed: stdout={} stderr={}",
        String::from_utf8_lossy(&spawn.stdout),
        String::from_utf8_lossy(&spawn.stderr)
    );
    let spawn_json: Value = serde_json::from_slice(&spawn.stdout).unwrap();
    let ws_path = PathBuf::from(spawn_json["data"]["worktree_path"].as_str().unwrap());

    // Mutable pnpm metadata & shims MUST be writable (not 0444 read-only!)
    for mutable_rel in [
        "node_modules/.modules.yaml",
        "node_modules/.pnpm-workspace-state-v1.json",
        "node_modules/.pnpm/lock.yaml",
        "node_modules/.bin/jest",
        "packages/api/node_modules/.modules.yaml",
    ] {
        let p = ws_path.join(mutable_rel);
        assert!(p.is_file(), "expected {mutable_rel} to exist in workspace");
        assert!(
            !fs::metadata(&p).unwrap().permissions().readonly(),
            "{mutable_rel} must be writable in cas_hardlink workspace"
        );
        // Opening for write must succeed without EACCES!
        fs::OpenOptions::new()
            .append(true)
            .open(&p)
            .unwrap_or_else(|e| panic!("failed opening {mutable_rel} writable: {e}"));
    }

    // Regular immutable package files stay read-only CAS hardlinks
    let immutable = ws_path.join("node_modules/lodash/index.js");
    assert!(immutable.is_file());
    assert!(fs::metadata(&immutable).unwrap().permissions().readonly());
}

#[test]
fn github_pr_lifecycle_issue_and_compact_ci_work_against_mock_server() {
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
    let local_main_sha = git_ok(&repo, &["rev-parse", "main"]);
    // Put local checkout on an unrelated feature branch to verify `oz ci --json` envelope accuracy!
    git_ok(&repo, &["checkout", "-b", "fix/unrelated-local-branch"]);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let api_base = format!("http://{addr}");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        let sha_3340 = "a".repeat(40);
        let sha_3341 = "b".repeat(40);
        let sha_main = local_main_sha;

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
            let (path, query) = raw_target.split_once('?').unwrap_or((raw_target, ""));

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
            let body_str = String::from_utf8_lossy(&body_bytes);

            let pr_3340 = GitHubPullRequest {
                number: 3340,
                html_url: "https://github.com/acme/demo/pull/3340".to_string(),
                state: "open".to_string(),
                draft: false,
                merged: false,
                title: "feat: Cleos risk class".to_string(),
                body: Some("Detailed PR description body".to_string()),
                head: GitHubPrRef {
                    ref_name: "feat/3340".to_string(),
                    sha: sha_3340.clone(),
                    label: None,
                },
                base: GitHubPrRef {
                    ref_name: "main".to_string(),
                    sha: sha_main.clone(),
                    label: None,
                },
                user: Some(GitHubUser {
                    login: "dev".to_string(),
                }),
                mergeable: Some(true),
                mergeable_state: Some("clean".to_string()),
                ..Default::default()
            };
            let pr_3341 = GitHubPullRequest {
                number: 3341,
                html_url: "https://github.com/acme/demo/pull/3341".to_string(),
                state: "open".to_string(),
                draft: false,
                merged: false,
                title: "fix: HubSpot serverless".to_string(),
                body: Some("Body 3341".to_string()),
                head: GitHubPrRef {
                    ref_name: "fix/3341".to_string(),
                    sha: sha_3341.clone(),
                    label: None,
                },
                base: GitHubPrRef {
                    ref_name: "main".to_string(),
                    sha: sha_main.clone(),
                    label: None,
                },
                user: Some(GitHubUser {
                    login: "dev".to_string(),
                }),
                mergeable: Some(false),
                mergeable_state: Some("dirty".to_string()),
                ..Default::default()
            };

            let (status, resp_body): (&str, Vec<u8>) = match (method, path) {
                ("GET", "/repos/acme/demo/pulls") => (
                    "200 OK",
                    serde_json::to_vec(&vec![pr_3340.clone(), pr_3341.clone()]).unwrap(),
                ),
                ("GET", "/repos/acme/demo/pulls/3340") => {
                    ("200 OK", serde_json::to_vec(&pr_3340).unwrap())
                }
                ("PATCH", "/repos/acme/demo/pulls/3340") => {
                    let req_json: Value = serde_json::from_str(&body_str).unwrap_or_default();
                    let mut updated = pr_3340.clone();
                    if let Some(t) = req_json.get("title").and_then(|v| v.as_str()) {
                        updated.title = t.to_string();
                    }
                    if let Some(b) = req_json.get("body").and_then(|v| v.as_str()) {
                        updated.body = Some(b.to_string());
                    }
                    ("200 OK", serde_json::to_vec(&updated).unwrap())
                }
                ("GET", "/repos/acme/demo/pulls/3341") => {
                    ("200 OK", serde_json::to_vec(&pr_3341).unwrap())
                }
                ("GET", "/repos/acme/demo/pulls/3340/files") => (
                    "200 OK",
                    serde_json::to_vec(&vec![GitHubPullRequestFile {
                        filename: "src/app.ts".to_string(),
                        previous_filename: None,
                        status: "modified".to_string(),
                        additions: 5,
                        deletions: 1,
                        changes: 6,
                        sha: None,
                        patch: None,
                    }])
                    .unwrap(),
                ),
                ("GET", "/repos/acme/demo/pulls/3340/reviews") => (
                    "200 OK",
                    serde_json::to_vec(&vec![GitHubPullRequestReview {
                        id: 1,
                        state: "APPROVED".to_string(),
                        body: None,
                        html_url: None,
                        commit_id: Some(sha_3340.clone()),
                        submitted_at: None,
                        user: Some(GitHubUser {
                            login: "foreman".to_string(),
                        }),
                    }])
                    .unwrap(),
                ),
                (
                    "GET",
                    "/repos/acme/demo/pulls/3341/reviews"
                    | "/repos/acme/demo/pulls/3341/comments"
                    | "/repos/acme/demo/issues/3340/comments",
                ) => ("200 OK", b"[]".to_vec()),
                ("GET", "/repos/acme/demo/pulls/3340/comments") => (
                    "200 OK",
                    serde_json::to_vec(&vec![GitHubPullRequestComment {
                        id: 4_694_921_942,
                        path: "src/app.ts".to_string(),
                        line: Some(12),
                        side: Some("RIGHT".to_string()),
                        body: "Please tighten error handling.".to_string(),
                        user: Some(GitHubUser {
                            login: "foreman".to_string(),
                        }),
                        ..Default::default()
                    }])
                    .unwrap(),
                ),
                ("POST", "/repos/acme/demo/pulls/3340/comments/4694921942/replies") => {
                    let req_json: Value = serde_json::from_str(&body_str).unwrap_or_default();
                    let reply_body = req_json["body"].as_str().unwrap_or("").to_string();
                    (
                        "201 Created",
                        serde_json::to_vec(&GitHubPullRequestComment {
                            id: 4_694_929_999,
                            in_reply_to_id: Some(4_694_921_942),
                            path: "src/app.ts".to_string(),
                            line: Some(12),
                            side: Some("RIGHT".to_string()),
                            body: reply_body,
                            user: Some(GitHubUser {
                                login: "dev".to_string(),
                            }),
                            html_url: Some(
                                "https://github.com/acme/demo/pull/3340#discussion_r4694929999"
                                    .to_string(),
                            ),
                            ..Default::default()
                        })
                        .unwrap(),
                    )
                }
                ("POST", "/graphql") => {
                    if body_str.contains("resolveReviewThread") {
                        (
                            "200 OK",
                            serde_json::to_vec(&json!({
                                "data": {
                                    "resolveReviewThread": {
                                        "thread": { "id": "PRRT_kwDO_t1", "isResolved": true }
                                    }
                                }
                            }))
                            .unwrap(),
                        )
                    } else if body_str.contains("unresolveReviewThread") {
                        (
                            "200 OK",
                            serde_json::to_vec(&json!({
                                "data": {
                                    "unresolveReviewThread": {
                                        "thread": { "id": "PRRT_kwDO_t1", "isResolved": false }
                                    }
                                }
                            }))
                            .unwrap(),
                        )
                    } else {
                        (
                            "200 OK",
                            serde_json::to_vec(&json!({
                                "data": {
                                    "repository": {
                                        "pullRequest": {
                                            "reviewThreads": {
                                                "pageInfo": { "hasNextPage": false, "endCursor": null },
                                                "nodes": [{
                                                    "id": "PRRT_kwDO_t1",
                                                    "isResolved": false,
                                                    "isOutdated": false,
                                                    "comments": { "nodes": [{ "databaseId": 4_694_921_942_u64 }] }
                                                }]
                                            }
                                        }
                                    }
                                }
                            }))
                            .unwrap(),
                        )
                    }
                }
                ("GET", "/search/issues") => {
                    let is_pr = query.contains("is%3Apr") || query.contains("is:pr");
                    let item = GitHubIssue {
                        number: if is_pr { 3089 } else { 3344 },
                        html_url: format!(
                            "https://github.com/acme/demo/{}/{}",
                            if is_pr { "pull" } else { "issues" },
                            if is_pr { 3089 } else { 3344 }
                        ),
                        state: if is_pr { "closed" } else { "open" }.to_string(),
                        title: if is_pr {
                            "feat: Cleos merged PR".to_string()
                        } else {
                            "Audit Cleos open deals".to_string()
                        },
                        user: Some(GitHubUser {
                            login: "dev".to_string(),
                        }),
                        labels: vec![GitHubLabel {
                            name: "cleos".to_string(),
                            color: None,
                            description: None,
                        }],
                        pull_request: is_pr.then(oz_github::GitHubIssuePullRequestRef::default),
                        ..Default::default()
                    };
                    (
                        "200 OK",
                        serde_json::to_vec(&json!({
                            "total_count": 1,
                            "incomplete_results": false,
                            "items": [item]
                        }))
                        .unwrap(),
                    )
                }
                ("POST", "/repos/acme/demo/issues/3344/comments") => {
                    let req_json: Value = serde_json::from_str(&body_str).unwrap_or_default();
                    let body = req_json["body"].as_str().unwrap_or("").to_string();
                    (
                        "201 Created",
                        serde_json::to_vec(&GitHubIssueComment {
                            id: 99001,
                            body,
                            user: Some(GitHubUser {
                                login: "dev".to_string(),
                            }),
                            created_at: Some("2026-11-25T21:00:00Z".to_string()),
                            updated_at: None,
                            html_url: Some(
                                "https://github.com/acme/demo/issues/3344#issuecomment-99001"
                                    .to_string(),
                            ),
                        })
                        .unwrap(),
                    )
                }
                ("GET", p) if p == format!("/repos/acme/demo/commits/{sha_3340}/check-runs") => (
                    "200 OK",
                    serde_json::to_vec(&json!({
                        "total_count": 2,
                        "check_runs": [
                            GitHubCheckRun {
                                id: 101,
                                name: "lint".to_string(),
                                status: "completed".to_string(),
                                conclusion: Some("success".to_string()),
                                details_url: None,
                                html_url: Some("https://github.com/acme/demo/runs/101".to_string()),
                                output: GitHubCheckRunOutput::default(),
                            },
                            GitHubCheckRun {
                                id: 102,
                                name: "unit-tests".to_string(),
                                status: "completed".to_string(),
                                conclusion: Some("success".to_string()),
                                details_url: None,
                                html_url: Some("https://github.com/acme/demo/runs/102".to_string()),
                                output: GitHubCheckRunOutput::default(),
                            }
                        ]
                    }))
                    .unwrap(),
                ),
                ("GET", p) if p == format!("/repos/acme/demo/commits/{sha_3341}/check-runs") => (
                    "200 OK",
                    serde_json::to_vec(&json!({
                        "total_count": 2,
                        "check_runs": [
                            GitHubCheckRun {
                                id: 201,
                                name: "lint".to_string(),
                                status: "completed".to_string(),
                                conclusion: Some("success".to_string()),
                                details_url: None,
                                html_url: Some("https://github.com/acme/demo/runs/201".to_string()),
                                output: GitHubCheckRunOutput::default(),
                            },
                            GitHubCheckRun {
                                id: 202,
                                name: "build".to_string(),
                                status: "completed".to_string(),
                                conclusion: Some("failure".to_string()),
                                details_url: None,
                                html_url: Some("https://github.com/acme/demo/runs/202".to_string()),
                                output: GitHubCheckRunOutput {
                                    title: Some("Build failed".to_string()),
                                    summary: Some("TS2322 type mismatch".to_string()),
                                    text: None,
                                },
                            }
                        ]
                    }))
                    .unwrap(),
                ),
                ("GET", p)
                    if p == "/repos/acme/demo/commits/main/check-runs"
                        || p == format!("/repos/acme/demo/commits/{sha_main}/check-runs") =>
                {
                    (
                        "200 OK",
                        serde_json::to_vec(&json!({
                            "total_count": 1,
                            "check_runs": [
                                GitHubCheckRun {
                                    id: 301,
                                    name: "ci".to_string(),
                                    status: "completed".to_string(),
                                    conclusion: Some("success".to_string()),
                                    details_url: None,
                                    html_url: None,
                                    output: GitHubCheckRunOutput::default(),
                                }
                            ]
                        }))
                        .unwrap(),
                    )
                }
                ("GET", "/repos/acme/demo/actions/runs") => (
                    "200 OK",
                    serde_json::to_vec(&json!({
                        "total_count": 2,
                        "workflow_runs": [
                            GitHubWorkflowRun {
                                id: 9001,
                                name: Some("CI".to_string()),
                                head_branch: Some("main".to_string()),
                                head_sha: sha_main.clone(),
                                status: "completed".to_string(),
                                conclusion: Some("success".to_string()),
                                event: Some("push".to_string()),
                                ..Default::default()
                            },
                            GitHubWorkflowRun {
                                id: 9002,
                                name: Some("Sentry / Sourcemaps".to_string()),
                                head_branch: Some("main".to_string()),
                                head_sha: sha_main.clone(),
                                status: "in_progress".to_string(),
                                conclusion: None,
                                event: Some("push".to_string()),
                                ..Default::default()
                            }
                        ]
                    }))
                    .unwrap(),
                ),
                _ => ("404 Not Found", b"{\"message\":\"not found\"}".to_vec()),
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

    let envs = [
        ("OZ_GITHUB_API_URL", api_base.as_str()),
        ("GITHUB_TOKEN", "test-token"),
    ];

    // 1. `oz pr comments 3340` surfaces `[t1] #4694921942` in text output
    let comments_txt = run_oz_env(&repo, &["pr", "comments", "3340"], &envs);
    assert!(comments_txt.status.success());
    let comments_out = String::from_utf8_lossy(&comments_txt.stdout);
    assert!(
        comments_out.contains("[t1] #4694921942"),
        "expected [t1] #4694921942 handle, got:\n{comments_out}"
    );

    // 2. `oz pr reply 3340 t1 -m "Fixed" --resolve --json`
    let reply = run_oz_env(
        &repo,
        &[
            "pr",
            "reply",
            "3340",
            "t1",
            "-m",
            "Fixed in commit",
            "--resolve",
            "--json",
        ],
        &envs,
    );
    assert!(
        reply.status.success(),
        "oz pr reply failed: stdout={} stderr={}",
        String::from_utf8_lossy(&reply.stdout),
        String::from_utf8_lossy(&reply.stderr)
    );
    let reply_json: Value = serde_json::from_slice(&reply.stdout).unwrap();
    assert_eq!(
        reply_json["data"]["in_reply_to_comment_id"],
        4_694_921_942_u64
    );
    assert_eq!(reply_json["data"]["resolved"], true);
    assert_eq!(reply_json["data"]["reply"]["body"], "Fixed in commit");

    // 3. `oz pr resolve 3340 t1 --unresolve --json`
    let unres = run_oz_env(
        &repo,
        &["pr", "resolve", "3340", "t1", "--unresolve", "--json"],
        &envs,
    );
    assert!(unres.status.success());
    let unres_json: Value = serde_json::from_slice(&unres.stdout).unwrap();
    assert_eq!(unres_json["data"]["is_resolved"], false);

    // 4. `oz pr edit 3340 --title ... --body-file <file> --json`
    let body_file = repo.join(".tmp-pr-body.md");
    fs::write(&body_file, "Updated body with `backticks` and $VARS").unwrap();
    let edit = run_oz_env(
        &repo,
        &[
            "pr",
            "edit",
            "3340",
            "--title",
            "feat: Updated Cleos title",
            "--body-file",
            body_file.to_str().unwrap(),
            "--json",
        ],
        &envs,
    );
    assert!(edit.status.success());
    let edit_json: Value = serde_json::from_slice(&edit.stdout).unwrap();
    assert_eq!(
        edit_json["data"]["pull_request"]["title"],
        "feat: Updated Cleos title"
    );
    assert_eq!(
        edit_json["data"]["pull_request"]["body"],
        "Updated body with `backticks` and $VARS"
    );

    // 5. `oz pr view 3340 --brief --json` omits body & files
    let brief = run_oz_env(&repo, &["pr", "view", "3340", "--brief", "--json"], &envs);
    assert!(brief.status.success());
    let brief_json: Value = serde_json::from_slice(&brief.stdout).unwrap();
    assert!(brief_json["data"]["pull_request"]["body"].is_null());
    assert_eq!(brief_json["data"]["files"].as_array().unwrap().len(), 0);

    // 6. `oz pr list --state merged --search cleos --json` & `oz issue list` & `oz issue comment`
    let pr_list = run_oz_env(
        &repo,
        &[
            "pr", "list", "--state", "merged", "--search", "cleos", "--json",
        ],
        &envs,
    );
    assert!(pr_list.status.success());
    let pr_list_json: Value = serde_json::from_slice(&pr_list.stdout).unwrap();
    assert_eq!(pr_list_json["data"]["pull_requests"][0]["number"], 3089);

    let issue_list = run_oz_env(
        &repo,
        &["issue", "list", "--search", "cleos", "--json"],
        &envs,
    );
    assert!(issue_list.status.success());
    let issue_list_json: Value = serde_json::from_slice(&issue_list.stdout).unwrap();
    assert_eq!(issue_list_json["data"]["issues"][0]["number"], 3344);

    let issue_cmt = run_oz_env(
        &repo,
        &[
            "issue",
            "comment",
            "3344",
            "--body",
            "Open deals: 42",
            "--json",
        ],
        &envs,
    );
    assert!(issue_cmt.status.success());
    let issue_cmt_json: Value = serde_json::from_slice(&issue_cmt.stdout).unwrap();
    assert_eq!(issue_cmt_json["data"]["comment"]["id"], 99001);
    assert_eq!(issue_cmt_json["data"]["comment"]["body"], "Open deals: 42");

    // 7. `oz pr status --pr 3340,3341 --json`
    let pr_st = run_oz_env(
        &repo,
        &["pr", "status", "--pr", "3340,3341", "--json"],
        &envs,
    );
    assert!(pr_st.status.success());
    let pr_st_json: Value = serde_json::from_slice(&pr_st.stdout).unwrap();
    assert!(pr_st_json["branch"].is_null());
    assert_eq!(pr_st_json["data"]["pr_count"], 2);
    assert_eq!(pr_st_json["data"]["prs"][0]["review_decision"], "APPROVED");
    assert_eq!(pr_st_json["data"]["prs"][0]["unresolved_threads"], 1);
    assert_eq!(pr_st_json["data"]["prs"][0]["ci_status"], "passing");
    assert_eq!(pr_st_json["data"]["prs"][1]["ci_status"], "failing");
    assert_eq!(pr_st_json["data"]["prs"][1]["mergeable"], false);

    // 8. `oz ci --pr 3340,3341 --json` compacts passing checks & clears misleading local branch/head_oid
    let ci_multi = run_oz_env(&repo, &["ci", "--pr", "3340,3341", "--json"], &envs);
    assert!(!ci_multi.status.success(), "PR 3341 has a failing check");
    let ci_multi_raw = String::from_utf8_lossy(&ci_multi.stdout);
    let ci_multi_json: Value = serde_json::from_str(&ci_multi_raw).unwrap();
    assert!(
        ci_multi_json["branch"].is_null(),
        "multi-PR envelope must not report unrelated local branch"
    );
    assert!(ci_multi_json["head_oid"].is_null());
    assert!(
        !ci_multi_raw.contains("\"failing_step\": null"),
        "null optional fields must be omitted"
    );
    assert_eq!(ci_multi_json["data"]["reports"][0]["passed_count"], 2);
    assert!(ci_multi_json["data"]["reports"][0].get("checks").is_none());
    assert_eq!(ci_multi_json["data"]["reports"][1]["passed_count"], 1);
    assert_eq!(
        ci_multi_json["data"]["reports"][1]["checks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // `--verbose` preserves full passing check objects
    let ci_verbose = run_oz_env(
        &repo,
        &["ci", "--pr", "3340,3341", "--verbose", "--json"],
        &envs,
    );
    let ci_verbose_json: Value = serde_json::from_slice(&ci_verbose.stdout).unwrap();
    assert_eq!(
        ci_verbose_json["data"]["reports"][0]["checks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // 9. `oz ci --branch main --json` reports branch="main", NOT local `fix/unrelated-local-branch`
    let ci_main = run_oz_env(&repo, &["ci", "--branch", "main", "--json"], &envs);
    assert!(ci_main.status.success());
    let ci_main_json: Value = serde_json::from_slice(&ci_main.stdout).unwrap();
    assert_eq!(ci_main_json["branch"], "main");
    assert_eq!(ci_main_json["data"]["passed_count"], 1);

    // 10. `oz ci --runs --branch main --json` lists runs across all workflows
    let ci_runs = run_oz_env(
        &repo,
        &["ci", "--runs", "--branch", "main", "--json"],
        &envs,
    );
    assert!(ci_runs.status.success());
    let ci_runs_json: Value = serde_json::from_slice(&ci_runs.stdout).unwrap();
    assert_eq!(ci_runs_json["branch"], "main");
    assert_eq!(ci_runs_json["data"]["total_runs"], 2);
    assert_eq!(ci_runs_json["data"]["runs"][0]["workflow_name"], "CI");
    assert_eq!(
        ci_runs_json["data"]["runs"][1]["workflow_name"],
        "Sentry / Sourcemaps"
    );

    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
}
