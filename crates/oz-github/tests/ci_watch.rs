// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for `watch_ci_status` and `query_ci_job_log` against a stateful mock
//! GitHub REST API server (ETag/304 caching, startup grace period, fail-fast, wait-all log
//! caching, superseded SHA detection, check filtering, and single-job drill-down).

use oz_core::{CommitOptions, OzoneRepo, commit_changes};
use oz_github::{
    CiJobLogOptions, CiQueryOptions, CiWatchEvent, CiWatchOptions, CiWatchOutcome,
    GitHubActionsJob, GitHubCheckRun, GitHubCheckRunOutput, GitHubJobStep, GitHubPrRef,
    GitHubPullRequest, GitHubWorkflowRun, query_ci_job_log, watch_ci_status,
};
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

struct MockStep {
    etag: String,
    check_runs: Vec<GitHubCheckRun>,
    workflow_runs: Vec<GitHubWorkflowRun>,
    jobs_by_run: HashMap<u64, Vec<GitHubActionsJob>>,
    pr_head_sha: Option<String>,
}

struct MockState {
    steps: Vec<MockStep>,
    check_runs_calls: usize,
    not_modified_responses: usize,
    job_log_downloads: HashMap<u64, usize>,
    job_logs: HashMap<u64, Vec<u8>>,
    jobs_by_id: HashMap<u64, GitHubActionsJob>,
    pr_number: u64,
    pr_branch: String,
}

impl MockState {
    fn current_step(&self) -> &MockStep {
        let idx = self
            .check_runs_calls
            .saturating_sub(1)
            .min(self.steps.len().saturating_sub(1));
        &self.steps[idx]
    }
}

