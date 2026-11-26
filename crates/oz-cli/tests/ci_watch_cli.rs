// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end CLI & MCP tests for agent-friendly GitHub CI watching:
//! - `oz ci --watch --json` (pass, fail-fast, flag validation, silent stdout while polling)
//! - `oz ci --job <id> --raw --max-excerpt-lines <N> --json`
//! - `oz push --watch --json` (push to local bare remote + watch pushed SHA)
//! - `oz mcp` stdio session exercising `oz_ci_logs` bounded `wait_seconds` & `job_id` drill-down

#![allow(clippy::too_many_lines)]

use oz_github::{
    GitHubActionsJob, GitHubCheckRun, GitHubCheckRunOutput, GitHubJobStep, GitHubPrRef,
    GitHubPullRequest,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn oz_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oz"))
}

struct MockStep {
    check_runs: Vec<GitHubCheckRun>,
}

struct MockState {
    steps: Vec<MockStep>,
    calls: usize,
    jobs_by_id: HashMap<u64, GitHubActionsJob>,
    job_logs: HashMap<u64, Vec<u8>>,
    pr_branch: String,
    pr_head_sha: String,
}

impl MockState {
    fn current_step(&self) -> &MockStep {
        let idx = self
            .calls
            .saturating_sub(1)
            .min(self.steps.len().saturating_sub(1));
        &self.steps[idx]
    }
}

struct MockGitHub {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockGitHub {
    fn start(state: MockState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("local_addr");
        let state = Arc::new(Mutex::new(state));
        let stop = Arc::new(AtomicBool::new(false));

        let st_bg = Arc::clone(&state);
        let stop_bg = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_bg.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        handle_conn(&mut stream, &st_bg);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for MockGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn handle_conn(stream: &mut std::net::TcpStream, state: &Arc<Mutex<MockState>>) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut req_line = String::new();
    if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
        return;
    }
    let parts: Vec<&str> = req_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    let path = parts[1].split('?').next().unwrap_or(parts[1]);

    let mut content_len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            content_len = v.trim().parse().unwrap_or(0);
        }
    }
    if content_len > 0 {
        let mut body = vec![0u8; content_len];
        let _ = reader.read_exact(&mut body);
    }

    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if segs.len() < 4 || segs[0] != "repos" {
        write_http(stream, "404 Not Found", "application/json", b"{}");
        return;
    }
    let tail = &segs[3..];
    let mut st = state.lock().expect("lock");

    if method == "GET" && tail == ["pulls"] {
        let pr = GitHubPullRequest {
            number: 11,
            html_url: "https://github.com/acme/rocket/pull/11".to_string(),
            state: "open".to_string(),
            draft: false,
            merged: false,
            merged_at: None,
            merge_commit_sha: None,
            title: "feat: watch".to_string(),
            body: None,
            head: GitHubPrRef {
                ref_name: st.pr_branch.clone(),
                sha: st.pr_head_sha.clone(),
                label: None,
            },
            base: GitHubPrRef {
                ref_name: "main".to_string(),
                sha: "1".repeat(40),
                label: None,
            },
            ..Default::default()
        };
        write_http(
            stream,
            "200 OK",
            "application/json",
            &serde_json::to_vec(&vec![pr]).unwrap(),
        );
        return;
    }

    if method == "GET" && tail.len() == 2 && tail[0] == "pulls" {
        let pr = GitHubPullRequest {
            number: 11,
            html_url: "https://github.com/acme/rocket/pull/11".to_string(),
            state: "open".to_string(),
            draft: false,
            merged: false,
            merged_at: None,
            merge_commit_sha: None,
            title: "feat: watch".to_string(),
            body: None,
            head: GitHubPrRef {
                ref_name: st.pr_branch.clone(),
                sha: st.pr_head_sha.clone(),
                label: None,
            },
            base: GitHubPrRef {
                ref_name: "main".to_string(),
                sha: "1".repeat(40),
                label: None,
            },
            ..Default::default()
        };
        write_http(
            stream,
            "200 OK",
            "application/json",
            &serde_json::to_vec(&pr).unwrap(),
        );
        return;
    }

    if method == "GET" && tail.len() == 3 && tail[0] == "commits" && tail[2] == "check-runs" {
        st.calls += 1;
        let step = st.current_step();
        let body = serde_json::to_vec(&json!({
            "total_count": step.check_runs.len(),
            "check_runs": step.check_runs,
        }))
        .unwrap();
        write_http(stream, "200 OK", "application/json", &body);
        return;
    }

    if method == "GET" && tail == ["actions", "runs"] {
        write_http(
            stream,
            "200 OK",
            "application/json",
            br#"{"total_count":0,"workflow_runs":[]}"#,
        );
        return;
    }

    if method == "GET"
        && tail.len() == 4
        && tail[0] == "actions"
        && tail[1] == "jobs"
        && tail[3] == "logs"
    {
        let job_id: u64 = tail[2].parse().unwrap_or(0);
        let bytes = st.job_logs.get(&job_id).cloned().unwrap_or_default();
        write_http(stream, "200 OK", "text/plain", &bytes);
        return;
    }

    if method == "GET" && tail.len() == 3 && tail[0] == "actions" && tail[1] == "jobs" {
        let job_id: u64 = tail[2].parse().unwrap_or(0);
        if let Some(job) = st.jobs_by_id.get(&job_id) {
            write_http(
                stream,
                "200 OK",
                "application/json",
                &serde_json::to_vec(job).unwrap(),
            );
            return;
        }
    }

    write_http(stream, "404 Not Found", "application/json", b"{}");
}

