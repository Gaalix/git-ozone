// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end integration tests for Ozone v1.1.0 Agent Ergonomics & PR Review workflows:
//! - `oz diff --line-numbers`, `--exclude`, `:!<path>`, loud error on unknown `:(magic)`
//! - `oz status` hunk collapsing (>5 hunks) vs `--hunks`
//! - `oz log --pickaxe`, `--grep-diff`, `--rev`, and `-- <paths...>`
//! - `oz show <rev>:<path> --lines` and binary blob handling
//! - `oz rev-parse`
//! - `oz ws spawn --backend cas_hardlink --from ... --branch ...` + `clean @ <oid>` output
//! - `oz pr view`, `oz pr diff`, `oz pr read`, and `oz pr review` (with pre-flight validation)

#![allow(clippy::too_many_lines)]

use clap::Parser;
use oz_cli::Cli;
use oz_github::{
    GitHubPrRef, GitHubPullRequest, GitHubPullRequestFile, GitHubPullRequestReview, GitHubUser,
};
use serde_json::Value;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;
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

fn git_ok(dir: &Path, args: &[&str]) {
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
}

fn init_sample_repo() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("sub/dir")).unwrap();
    fs::create_dir_all(repo.join("other")).unwrap();
    git_ok(&repo, &["init", "-b", "main"]);
    git_ok(&repo, &["config", "user.name", "Test Agent"]);
    git_ok(&repo, &["config", "user.email", "agent@example.com"]);

    fs::write(
        repo.join("sub/dir/alpha.txt"),
        "line1\nline2\nline3\nline4\nline5\n",
    )
    .unwrap();
    fs::write(repo.join("other/beta.txt"), "hello world\n").unwrap();
    fs::write(repo.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "chore: initial commit"]);

    fs::write(
        repo.join("sub/dir/alpha.txt"),
        "line1\nline2_v2\nline3\nline4\nline5\n",
    )
    .unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "feat: update alpha seenToken"]);

    fs::write(repo.join("other/beta.txt"), "hello world v2\n").unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "fix: update beta"]);

    (tmp, repo)
}

#[test]
fn cli_and_mcp_flag_parity_parse_cleanly() {
    assert!(
        Cli::try_parse_from([
            "oz",
            "log",
            "--pickaxe",
            "foo",
            "--grep-diff",
            "bar",
            "--rev",
            "HEAD",
            "--",
            "src/"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from(["oz", "log", "-n", "10", "--oneline", "--", "tvva-foreman/"]).is_ok()
    );
    assert!(
        Cli::try_parse_from([
            "oz",
            "ws",
            "spawn",
            "pr-1",
            "--backend",
            "cas_hardlink",
            "--from",
            "HEAD",
            "--branch",
            "feat/x"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from([
            "oz",
            "diff",
            "--stat",
            "--exclude",
            "pnpm-lock.yaml",
            "--line-numbers"
        ])
        .is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "status", "--hunks"]).is_ok());
    assert!(
        Cli::try_parse_from([
            "oz",
            "show",
            "HEAD:Cargo.toml",
            "--lines",
            "1..20",
            "--line-numbers"
        ])
        .is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "rev-parse", "--short", "HEAD"]).is_ok());
    assert!(
        Cli::try_parse_from([
            "oz",
            "pr",
            "review",
            "42",
            "--comment",
            "--inline",
            "a.rs:10=ok",
            "--dry-run"
        ])
        .is_ok()
    );
}