struct MockServer {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(state: MockState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock github");
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
                        handle_http_conn(&mut stream, &st_bg);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            state,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn handle_http_conn(stream: &mut std::net::TcpStream, state: &Arc<Mutex<MockState>>) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    let raw_target = parts[1];
    let path = raw_target.split('?').next().unwrap_or(raw_target);

    let mut headers: HashMap<String, String> = HashMap::new();
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
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            if key == "content-length" {
                content_len = val.parse().unwrap_or(0);
            }
            headers.insert(key, val);
        }
    }
    if content_len > 0 {
        let mut body = vec![0u8; content_len];
        let _ = reader.read_exact(&mut body);
    }

    let inm = headers.get("if-none-match").cloned();
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if segs.len() < 4 || segs[0] != "repos" {
        write_http(stream, "404 Not Found", "application/json", None, b"{}");
        return;
    }
    let tail = &segs[3..];

    let mut st = state.lock().expect("lock mock state");

    // GET /repos/:o/:r/pulls
    if method == "GET" && tail == ["pulls"] {
        let head_sha = st
            .current_step()
            .pr_head_sha
            .clone()
            .unwrap_or_else(|| "0".repeat(40));
        let pr = GitHubPullRequest {
            number: st.pr_number,
            html_url: format!("https://github.com/acme/rocket/pull/{}", st.pr_number),
            state: "open".to_string(),
            draft: false,
            merged: false,
            merged_at: None,
            merge_commit_sha: None,
            title: "feat: watch test".to_string(),
            body: None,
            head: GitHubPrRef {
                ref_name: st.pr_branch.clone(),
                sha: head_sha,
                label: None,
            },
            base: GitHubPrRef {
                ref_name: "main".to_string(),
                sha: "1".repeat(40),
                label: None,
            },
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&vec![pr]).unwrap();
        write_http(stream, "200 OK", "application/json", None, &bytes);
        return;
    }

    // GET /repos/:o/:r/pulls/:num
    if method == "GET" && tail.len() == 2 && tail[0] == "pulls" {
        let head_sha = st
            .current_step()
            .pr_head_sha
            .clone()
            .unwrap_or_else(|| "0".repeat(40));
        let pr = GitHubPullRequest {
            number: st.pr_number,
            html_url: format!("https://github.com/acme/rocket/pull/{}", st.pr_number),
            state: "open".to_string(),
            draft: false,
            merged: false,
            merged_at: None,
            merge_commit_sha: None,
            title: "feat: watch test".to_string(),
            body: None,
            head: GitHubPrRef {
                ref_name: st.pr_branch.clone(),
                sha: head_sha,
                label: None,
            },
            base: GitHubPrRef {
                ref_name: "main".to_string(),
                sha: "1".repeat(40),
                label: None,
            },
            ..Default::default()
        };
        let bytes = serde_json::to_vec(&pr).unwrap();
        write_http(stream, "200 OK", "application/json", None, &bytes);
        return;
    }

    // GET /repos/:o/:r/commits/:ref/check-runs
    if method == "GET" && tail.len() == 3 && tail[0] == "commits" && tail[2] == "check-runs" {
        st.check_runs_calls += 1;
        let step = st.current_step();
        let etag = step.etag.clone();
        if inm.as_deref() == Some(etag.as_str()) {
            st.not_modified_responses += 1;
            write_http(stream, "304 Not Modified", "application/json", Some(&etag), b"");
            return;
        }
        let body = serde_json::to_vec(&json!({
            "total_count": step.check_runs.len(),
            "check_runs": step.check_runs,
        }))
        .unwrap();
        write_http(stream, "200 OK", "application/json", Some(&etag), &body);
        return;
    }

    // GET /repos/:o/:r/actions/runs
    if method == "GET" && tail == ["actions", "runs"] {
        let step = st.current_step();
        let etag = format!("wf-{}", step.etag);
        if inm.as_deref() == Some(etag.as_str()) {
            write_http(stream, "304 Not Modified", "application/json", Some(&etag), b"");
            return;
        }
        let body = serde_json::to_vec(&json!({
            "total_count": step.workflow_runs.len(),
            "workflow_runs": step.workflow_runs,
        }))
        .unwrap();
        write_http(stream, "200 OK", "application/json", Some(&etag), &body);
        return;
    }

    // GET /repos/:o/:r/actions/runs/:run_id/jobs
    if method == "GET" && tail.len() == 4 && tail[0] == "actions" && tail[1] == "runs" && tail[3] == "jobs" {
        let run_id: u64 = tail[2].parse().unwrap_or(0);
        let step = st.current_step();
        let jobs = step.jobs_by_run.get(&run_id).cloned().unwrap_or_default();
        let body = serde_json::to_vec(&json!({
            "total_count": jobs.len(),
            "jobs": jobs,
        }))
        .unwrap();
        write_http(stream, "200 OK", "application/json", None, &body);
        return;
    }

    // GET /repos/:o/:r/actions/jobs/:job_id/logs
    if method == "GET" && tail.len() == 4 && tail[0] == "actions" && tail[1] == "jobs" && tail[3] == "logs" {
        let job_id: u64 = tail[2].parse().unwrap_or(0);
        *st.job_log_downloads.entry(job_id).or_insert(0) += 1;
        let bytes = st.job_logs.get(&job_id).cloned().unwrap_or_default();
        write_http(stream, "200 OK", "text/plain", None, &bytes);
        return;
    }

    // GET /repos/:o/:r/actions/jobs/:job_id
    if method == "GET" && tail.len() == 3 && tail[0] == "actions" && tail[1] == "jobs" {
        let job_id: u64 = tail[2].parse().unwrap_or(0);
        if let Some(job) = st.jobs_by_id.get(&job_id) {
            let bytes = serde_json::to_vec(job).unwrap();
            write_http(stream, "200 OK", "application/json", None, &bytes);
            return;
        }
    }

    write_http(stream, "404 Not Found", "application/json", None, b"{}");
}