fn write_http(stream: &mut std::net::TcpStream, status: &str, ct: &str, body: &[u8]) {
    let hdr = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(hdr.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(dir: &Path, branch: &str, remote_url: &str) -> String {
    run_git(dir, &["init", "-b", branch]);
    run_git(dir, &["config", "user.name", "Tester"]);
    run_git(dir, &["config", "user.email", "tester@example.com"]);
    run_git(dir, &["remote", "add", "origin", remote_url]);
    fs::write(dir.join("main.rs"), "fn main() {}\n").expect("write");
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-m", "init"]);
    run_git(dir, &["rev-parse", "HEAD"])
}

fn make_check(
    id: u64,
    name: &str,
    status: &str,
    conclusion: Option<&str>,
    run_id: u64,
    job_id: u64,
) -> GitHubCheckRun {
    GitHubCheckRun {
        id,
        name: name.to_string(),
        status: status.to_string(),
        conclusion: conclusion.map(str::to_string),
        details_url: Some(format!(
            "https://github.com/acme/rocket/actions/runs/{run_id}/job/{job_id}"
        )),
        html_url: Some(format!(
            "https://github.com/acme/rocket/actions/runs/{run_id}/job/{job_id}"
        )),
        output: GitHubCheckRunOutput::default(),
    }
}

#[test]
fn cli_ci_watch_pass_fail_fast_job_drilldown_and_flag_validation() {
    let tmp = TempDir::new().expect("tmp");
    let sha = init_repo(
        tmp.path(),
        "feat/watch",
        "https://github.com/acme/rocket.git",
    );

    // 1. Flag validation: --watch + --job -> exit 2
    let out = Command::new(oz_bin())
        .current_dir(tmp.path())
        .args(["ci", "--watch", "--job", "701", "--json"])
        .output()
        .expect("oz ci invalid");
    assert_eq!(out.status.code(), Some(2));

    // 2. Flag validation: --fail-fast + --wait-all -> exit 2
    let out = Command::new(oz_bin())
        .current_dir(tmp.path())
        .args(["ci", "--watch", "--fail-fast", "--wait-all", "--json"])
        .output()
        .expect("oz ci invalid");
    assert_eq!(out.status.code(), Some(2));

    // 3. Flag validation: oz push --delete --watch -> exit 2
    let out = Command::new(oz_bin())
        .current_dir(tmp.path())
        .args(["push", "--delete", "--watch", "--json"])
        .output()
        .expect("oz push invalid");
    assert_eq!(out.status.code(), Some(2));

    // 4. `oz ci --watch --json` fail-fast + `oz ci --job` drill-down
    let mut jobs_by_id = HashMap::new();
    jobs_by_id.insert(
        701,
        GitHubActionsJob {
            id: 701,
            run_id: Some(900),
            name: "cargo test".to_string(),
            status: "completed".to_string(),
            conclusion: Some("failure".to_string()),
            html_url: Some("https://github.com/acme/rocket/actions/runs/900/job/701".to_string()),
            steps: vec![GitHubJobStep {
                name: "Run tests".to_string(),
                status: "completed".to_string(),
                conclusion: Some("failure".to_string()),
                number: 2,
            }],
        },
    );
    let mut job_logs = HashMap::new();
    job_logs.insert(
        701,
        b"2026-11-24T09:00:00Z ##[group]Run tests\n\
2026-11-24T09:00:01Z test watch_it ... FAILED\n\
2026-11-24T09:00:02Z assertion failed: expected 1 == 2\n\
2026-11-24T09:00:03Z final context line\n"
            .to_vec(),
    );

    let mock_fail = MockGitHub::start(MockState {
        steps: vec![
            MockStep {
                check_runs: vec![
                    make_check(1, "cargo test", "in_progress", None, 900, 701),
                    make_check(2, "macos build", "in_progress", None, 900, 702),
                ],
            },
            MockStep {
                check_runs: vec![
                    make_check(1, "cargo test", "completed", Some("failure"), 900, 701),
                    make_check(2, "macos build", "in_progress", None, 900, 702),
                ],
            },
        ],
        calls: 0,
        jobs_by_id,
        job_logs,
        pr_branch: "feat/watch".to_string(),
        pr_head_sha: sha.clone(),
    });

    let out = Command::new(oz_bin())
        .current_dir(tmp.path())
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock_fail.base_url())
        .env("OZ_CI_POLL_INTERVAL_MS", "15")
        .args(["ci", "--watch", "--json"])
        .output()
        .expect("oz ci --watch");
    assert_eq!(out.status.code(), Some(1));
    // Stderr should have zero polling chatter in --json mode without --progress.
    assert!(
        out.stderr.is_empty(),
        "expected quiet stderr in --json mode, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("valid single JSON envelope");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "check_failed");
    assert_eq!(v["data"]["watch"]["outcome"], "failed_fast");
    assert_eq!(v["data"]["watch"]["stopped_early"], true);
    assert_eq!(v["data"]["checks"][0]["job_id"], 701);
    assert_eq!(v["data"]["checks"][0]["run_id"], 900);
    let recoveries = v["recovery_commands"]
        .as_array()
        .expect("recovery_commands");
    assert!(
        recoveries
            .iter()
            .any(|c| c.as_str().is_some_and(|s| s.contains("oz ci --job 701"))),
        "expected recovery command suggesting `oz ci --job 701`, got {recoveries:?}"
    );

    // Drill into job 701 with --raw
    let job_out = Command::new(oz_bin())
        .current_dir(tmp.path())
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock_fail.base_url())
        .args([
            "ci",
            "--job",
            "701",
            "--raw",
            "--max-excerpt-lines",
            "2",
            "--json",
        ])
        .output()
        .expect("oz ci --job");
    assert_eq!(job_out.status.code(), Some(0));
    let jv: Value = serde_json::from_slice(&job_out.stdout).expect("job JSON");
    assert_eq!(jv["ok"], true);
    assert_eq!(jv["data"]["job_id"], 701);
    assert_eq!(jv["data"]["raw"], true);
    assert_eq!(
        jv["data"]["log_excerpt"],
        "assertion failed: expected 1 == 2\nfinal context line"
    );

    // 5. `oz ci --watch --json` passing!
    let mock_pass = MockGitHub::start(MockState {
        steps: vec![
            MockStep { check_runs: vec![] },
            MockStep {
                check_runs: vec![make_check(
                    1,
                    "cargo test",
                    "completed",
                    Some("success"),
                    901,
                    710,
                )],
            },
        ],
        calls: 0,
        jobs_by_id: HashMap::new(),
        job_logs: HashMap::new(),
        pr_branch: "feat/watch".to_string(),
        pr_head_sha: sha,
    });

    let pass_out = Command::new(oz_bin())
        .current_dir(tmp.path())
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock_pass.base_url())
        .env("OZ_CI_POLL_INTERVAL_MS", "15")
        .args(["ci", "--watch", "--json"])
        .output()
        .expect("oz ci --watch pass");
    assert_eq!(pass_out.status.code(), Some(0));
    let pv: Value = serde_json::from_slice(&pass_out.stdout).expect("pass JSON");
    assert_eq!(pv["ok"], true);
    assert_eq!(pv["data"]["all_passed"], true);
    assert_eq!(pv["data"]["watch"]["outcome"], "passed");
}