#[test]
fn status_collapses_large_hunk_lists_and_diff_handles_excludes_and_line_numbers() {
    let (_tmp, repo) = init_sample_repo();

    // Create a file with 8 separate hunks (>5) to verify collapsing in `oz status`.
    let mut orig = String::new();
    for i in 1..=80 {
        let _ = writeln!(orig, "row {i}");
    }
    fs::write(repo.join("pnpm-lock.yaml"), &orig).unwrap();
    git_ok(&repo, &["add", "pnpm-lock.yaml"]);
    git_ok(&repo, &["commit", "-m", "chore: seed lockfile"]);

    let mut modified_lines: Vec<String> = (1..=80).map(|i| format!("row {i}\n")).collect();
    for idx in [0, 10, 20, 30, 40, 50, 60, 70] {
        modified_lines[idx] = format!("row {} MOD\n", idx + 1);
    }
    fs::write(repo.join("pnpm-lock.yaml"), modified_lines.concat()).unwrap();
    fs::write(
        repo.join("sub/dir/alpha.txt"),
        "line1\nline2_dirty\nline3\nline4\nline5\n",
    )
    .unwrap();

    // Default `oz status` collapses >5 hunks into `(8 hunks: hXa..hXh)`
    let st_compact = run_oz(&repo, &["status"]);
    assert!(st_compact.status.success());
    let st_text = String::from_utf8_lossy(&st_compact.stdout);
    assert!(
        st_text.contains("8 hunks:"),
        "expected collapsed hunks in:\n{st_text}"
    );
    assert!(
        !st_text.contains("h1c, h1d") && !st_text.contains("h2c, h2d"),
        "should not enumerate all 8 hunks:\n{st_text}"
    );

    // `oz status --hunks` lists all hunks inline
    let st_full = run_oz(&repo, &["status", "--hunks"]);
    assert!(st_full.status.success());
    let st_full_text = String::from_utf8_lossy(&st_full.stdout);
    assert!(
        st_full_text.contains("hunks: "),
        "expected full hunk list in:\n{st_full_text}"
    );

    // `oz diff --stat -- ':!pnpm-lock.yaml'` must show alpha.txt (NOT 0 files!)
    let diff_neg = run_oz(&repo, &["diff", "--stat", "--", ":!pnpm-lock.yaml"]);
    assert!(diff_neg.status.success());
    let diff_neg_text = String::from_utf8_lossy(&diff_neg.stdout);
    assert!(
        diff_neg_text.contains("sub/dir/alpha.txt"),
        "expected alpha.txt in:\n{diff_neg_text}"
    );
    assert!(
        !diff_neg_text.contains("pnpm-lock.yaml"),
        "lockfile should be excluded:\n{diff_neg_text}"
    );
    assert!(diff_neg_text.contains("1 file(s) changed"));

    // `oz diff --exclude pnpm-lock.yaml --line-numbers` renders explicit old/new line numbers
    let diff_num = run_oz(
        &repo,
        &["diff", "--exclude", "pnpm-lock.yaml", "--line-numbers"],
    );
    assert!(diff_num.status.success());
    let diff_num_text = String::from_utf8_lossy(&diff_num.stdout);
    assert!(diff_num_text.contains("   2      | -line2_v2"));
    assert!(diff_num_text.contains("        2 | +line2_dirty"));

    // `oz diff --json` includes old_line / new_line fields
    let diff_json = run_oz(&repo, &["diff", "--exclude", "pnpm-lock.yaml", "--json"]);
    assert!(diff_json.status.success());
    let v: Value = serde_json::from_slice(&diff_json.stdout).unwrap();
    let lines = v["data"]["files"][0]["hunks"][0]["lines"]
        .as_array()
        .unwrap();
    assert!(
        lines
            .iter()
            .any(|l| l["kind"] == "add" && l["new_line"] == 2 && l.get("old_line").is_none())
    );
    assert!(
        lines
            .iter()
            .any(|l| l["kind"] == "delete" && l["old_line"] == 2 && l.get("new_line").is_none())
    );

    // Unknown pathspec magic fails loudly
    let bad = run_oz(&repo, &["diff", "--stat", "--", ":(bogus)pnpm-lock.yaml"]);
    assert!(!bad.status.success(), "unknown magic must exit non-zero");
}