fn write_http(
    stream: &mut std::net::TcpStream,
    status: &str,
    content_type: &str,
    etag: Option<&str>,
    body: &[u8],
) {
    let mut hdr = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(tag) = etag {
        hdr.push_str(&format!("ETag: {tag}\r\n"));
    }
    hdr.push_str("\r\n");
    let _ = stream.write_all(hdr.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn init_test_repo(dir: &Path, branch: &str) -> (OzoneRepo, String) {
    let run_git = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {:?} failed", args);
    };
    run_git(&["init", "-b", branch]);
    run_git(&["config", "user.name", "Test"]);
    run_git(&["config", "user.email", "test@example.com"]);
    run_git(&["remote", "add", "origin", "https://github.com/acme/rocket.git"]);
    fs::write(dir.join("lib.rs"), "pub fn ok() -> bool { true }\n").expect("write");
    let repo = OzoneRepo::discover(Some(dir)).expect("discover");
    let outcome = commit_changes(
        &repo,
        &CommitOptions {
            message: "feat: initial".to_string(),
            ..Default::default()
        },
    )
    .expect("commit");
    let repo = OzoneRepo::discover(Some(dir)).expect("rediscover");
    (repo, outcome.commit_oid)
}

fn make_check(id: u64, name: &str, status: &str, conclusion: Option<&str>, run_id: u64, job_id: u64) -> GitHubCheckRun {
    GitHubCheckRun {
        id,
        name: name.to_string(),
        status: status.to_string(),
        conclusion: conclusion.map(str::to_string),
        details_url: Some(format!("https://github.com/acme/rocket/actions/runs/{run_id}/job/{job_id}")),
        html_url: Some(format!("https://github.com/acme/rocket/actions/runs/{run_id}/job/{job_id}")),
        output: GitHubCheckRunOutput::default(),
    }
}

#[test]
fn watch_handles_startup_grace_etag_304_and_eventual_pass() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, sha) = init_test_repo(tmp.path(), "feat/ci-watch");

    let server = MockServer::start(MockState {
        steps: vec![
            // Poll 1: newly pushed commit -> 0 checks yet
            MockStep {
                etag: "\"e1\"".to_string(),
                check_runs: vec![],
                workflow_runs: vec![],
                jobs_by_run: HashMap::new(),
                pr_head_sha: Some(sha.clone()),
            },
            // Poll 2: checks registered and in_progress
            MockStep {
                etag: "\"e2\"".to_string(),
                check_runs: vec![make_check(1, "cargo test", "in_progress", None, 500, 701)],
                workflow_runs: vec![],
                jobs_by_run: HashMap::new(),
                pr_head_sha: Some(sha.clone()),
            },
            // Poll 3: unchanged ETag -> 304 Not Modified!
            MockStep {
                etag: "\"e2\"".to_string(),
                check_runs: vec![make_check(1, "cargo test", "in_progress", None, 500, 701)],
                workflow_runs: vec![],
                jobs_by_run: HashMap::new(),
                pr_head_sha: Some(sha.clone()),
            },
            // Poll 4: passed!
            MockStep {
                etag: "\"e3\"".to_string(),
                check_runs: vec![make_check(1, "cargo test", "completed", Some("success"), 500, 701)],
                workflow_runs: vec![],
                jobs_by_run: HashMap::new(),
                pr_head_sha: Some(sha.clone()),
            },
        ],
        check_runs_calls: 0,
        not_modified_responses: 0,
        job_log_downloads: HashMap::new(),
        job_logs: HashMap::new(),
        jobs_by_id: HashMap::new(),
        pr_number: 42,
        pr_branch: "feat/ci-watch".to_string(),
    });

    let mut events = Vec::new();
    let report = watch_ci_status(
        &repo,
        &CiWatchOptions {
            query: CiQueryOptions {
                auth_token: Some("tok".to_string()),
                api_url: Some(server.base_url()),
                ..Default::default()
            },
            fail_fast: true,
            timeout: Duration::from_secs(5),
            startup_timeout: Duration::from_secs(5),
            poll_interval: Some(Duration::from_millis(15)),
        },
        |ev| events.push(ev),
    )
    .expect("watch_ci_status");

    assert!(report.all_passed);
    assert_eq!(report.overall_status, "passing");
    let w = report.watch.expect("watch metadata");
    assert_eq!(w.outcome, CiWatchOutcome::Passed);
    assert_eq!(w.polls, 4);
    assert!(!w.stopped_early);
    assert_eq!(report.checks[0].job_id, Some(701));
    assert_eq!(report.checks[0].run_id, Some(500));

    let st = server.state.lock().unwrap();
    assert_eq!(st.not_modified_responses, 1, "expected 304 Not Modified on unchanged ETag");
    assert!(events.iter().any(|e| matches!(e, CiWatchEvent::WaitingForChecks { .. })));
    assert!(events.iter().any(|e| matches!(e, CiWatchEvent::Completed { outcome: CiWatchOutcome::Passed, .. })));
}