#[test]
fn mcp_oz_ci_logs_supports_bounded_wait_and_job_drilldown() {
    let tmp = TempDir::new().expect("tmp");
    let sha = init_repo(
        tmp.path(),
        "feat/mcp-watch",
        "https://github.com/acme/rocket.git",
    );

    let mut jobs_by_id = HashMap::new();
    jobs_by_id.insert(
        812,
        GitHubActionsJob {
            id: 812,
            run_id: Some(920),
            name: "clippy".to_string(),
            status: "completed".to_string(),
            conclusion: Some("failure".to_string()),
            html_url: Some("https://github.com/acme/rocket/actions/runs/920/job/812".to_string()),
            steps: vec![GitHubJobStep {
                name: "Clippy".to_string(),
                status: "completed".to_string(),
                conclusion: Some("failure".to_string()),
                number: 1,
            }],
        },
    );
    let mut job_logs = HashMap::new();
    job_logs.insert(
        812,
        b"2026-11-24T09:00:00Z error: needless_return\n".to_vec(),
    );

    let mock = MockGitHub::start(MockState {
        steps: vec![
            MockStep {
                check_runs: vec![make_check(1, "clippy", "in_progress", None, 920, 812)],
            },
            MockStep {
                check_runs: vec![make_check(
                    1,
                    "clippy",
                    "completed",
                    Some("success"),
                    920,
                    812,
                )],
            },
        ],
        calls: 0,
        jobs_by_id,
        job_logs,
        pr_branch: "feat/mcp-watch".to_string(),
        pr_head_sha: sha,
    });

    let mut child = Command::new(oz_bin())
        .current_dir(tmp.path())
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock.base_url())
        .env("OZ_CI_POLL_INTERVAL_MS", "15")
        .args(["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oz mcp");

    let mut stdin = child.stdin.take().unwrap();
    let reqs = vec![
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-03-26", "clientInfo": { "name": "test", "version": "1.0" } }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "oz_ci_logs",
                "arguments": {
                    "directory": tmp.path().to_string_lossy(),
                    "wait_seconds": 5
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "oz_ci_logs",
                "arguments": {
                    "directory": tmp.path().to_string_lossy(),
                    "job_id": 812,
                    "max_excerpt_lines": 50
                }
            }
        }),
    ];
    for r in &reqs {
        writeln!(stdin, "{}", serde_json::to_string(r).unwrap()).unwrap();
    }
    drop(stdin);

    let output = child.wait_with_output().expect("mcp wait");
    assert!(output.status.success(), "oz mcp failed");
    let lines: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("jsonrpc line"))
        .collect();
    assert_eq!(lines.len(), 3);

    let wait_payload: Value =
        serde_json::from_str(lines[1]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(wait_payload["ok"], true);
    assert_eq!(wait_payload["all_passed"], true);
    assert_eq!(wait_payload["watch_outcome"], "passed");
    assert_eq!(wait_payload["checks"][0]["job_id"], 812);

    let job_payload: Value =
        serde_json::from_str(lines[2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(job_payload["ok"], true);
    assert_eq!(job_payload["checks"][0]["job_id"], 812);
    assert!(
        job_payload["checks"][0]["log_excerpt"]
            .as_str()
            .unwrap()
            .contains("needless_return")
    );
}

#[test]
fn cli_push_watch_pushes_and_watches_pushed_sha() {
    let tmp = TempDir::new().expect("tmp");
    let bare_dir = tmp.path().join("acme").join("rocket.git");
    fs::create_dir_all(&bare_dir).expect("mkdir bare");
    run_git(&bare_dir, &["init", "--bare", "-b", "main"]);

    let work_dir = tmp.path().join("work");
    fs::create_dir_all(&work_dir).expect("mkdir work");
    let bare_url = format!("file://{}", bare_dir.display());
    let sha = init_repo(&work_dir, "feat/push-watch", &bare_url);

    let mock = MockGitHub::start(MockState {
        steps: vec![
            MockStep { check_runs: vec![] },
            MockStep {
                check_runs: vec![make_check(
                    1,
                    "ci-build",
                    "completed",
                    Some("success"),
                    950,
                    850,
                )],
            },
        ],
        calls: 0,
        jobs_by_id: HashMap::new(),
        job_logs: HashMap::new(),
        pr_branch: "feat/push-watch".to_string(),
        pr_head_sha: sha.clone(),
    });

    let out = Command::new(oz_bin())
        .current_dir(&work_dir)
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock.base_url())
        .env("OZ_CI_POLL_INTERVAL_MS", "15")
        .args(["push", "--watch", "--json"])
        .output()
        .expect("oz push --watch");
    assert_eq!(
        out.status.code(),
        Some(0),
        "oz push --watch failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("push --watch JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["pushed_refs"][0]["new_oid"], sha);
    assert_eq!(v["data"]["ci"]["all_passed"], true);
    assert_eq!(v["data"]["ci"]["commit_sha"], sha);
    assert_eq!(v["data"]["ci"]["watch"]["outcome"], "passed");
}