#[test]
fn log_paths_aliases_show_blob_rev_parse_and_ws_spawn_backend() {
    let (_tmp, repo) = init_sample_repo();

    // `oz log -n 10 --oneline -- sub/dir/` filters to the 2 commits touching sub/dir/
    let log_dir = run_oz(&repo, &["log", "-n", "10", "--oneline", "--", "sub/dir/"]);
    assert!(
        log_dir.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&log_dir.stderr)
    );
    let log_dir_text = String::from_utf8_lossy(&log_dir.stdout);
    assert!(log_dir_text.contains("update alpha"));
    assert!(log_dir_text.contains("initial commit"));
    assert!(
        !log_dir_text.contains("update beta"),
        "unrelated commit leaked:\n{log_dir_text}"
    );

    // `oz log --pickaxe` & `--rev HEAD`
    let log_pickaxe = run_oz(
        &repo,
        &["log", "--oneline", "--rev", "HEAD", "--pickaxe", "line2_v2"],
    );
    assert!(
        log_pickaxe.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&log_pickaxe.stderr)
    );
    let pickaxe_text = String::from_utf8_lossy(&log_pickaxe.stdout);
    assert!(pickaxe_text.contains("update alpha"));
    assert!(!pickaxe_text.contains("update beta"));

    // `oz show HEAD:sub/dir/alpha.txt --lines 2..4` reads blob without touching working tree
    fs::write(repo.join("sub/dir/alpha.txt"), "LOCAL DIRTY EDIT\n").unwrap();
    let blob_out = run_oz(
        &repo,
        &["show", "HEAD:sub/dir/alpha.txt", "--lines", "2..4"],
    );
    assert!(
        blob_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&blob_out.stderr)
    );
    let blob_text = String::from_utf8_lossy(&blob_out.stdout);
    assert!(blob_text.contains("   2 | line2_v2\n"));
    assert!(blob_text.contains("   4 | line4\n"));
    assert!(!blob_text.contains("LOCAL DIRTY EDIT"));
    assert!(!blob_text.contains("line1"));
    assert!(!blob_text.contains("line5"));

    // `oz rev-parse`
    let rp_top = run_oz(&repo, &["rev-parse", "--show-toplevel"]);
    assert!(rp_top.status.success());
    assert_eq!(
        PathBuf::from(String::from_utf8_lossy(&rp_top.stdout).trim())
            .canonicalize()
            .unwrap(),
        repo.canonicalize().unwrap()
    );

    let rp_inside = run_oz(&repo, &["rev-parse", "--is-inside-work-tree"]);
    assert!(rp_inside.status.success());
    assert_eq!(String::from_utf8_lossy(&rp_inside.stdout).trim(), "true");

    let rp_head = run_oz(&repo, &["rev-parse", "--verify", "HEAD"]);
    assert!(rp_head.status.success());
    let head_sha = String::from_utf8_lossy(&rp_head.stdout).trim().to_string();
    assert_eq!(head_sha.len(), 40);

    let rp_short = run_oz(&repo, &["rev-parse", "--short=10", "HEAD"]);
    assert!(rp_short.status.success());
    assert_eq!(
        String::from_utf8_lossy(&rp_short.stdout).trim(),
        &head_sha[..10]
    );

    let rp_branch = run_oz(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert!(rp_branch.status.success());
    assert_eq!(String::from_utf8_lossy(&rp_branch.stdout).trim(), "main");

    // `oz ws spawn --backend cas_hardlink --from HEAD --branch feat/isolated` reports `clean @ <oid>`
    let ws = run_oz(
        &repo,
        &[
            "ws",
            "spawn",
            "pr-isolated",
            "--backend",
            "cas_hardlink",
            "--from",
            "HEAD",
            "--branch",
            "feat/isolated",
            "--no-warm",
        ],
    );
    assert!(
        ws.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ws.stderr)
    );
    let ws_text = String::from_utf8_lossy(&ws.stdout);
    assert!(
        ws_text.contains("clean @ "),
        "expected clean @ oid in:\n{ws_text}"
    );
    assert!(
        ws_text.contains("cas_hardlink"),
        "expected cas_hardlink in:\n{ws_text}"
    );

    let _ = run_oz(
        &repo,
        &["ws", "rm", "pr-isolated", "--force", "--delete-branch"],
    );
}