#[test]
fn watch_fail_fast_exits_early_and_wait_all_caches_failure_logs() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, sha) = init_test_repo(tmp.path(), "feat/fail-fast");

    let job_fail = GitHubActionsJob {
        id: 801,
        run_id: Some(600),
        name: "cargo clippy".to_string(),
        status: "completed".to_string(),
        conclusion: Some("failure".to_string()),
        html_url: Some("https://github.com/acme/rocket/actions/runs/600/job/801".to_string()),
        steps: vec![GitHubJobStep {
            name: "Run clippy".to_string(),
            status: "completed".to_string(),
            conclusion: Some("failure".to_string()),
            number: 3,
        }],
    };
    let raw_log = b"2026-11-18T12:00:00Z ##[group]Run clippy\n\
2026-11-18T12:00:01Z error: unused variable `x`\n\
2026-11-18T12:00:01Z   --> src/lib.rs:9:9\n"
        .to_vec();

    let mut jobs_by_id = HashMap::new();
    jobs_by_id.insert(801, job_fail.clone());
    let mut job_logs = HashMap::new();
    job_logs.insert(801, raw_log.clone());

    let steps = vec![
        MockStep {
            etag: "\"f1\"".to_string(),
            check_runs: vec![
                make_check(1, "cargo clippy", "completed", Some("failure"), 600, 801),
                make_check(2, "macos build", "in_progress", None, 600, 802),
            ],
            workflow_runs: vec![],
            jobs_by_run: HashMap::new(),
            pr_head_sha: Some(sha.clone()),
        },
        MockStep {
            etag: "\"f2\"".to_string(),
            check_runs: vec![
                make_check(1, "cargo clippy", "completed", Some("failure"), 600, 801),
                make_check(2, "macos build", "completed", Some("success"), 600, 802),
            ],
            workflow_runs: vec![],
            jobs_by_run: HashMap::new(),
            pr_head_sha: Some(sha.clone()),
        },
    ];

    // 1. Fail-fast: exits on poll 1 while macos build is still pending!
    let server_ff = MockServer::start(MockState {
        steps: vec![
            MockStep {
                etag: steps[0].etag.clone(),
                check_runs: steps[0].check_runs.clone(),
                workflow_runs: vec![],
                jobs_by_run: HashMap::new(),
                pr_head_sha: Some(sha.clone()),
            },
            MockStep {
                etag: steps[1].etag.clone(),
                check_runs: steps[1].check_runs.clone(),
                workflow_runs: vec![],
                jobs_by_run: HashMap::new(),
                pr_head_sha: Some(sha.clone()),
            },
        ],
        check_runs_calls: 0,
        not_modified_responses: 0,
        job_log_downloads: HashMap::new(),
        job_logs: job_logs.clone(),
        jobs_by_id: jobs_by_id.clone(),
        pr_number: 43,
        pr_branch: "feat/fail-fast".to_string(),
    });

    let ff_report = watch_ci_status(
        &repo,
        &CiWatchOptions {
            query: CiQueryOptions {
                auth_token: Some("tok".to_string()),
                api_url: Some(server_ff.base_url()),
                ..Default::default()
            },
            fail_fast: true,
            timeout: Duration::from_secs(5),
            startup_timeout: Duration::from_secs(5),
            poll_interval: Some(Duration::from_millis(15)),
        },
        |_| {},
    )
    .expect("fail_fast watch");

    let w_ff = ff_report.watch.expect("watch meta");
    assert_eq!(w_ff.outcome, CiWatchOutcome::FailedFast);
    assert!(w_ff.stopped_early);
    assert_eq!(w_ff.polls, 1);
    assert_eq!(ff_report.pending_count, 1);
    assert_eq!(ff_report.failing_count, 1);
    assert!(
        ff_report.checks[0]
            .log_excerpt
            .as_deref()
            .unwrap_or("")
            .contains("unused variable `x`")
    );

    // 2. Wait-all: waits until poll 2 and downloads job 801 logs ONLY ONCE!
    let server_wa = MockServer::start(MockState {
        steps,
        check_runs_calls: 0,
        not_modified_responses: 0,
        job_log_downloads: HashMap::new(),
        job_logs,
        jobs_by_id,
        pr_number: 43,
        pr_branch: "feat/fail-fast".to_string(),
    });

    let wa_report = watch_ci_status(
        &repo,
        &CiWatchOptions {
            query: CiQueryOptions {
                auth_token: Some("tok".to_string()),
                api_url: Some(server_wa.base_url()),
                ..Default::default()
            },
            fail_fast: false,
            timeout: Duration::from_secs(5),
            startup_timeout: Duration::from_secs(5),
            poll_interval: Some(Duration::from_millis(15)),
        },
        |_| {},
    )
    .expect("wait_all watch");

    let w_wa = wa_report.watch.expect("watch meta");
    assert_eq!(w_wa.outcome, CiWatchOutcome::Failed);
    assert!(!w_wa.stopped_early);
    assert_eq!(w_wa.polls, 2);
    assert_eq!(wa_report.pending_count, 0);
    let st = server_wa.state.lock().unwrap();
    assert_eq!(
        st.job_log_downloads.get(&801).copied().unwrap_or(0),
        1,
        "failing job log should be cached across wait-all ticks"
    );
}

#[test]
fn watch_detects_superseded_pr_head_and_supports_job_drilldown_and_filtering() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, sha) = init_test_repo(tmp.path(), "feat/superseded");
    let newer_sha = "b".repeat(40);

    let job = GitHubActionsJob {
        id: 909,
        run_id: Some(777),
        name: "cargo test".to_string(),
        status: "completed".to_string(),
        conclusion: Some("failure".to_string()),
        html_url: Some("https://github.com/acme/rocket/actions/runs/777/job/909".to_string()),
        steps: vec![GitHubJobStep {
            name: "Run tests".to_string(),
            status: "completed".to_string(),
            conclusion: Some("failure".to_string()),
            number: 2,
        }],
    };
    let mut jobs_by_id = HashMap::new();
    jobs_by_id.insert(909, job);
    let mut job_logs = HashMap::new();
    job_logs.insert(
        909,
        b"2026-11-18T12:00:00Z ##[group]Run tests\n\
2026-11-18T12:00:01Z alpha\n\
2026-11-18T12:00:02Z beta\n\
2026-11-18T12:00:03Z test foo ... FAILED\n\
2026-11-18T12:00:04Z tail line\n"
            .to_vec(),
    );

    let server = MockServer::start(MockState {
        steps: vec![
            MockStep {
                etag: "\"s1\"".to_string(),
                check_runs: vec![make_check(1, "cargo test", "in_progress", None, 777, 909)],
                workflow_runs: vec![],
                jobs_by_run: HashMap::new(),
                pr_head_sha: Some(sha.clone()),
            },
            MockStep {
                etag: "\"s2\"".to_string(),
                check_runs: vec![make_check(1, "cargo test", "completed", Some("cancelled"), 777, 909)],
                workflow_runs: vec![GitHubWorkflowRun {
                    id: 778,
                    name: Some("CI".to_string()),
                    head_branch: Some("feat/superseded".to_string()),
                    head_sha: newer_sha.clone(),
                    status: "in_progress".to_string(),
                    conclusion: None,
                    html_url: None,
                }],
                jobs_by_run: HashMap::new(),
                pr_head_sha: Some(newer_sha.clone()),
            },
        ],
        check_runs_calls: 0,
        not_modified_responses: 0,
        job_log_downloads: HashMap::new(),
        job_logs,
        jobs_by_id,
        pr_number: 99,
        pr_branch: "feat/superseded".to_string(),
    });

    let report = watch_ci_status(
        &repo,
        &CiWatchOptions {
            query: CiQueryOptions {
                pr_number: Some(99),
                auth_token: Some("tok".to_string()),
                api_url: Some(server.base_url()),
                ..Default::default()
            },
            fail_fast: true,
            timeout: Duration::from_secs(5),
            startup_timeout: Duration::from_secs(5),
            poll_interval: Some(Duration::from_millis(15)),
        },
        |_| {},
    )
    .expect("watch superseded");

    assert_eq!(report.overall_status, "superseded");
    let w = report.watch.expect("watch meta");
    assert_eq!(w.outcome, CiWatchOutcome::Superseded);
    assert_eq!(w.superseded_by_sha.as_deref(), Some(newer_sha.as_str()));

    // Verify single-job drill-down (`raw: true` tail vs default excerpt).
    let job_raw = query_ci_job_log(
        &repo,
        &CiJobLogOptions {
            job_id: 909,
            step: Some("Run tests".to_string()),
            max_excerpt_lines: 2,
            raw: true,
            remote: None,
            auth_token: Some("tok".to_string()),
            api_url: Some(server.base_url()),
        },
    )
    .expect("query_ci_job_log raw");
    assert_eq!(job_raw.job_id, 909);
    assert_eq!(job_raw.run_id, Some(777));
    assert_eq!(
        job_raw.log_excerpt.as_deref(),
        Some("test foo ... FAILED\ntail line")
    );
}

#[test]
fn watch_returns_no_checks_timed_out_when_no_checks_register() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, sha) = init_test_repo(tmp.path(), "feat/no-ci");

    let server = MockServer::start(MockState {
        steps: vec![MockStep {
            etag: "\"empty\"".to_string(),
            check_runs: vec![],
            workflow_runs: vec![],
            jobs_by_run: HashMap::new(),
            pr_head_sha: Some(sha),
        }],
        check_runs_calls: 0,
        not_modified_responses: 0,
        job_log_downloads: HashMap::new(),
        job_logs: HashMap::new(),
        jobs_by_id: HashMap::new(),
        pr_number: 100,
        pr_branch: "feat/no-ci".to_string(),
    });

    let report = watch_ci_status(
        &repo,
        &CiWatchOptions {
            query: CiQueryOptions {
                auth_token: Some("tok".to_string()),
                api_url: Some(server.base_url()),
                ..Default::default()
            },
            fail_fast: true,
            timeout: Duration::from_secs(5),
            startup_timeout: Duration::from_millis(45),
            poll_interval: Some(Duration::from_millis(15)),
        },
        |_| {},
    )
    .expect("watch no checks");

    let w = report.watch.expect("watch meta");
    assert_eq!(w.outcome, CiWatchOutcome::NoChecksTimedOut);
    assert_eq!(w.recommended_poll_secs, Some(10));
}