#[test]
fn pr_view_diff_read_and_review_against_mock_github_api() {
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

    let posted_reviews: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let posted_reviews_srv = Arc::clone(&posted_reviews);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let api_base = format!("http://{addr}");

    let handle = thread::spawn(move || {
        // Serve up to 12 requests for this test
        for _ in 0..12 {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                continue;
            }
            let parts: Vec<&str> = request_line.split_whitespace().collect();
            let method = parts.first().copied().unwrap_or("GET");
            let path = parts.get(1).copied().unwrap_or("/");

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

            let pr = GitHubPullRequest {
                number: 3293,
                html_url: "https://github.com/acme/demo/pull/3293".to_string(),
                state: "open".to_string(),
                title: "feat: consultation stage".to_string(),
                body: Some("PR description body".to_string()),
                head: GitHubPrRef {
                    ref_name: "feat/consult".to_string(),
                    sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                    label: None,
                },
                base: GitHubPrRef {
                    ref_name: "main".to_string(),
                    sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
                    label: None,
                },
                user: Some(GitHubUser {
                    login: "alice".to_string(),
                }),
                ..Default::default()
            };

            let files = vec![GitHubPullRequestFile {
                filename: "src/index.ts".to_string(),
                previous_filename: None,
                status: "modified".to_string(),
                additions: 2,
                deletions: 1,
                changes: 3,
                sha: None,
                patch: Some(
                    "@@ -396,3 +396,4 @@ export function run() {\n const a = 1;\n-const b = 2;\n+const b = 3;\n+const c = 4;\n return a;"
                        .to_string(),
                ),
            }];

            let (status, content_type, resp_body): (&str, &str, Vec<u8>) = if method == "GET"
                && path == "/repos/acme/demo/pulls/3293"
            {
                (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&pr).unwrap(),
                )
            } else if method == "GET" && path.starts_with("/repos/acme/demo/pulls/3293/files") {
                (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&files).unwrap(),
                )
            } else if method == "GET" && path.starts_with("/repos/acme/demo/pulls/3293/reviews") {
                ("200 OK", "application/json", b"[]".to_vec())
            } else if method == "GET" && path.starts_with("/repos/acme/demo/contents/src/index.ts")
            {
                (
                    "200 OK",
                    "application/octet-stream",
                    b"l1\nl2\nl3\nl4\nl5\n".to_vec(),
                )
            } else if method == "POST" && path == "/repos/acme/demo/pulls/3293/reviews" {
                let val: Value = serde_json::from_slice(&body_bytes).unwrap();
                posted_reviews_srv.lock().unwrap().push(val);
                let rev = GitHubPullRequestReview {
                    id: 777,
                    state: "COMMENTED".to_string(),
                    body: Some("Looks solid".to_string()),
                    html_url: Some(
                        "https://github.com/acme/demo/pull/3293#pullrequestreview-777".to_string(),
                    ),
                    commit_id: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
                    submitted_at: None,
                    user: Some(GitHubUser {
                        login: "reviewer".to_string(),
                    }),
                };
                (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec(&rev).unwrap(),
                )
            } else {
                (
                    "404 Not Found",
                    "application/json",
                    b"{\"message\":\"not found\"}".to_vec(),
                )
            };

            let header = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                resp_body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&resp_body);
            let _ = stream.flush();
        }
    });

    let envs = [
        ("OZ_GITHUB_API_URL", api_base.as_str()),
        ("GITHUB_TOKEN", "test-token-123"),
    ];

    // 1. `oz pr view 3293`
    let view = run_oz_env(&repo, &["pr", "view", "3293"], &envs);
    assert!(
        view.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&view.stderr)
    );
    let view_txt = String::from_utf8_lossy(&view.stdout);
    assert!(view_txt.contains("PR #3293 [open] feat: consultation stage"));
    assert!(view_txt.contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
    assert!(view_txt.contains("src/index.ts"));

    // 2. `oz pr diff 3293` (line numbers enabled by default!)
    let diff = run_oz_env(&repo, &["pr", "diff", "3293"], &envs);
    assert!(
        diff.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&diff.stderr)
    );
    let diff_txt = String::from_utf8_lossy(&diff.stdout);
    assert!(diff_txt.contains(" 396  396 |  const a = 1;"));
    assert!(diff_txt.contains("      398 | +const c = 4;"));

    // 3. `oz pr read 3293 src/index.ts --lines 2..4` (remote GitHub contents fallback)
    let read = run_oz_env(
        &repo,
        &["pr", "read", "3293", "src/index.ts", "--lines", "2..4"],
        &envs,
    );
    assert!(
        read.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&read.stderr)
    );
    let read_txt = String::from_utf8_lossy(&read.stdout);
    assert!(read_txt.contains("   2 | l2\n"));
    assert!(read_txt.contains("   4 | l4\n"));
    assert!(!read_txt.contains("l1"));

    // 4. `oz pr review 3293` with invalid line fails locally BEFORE posting
    let bad_rev = run_oz_env(
        &repo,
        &[
            "pr",
            "review",
            "3293",
            "--comment",
            "--inline",
            "src/index.ts:999=Nope",
        ],
        &envs,
    );
    assert!(!bad_rev.status.success());
    let bad_err = String::from_utf8_lossy(&bad_rev.stderr);
    assert!(
        bad_err.contains("outside PR #3293 diff hunks"),
        "stderr:\n{bad_err}"
    );
    assert!(
        posted_reviews.lock().unwrap().is_empty(),
        "must not POST on invalid line"
    );

    // 5. `oz pr review 3293` with valid inline comment auto-fills commit_id & POSTs
    let ok_rev = run_oz_env(
        &repo,
        &[
            "pr",
            "review",
            "3293",
            "--comment",
            "--body",
            "Looks solid",
            "--inline",
            "src/index.ts:398=Nice addition",
        ],
        &envs,
    );
    assert!(
        ok_rev.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ok_rev.stderr)
    );
    let posted = posted_reviews.lock().unwrap();
    assert_eq!(posted.len(), 1);
    assert_eq!(
        posted[0]["commit_id"],
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );
    assert_eq!(posted[0]["event"], "COMMENT");
    assert_eq!(posted[0]["comments"][0]["path"], "src/index.ts");
    assert_eq!(posted[0]["comments"][0]["line"], 398);
    assert_eq!(posted[0]["comments"][0]["side"], "RIGHT");

    drop(posted);
    let _ = handle.join();
}
