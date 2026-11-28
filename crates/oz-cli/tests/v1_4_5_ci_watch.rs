// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end integration tests for v1.4.5 CI watch, tag peeling, and URL output:
//! 1. `oz ci --workflow Release --watch --rev <tag-or-sha>` waiting past empty polls while an older
//!    run exists, verifying `head_sha` query filtering and no bogus `?branch=` on tags/SHAs.
//! 2. Annotated tag peeling in `oz ci --ref/--rev` and `oz push origin <tag> --watch`, including
//!    waiting past pre-existing completed `main` check runs when pushing a tag.
//! 3. `oz push origin v0.7.0 --watch --workflow Release` end-to-end against a local mock GitHub
//!    server (ignoring pre-existing runs for the same SHA + flag validation).
//! 4. Clickable `html_url` rendering in `oz ci` (passing compacted & failing), `oz ci --runs`,
//!    `oz ci --workflow`, and `oz ci --job` human output.

#![allow(clippy::too_many_lines)]

use oz_github::{
    GitHubActionsJob, GitHubCheckRun, GitHubCheckRunOutput, GitHubJobStep, GitHubWorkflow,
    GitHubWorkflowRun,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn oz_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oz"))
}

#[derive(Debug, Clone)]
struct RecordedRequest {
    path: String,
    query: HashMap<String, String>,
}

#[derive(Default)]
struct MockState {
    workflows: Vec<GitHubWorkflow>,
    /// Stepped responses for `GET /repos/.../actions/workflows/{id}/runs`.
    workflow_run_steps: Vec<Vec<GitHubWorkflowRun>>,
    workflow_run_calls: usize,
    /// Stepped responses for `GET /repos/.../actions/runs`.
    all_runs_steps: Vec<Vec<GitHubWorkflowRun>>,
    all_runs_calls: usize,
    /// Stepped responses for `GET /repos/.../commits/{ref}/check-runs`.
    check_run_steps: Vec<Vec<GitHubCheckRun>>,
    check_run_calls: usize,
    /// Jobs returned for `GET /repos/.../actions/runs/{run_id}/jobs`.
    jobs_by_run: HashMap<u64, Vec<GitHubActionsJob>>,
    /// Individual jobs for `GET /repos/.../actions/jobs/{job_id}`.
    jobs_by_id: HashMap<u64, GitHubActionsJob>,
    /// Logs for `GET /repos/.../actions/jobs/{job_id}/logs`.
    job_logs: HashMap<u64, Vec<u8>>,
    /// Every HTTP GET request received (path + decoded query params).
    requests: Vec<RecordedRequest>,
}

struct MockGitHub {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
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
            state,
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

fn parse_query(raw_query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in raw_query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(k.to_string(), v.to_string());
    }
    map
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
    let (path, query_str) = parts[1].split_once('?').unwrap_or((parts[1], ""));
    let query = parse_query(query_str);

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
    st.requests.push(RecordedRequest {
        path: path.to_string(),
        query: query.clone(),
    });

    if method == "GET" && tail == ["pulls"] {
        write_http(stream, "200 OK", "application/json", b"[]");
        return;
    }

    if method == "GET" && tail == ["actions", "workflows"] {
        let body = serde_json::to_vec(&json!({
            "total_count": st.workflows.len(),
            "workflows": st.workflows,
        }))
        .unwrap();
        write_http(stream, "200 OK", "application/json", &body);
        return;
    }

    if method == "GET"
        && tail.len() == 4
        && tail[0] == "actions"
        && tail[1] == "workflows"
        && tail[3] == "runs"
    {
        st.workflow_run_calls += 1;
        let idx = st
            .workflow_run_calls
            .saturating_sub(1)
            .min(st.workflow_run_steps.len().saturating_sub(1));
        let raw_runs = st.workflow_run_steps.get(idx).cloned().unwrap_or_default();
        // Emulate GitHub's `head_sha` and `branch` query filtering when provided.
        let filtered: Vec<GitHubWorkflowRun> = raw_runs
            .into_iter()
            .filter(|r| {
                query
                    .get("head_sha")
                    .is_none_or(|sha| r.head_sha.eq_ignore_ascii_case(sha))
            })
            .filter(|r| {
                query
                    .get("branch")
                    .is_none_or(|br| r.head_branch.as_deref() == Some(br.as_str()))
            })
            .collect();
        let body = serde_json::to_vec(&json!({
            "total_count": filtered.len(),
            "workflow_runs": filtered,
        }))
        .unwrap();
        write_http(stream, "200 OK", "application/json", &body);
        return;
    }

    if method == "GET" && tail == ["actions", "runs"] {
        st.all_runs_calls += 1;
        let idx = st
            .all_runs_calls
            .saturating_sub(1)
            .min(st.all_runs_steps.len().saturating_sub(1));
        let raw_runs = st.all_runs_steps.get(idx).cloned().unwrap_or_default();
        let filtered: Vec<GitHubWorkflowRun> = raw_runs
            .into_iter()
            .filter(|r| {
                query
                    .get("head_sha")
                    .is_none_or(|sha| r.head_sha.eq_ignore_ascii_case(sha))
            })
            .filter(|r| {
                query
                    .get("branch")
                    .is_none_or(|br| r.head_branch.as_deref() == Some(br.as_str()))
            })
            .collect();
        let body = serde_json::to_vec(&json!({
            "total_count": filtered.len(),
            "workflow_runs": filtered,
        }))
        .unwrap();
        write_http(stream, "200 OK", "application/json", &body);
        return;
    }

    if method == "GET"
        && tail.len() == 4
        && tail[0] == "actions"
        && tail[1] == "runs"
        && tail[3] == "jobs"
    {
        let run_id: u64 = tail[2].parse().unwrap_or(0);
        let jobs = st.jobs_by_run.get(&run_id).cloned().unwrap_or_default();
        let body = serde_json::to_vec(&json!({
            "total_count": jobs.len(),
            "jobs": jobs,
        }))
        .unwrap();
        write_http(stream, "200 OK", "application/json", &body);
        return;
    }

    if method == "GET" && tail.len() == 3 && tail[0] == "commits" && tail[2] == "check-runs" {
        st.check_run_calls += 1;
        let idx = st
            .check_run_calls
            .saturating_sub(1)
            .min(st.check_run_steps.len().saturating_sub(1));
        let runs = st.check_run_steps.get(idx).cloned().unwrap_or_default();
        let body = serde_json::to_vec(&json!({
            "total_count": runs.len(),
            "check_runs": runs,
        }))
        .unwrap();
        write_http(stream, "200 OK", "application/json", &body);
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

fn make_wf_run(
    id: u64,
    name: &str,
    branch: &str,
    head_sha: &str,
    status: &str,
    conclusion: Option<&str>,
) -> GitHubWorkflowRun {
    GitHubWorkflowRun {
        id,
        name: Some(name.to_string()),
        path: Some(".github/workflows/release.yml".to_string()),
        event: Some("push".to_string()),
        head_branch: Some(branch.to_string()),
        head_sha: head_sha.to_string(),
        status: status.to_string(),
        conclusion: conclusion.map(str::to_string),
        html_url: Some(format!("https://github.com/acme/rocket/actions/runs/{id}")),
        created_at: Some("2026-11-28T14:00:00Z".to_string()),
        updated_at: Some("2026-11-28T14:01:00Z".to_string()),
    }
}

fn make_job(
    id: u64,
    run_id: u64,
    name: &str,
    status: &str,
    conclusion: Option<&str>,
) -> GitHubActionsJob {
    GitHubActionsJob {
        id,
        run_id: Some(run_id),
        name: name.to_string(),
        status: status.to_string(),
        conclusion: conclusion.map(str::to_string),
        html_url: Some(format!(
            "https://github.com/acme/rocket/actions/runs/{run_id}/job/{id}"
        )),
        steps: vec![],
    }
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
fn workflow_watch_rev_waits_through_empty_polls_and_ignores_older_run() {
    let tmp = TempDir::new().expect("tmp");
    let old_sha = init_repo(tmp.path(), "main", "https://github.com/acme/rocket.git");
    run_git(tmp.path(), &["tag", "-a", "v0.6.0", "-m", "v0.6.0"]);

    fs::write(
        tmp.path().join("main.rs"),
        "fn main() { println!(\"v0.7.0\"); }\n",
    )
    .expect("write");
    run_git(tmp.path(), &["add", "."]);
    run_git(tmp.path(), &["commit", "-m", "release v0.7.0"]);
    let new_sha = run_git(tmp.path(), &["rev-parse", "HEAD"]);
    run_git(tmp.path(), &["tag", "-a", "v0.7.0", "-m", "Release v0.7.0"]);
    let tag_oid = run_git(tmp.path(), &["rev-parse", "refs/tags/v0.7.0"]);
    assert_ne!(tag_oid, new_sha);

    let stale_old_run = make_wf_run(
        100,
        "Release",
        "v0.6.0",
        &old_sha,
        "completed",
        Some("success"),
    );
    let new_run_running = make_wf_run(200, "Release", "v0.7.0", &new_sha, "in_progress", None);
    let new_run_done = make_wf_run(
        200,
        "Release",
        "v0.7.0",
        &new_sha,
        "completed",
        Some("success"),
    );

    let mut jobs_by_run = HashMap::new();
    jobs_by_run.insert(
        200,
        vec![make_job(
            701,
            200,
            "publish-binaries",
            "completed",
            Some("success"),
        )],
    );

    let mock = MockGitHub::start(MockState {
        workflows: vec![GitHubWorkflow {
            id: 42,
            name: "Release".to_string(),
            path: ".github/workflows/release.yml".to_string(),
            state: "active".to_string(),
        }],
        // Polls 1 & 2: only old v0.6.0 run exists (0 matching runs for v0.7.0!).
        // Poll 3: new run for v0.7.0 appears in_progress.
        // Poll 4: new run completes with success.
        workflow_run_steps: vec![
            vec![stale_old_run.clone()],
            vec![stale_old_run.clone()],
            vec![new_run_running, stale_old_run.clone()],
            vec![new_run_done, stale_old_run],
        ],
        jobs_by_run,
        ..Default::default()
    });

    let out = Command::new(oz_bin())
        .current_dir(tmp.path())
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock.base_url())
        .env("OZ_CI_POLL_INTERVAL_MS", "15")
        .args([
            "ci",
            "--workflow",
            "Release",
            "--watch",
            "--rev",
            "v0.7.0",
            "--progress",
            "--json",
        ])
        .output()
        .expect("oz ci --workflow --watch --rev");

    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let v: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["all_passed"], true);
    assert_eq!(v["data"]["head_sha_filter"], new_sha);
    assert_eq!(v["data"]["runs"][0]["run_id"], 200);
    assert_eq!(v["data"]["runs"][0]["commit_sha"], new_sha);
    assert_eq!(v["data"]["watch"]["outcome"], "passed");
    assert!(
        v["data"]["watch"]["polls"].as_u64().unwrap_or(0) >= 4,
        "expected >= 4 polls through 2 empty polls + in_progress + completed, got {}",
        v["data"]["watch"]["polls"]
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("waiting for checks") && stderr.contains(&new_sha[..10]),
        "expected WaitingForChecks progress with peeled SHA, got stderr={stderr}"
    );

    // Verify HTTP queries passed head_sha=<peeled_commit_sha> and NEVER ?branch=v0.7.0 or tag_oid.
    let st = mock.state.lock().unwrap();
    let wf_reqs: Vec<&RecordedRequest> = st
        .requests
        .iter()
        .filter(|r| r.path.ends_with("/actions/workflows/42/runs"))
        .collect();
    assert!(wf_reqs.len() >= 4);
    for req in wf_reqs {
        assert_eq!(
            req.query.get("head_sha").map(String::as_str),
            Some(new_sha.as_str()),
            "expected head_sha={new_sha}, got {:?}",
            req.query
        );
        assert!(
            !req.query.contains_key("branch"),
            "tag --rev must not send ?branch=, got {:?}",
            req.query
        );
    }
}

#[test]
fn annotated_tag_peeling_in_ci_and_push_watch_waits_for_new_tag_checks() {
    let tmp = TempDir::new().expect("tmp");
    let bare_dir = tmp.path().join("acme").join("rocket.git");
    fs::create_dir_all(&bare_dir).expect("mkdir bare");
    run_git(&bare_dir, &["init", "--bare", "-b", "main"]);

    let work_dir = tmp.path().join("work");
    fs::create_dir_all(&work_dir).expect("mkdir work");
    let bare_url = format!("file://{}", bare_dir.display());
    let commit_sha = init_repo(&work_dir, "main", &bare_url);
    run_git(&work_dir, &["push", "origin", "main"]);
    run_git(&work_dir, &["tag", "-a", "v0.7.0", "-m", "Release v0.7.0"]);
    let tag_oid = run_git(&work_dir, &["rev-parse", "refs/tags/v0.7.0"]);
    assert_ne!(tag_oid, commit_sha);

    // Part A: `oz ci --rev v0.7.0 --json` peels annotated tag to commit_sha.
    let mock_ci = MockGitHub::start(MockState {
        check_run_steps: vec![vec![make_check(
            10,
            "CI",
            "completed",
            Some("success"),
            900,
            701,
        )]],
        ..Default::default()
    });
    let ci_out = Command::new(oz_bin())
        .current_dir(&work_dir)
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock_ci.base_url())
        .args(["ci", "--rev", "v0.7.0", "--json"])
        .output()
        .expect("oz ci --rev v0.7.0");
    assert_eq!(ci_out.status.code(), Some(0));
    let ci_v: Value = serde_json::from_slice(&ci_out.stdout).expect("ci json");
    assert_eq!(ci_v["data"]["commit_sha"], commit_sha);
    {
        let st = mock_ci.state.lock().unwrap();
        let expected_path = format!("/repos/acme/rocket/commits/{commit_sha}/check-runs");
        assert!(
            st.requests.iter().any(|r| r.path == expected_path),
            "expected check-runs query for peeled commit SHA {commit_sha}, got {:?}",
            st.requests
        );
        assert!(
            !st.requests.iter().any(|r| r.path.contains(&tag_oid)),
            "must never query raw annotated tag object OID {tag_oid}"
        );
    }

    // Part B: `oz push origin v0.7.0 --watch --json` where commit ALREADY has completed `CI`
    // check from earlier `main` push. Pre-push snapshot sees check 10; post-push Poll 1 still
    // sees only check 10; Poll 2 sees new `Release` check 20 (in_progress); Poll 3 completes!
    let preexisting_ci = make_check(10, "CI", "completed", Some("success"), 900, 701);
    let release_running = make_check(20, "Release", "in_progress", None, 905, 705);
    let release_done = make_check(20, "Release", "completed", Some("success"), 905, 705);

    let mock_push = MockGitHub::start(MockState {
        // Call 1: pre-push snapshot -> [preexisting_ci]
        // Call 2: post-push poll 1 -> [preexisting_ci] (must NOT exit Passed yet!)
        // Call 3: post-push poll 2 -> [preexisting_ci, release_running]
        // Call 4: post-push poll 3 -> [preexisting_ci, release_done]
        check_run_steps: vec![
            vec![preexisting_ci.clone()],
            vec![preexisting_ci.clone()],
            vec![preexisting_ci.clone(), release_running],
            vec![preexisting_ci, release_done],
        ],
        ..Default::default()
    });

    let push_out = Command::new(oz_bin())
        .current_dir(&work_dir)
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock_push.base_url())
        .env("OZ_CI_POLL_INTERVAL_MS", "15")
        .args(["push", "origin", "v0.7.0", "--watch", "--json"])
        .output()
        .expect("oz push origin v0.7.0 --watch");

    assert_eq!(
        push_out.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&push_out.stdout),
        String::from_utf8_lossy(&push_out.stderr)
    );
    let pv: Value = serde_json::from_slice(&push_out.stdout).expect("push json");
    assert_eq!(pv["ok"], true);
    assert_eq!(
        pv["data"]["pushed_refs"][0]["remote_ref"],
        "refs/tags/v0.7.0"
    );
    assert_eq!(pv["data"]["ci"]["commit_sha"], commit_sha);
    assert_eq!(pv["data"]["ci"]["all_passed"], true);
    assert_eq!(pv["data"]["ci"]["total_count"], 2);
    assert_eq!(pv["data"]["ci"]["watch"]["outcome"], "passed");
    assert!(
        pv["data"]["ci"]["watch"]["polls"].as_u64().unwrap_or(0) >= 3,
        "expected >= 3 watch polls waiting for newly triggered Release check, got {}",
        pv["data"]["ci"]["watch"]["polls"]
    );

    let st = mock_push.state.lock().unwrap();
    assert!(
        !st.requests.iter().any(|r| r.path.contains(&tag_oid)),
        "push --watch must peel annotated tag OID {tag_oid} to commit SHA {commit_sha}"
    );
}

#[test]
fn push_watch_workflow_e2e_and_flag_validation() {
    let tmp = TempDir::new().expect("tmp");
    let bare_dir = tmp.path().join("acme").join("rocket.git");
    fs::create_dir_all(&bare_dir).expect("mkdir bare");
    run_git(&bare_dir, &["init", "--bare", "-b", "main"]);

    let work_dir = tmp.path().join("work");
    fs::create_dir_all(&work_dir).expect("mkdir work");
    let bare_url = format!("file://{}", bare_dir.display());
    let commit_sha = init_repo(&work_dir, "main", &bare_url);
    run_git(&work_dir, &["push", "origin", "main"]);
    run_git(&work_dir, &["tag", "-a", "v0.7.0", "-m", "Release v0.7.0"]);

    // 1. Flag validation: `--workflow` without `--watch` -> exit 2.
    let invalid = Command::new(oz_bin())
        .current_dir(&work_dir)
        .args([
            "push",
            "origin",
            "v0.7.0",
            "--workflow",
            "Release",
            "--json",
        ])
        .output()
        .expect("oz push --workflow without --watch");
    assert_eq!(invalid.status.code(), Some(2));

    // 2. `oz push origin v0.7.0 --watch --workflow Release --json`:
    //    An older completed run 300 already existed on this SHA before the push; must ignore it
    //    and wait for newly created run 301!
    let pre_run = make_wf_run(
        300,
        "Release",
        "v0.7.0-rc1",
        &commit_sha,
        "completed",
        Some("success"),
    );
    let new_run_pending = make_wf_run(301, "Release", "v0.7.0", &commit_sha, "in_progress", None);
    let new_run_done = make_wf_run(
        301,
        "Release",
        "v0.7.0",
        &commit_sha,
        "completed",
        Some("success"),
    );

    let mut jobs_by_run = HashMap::new();
    jobs_by_run.insert(
        301,
        vec![make_job(
            801,
            301,
            "release-build",
            "completed",
            Some("success"),
        )],
    );

    let mock = MockGitHub::start(MockState {
        workflows: vec![GitHubWorkflow {
            id: 77,
            name: "Release".to_string(),
            path: ".github/workflows/release.yml".to_string(),
            state: "active".to_string(),
        }],
        // Call 1: pre-push snapshot -> [pre_run (300)]
        // Call 2: post-push poll 1 -> [pre_run (300)] (ignored!)
        // Call 3: post-push poll 2 -> [new_run_pending (301), pre_run (300)]
        // Call 4: post-push poll 3 -> [new_run_done (301), pre_run (300)]
        workflow_run_steps: vec![
            vec![pre_run.clone()],
            vec![pre_run.clone()],
            vec![new_run_pending, pre_run.clone()],
            vec![new_run_done, pre_run],
        ],
        jobs_by_run,
        ..Default::default()
    });

    let out = Command::new(oz_bin())
        .current_dir(&work_dir)
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock.base_url())
        .env("OZ_CI_POLL_INTERVAL_MS", "15")
        .args([
            "push",
            "origin",
            "v0.7.0",
            "--watch",
            "--workflow",
            "Release",
            "--json",
        ])
        .output()
        .expect("oz push --watch --workflow");

    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["workflow_ci"]["workflow"], "Release");
    assert_eq!(v["data"]["workflow_ci"]["runs"][0]["run_id"], 301);
    assert_eq!(
        v["data"]["workflow_ci"]["runs"][0]["commit_sha"],
        commit_sha
    );
    assert_eq!(
        v["data"]["workflow_ci"]["runs"][0]["html_url"],
        "https://github.com/acme/rocket/actions/runs/301"
    );
    assert_eq!(v["data"]["workflow_ci"]["watch"]["outcome"], "passed");
    assert_eq!(v["data"]["ci"]["all_passed"], true);
    assert_eq!(v["data"]["ci"]["commit_sha"], commit_sha);

    let st = mock.state.lock().unwrap();
    let wf_reqs: Vec<&RecordedRequest> = st
        .requests
        .iter()
        .filter(|r| r.path.ends_with("/actions/workflows/77/runs"))
        .collect();
    assert!(wf_reqs.len() >= 4);
    for req in wf_reqs {
        assert_eq!(
            req.query.get("head_sha").map(String::as_str),
            Some(commit_sha.as_str())
        );
        assert!(
            !req.query.contains_key("branch"),
            "tag push --workflow must not pass ?branch="
        );
    }

    // 3. Human output of `oz push origin v0.7.1 --watch --workflow Release` prints clickable URL.
    run_git(&work_dir, &["tag", "-a", "v0.7.1", "-m", "Release v0.7.1"]);
    let mock_human = MockGitHub::start(MockState {
        workflows: vec![GitHubWorkflow {
            id: 77,
            name: "Release".to_string(),
            path: ".github/workflows/release.yml".to_string(),
            state: "active".to_string(),
        }],
        workflow_run_steps: vec![
            vec![],
            vec![make_wf_run(
                302,
                "Release",
                "v0.7.1",
                &commit_sha,
                "completed",
                Some("success"),
            )],
        ],
        ..Default::default()
    });
    let human_out = Command::new(oz_bin())
        .current_dir(&work_dir)
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock_human.base_url())
        .env("OZ_CI_POLL_INTERVAL_MS", "15")
        .args([
            "push",
            "origin",
            "v0.7.1",
            "--watch",
            "--workflow",
            "Release",
        ])
        .output()
        .expect("oz push --watch --workflow human");
    assert_eq!(human_out.status.code(), Some(0));
    let human_stdout = String::from_utf8_lossy(&human_out.stdout);
    assert!(
        human_stdout.contains("https://github.com/acme/rocket/actions/runs/302"),
        "expected clickable run URL in push --watch --workflow human output, got:\n{human_stdout}"
    );
}

#[test]
fn human_output_renders_clickable_urls_for_ci_runs_workflow_and_job() {
    let tmp = TempDir::new().expect("tmp");
    let sha = init_repo(tmp.path(), "main", "https://github.com/acme/rocket.git");

    // 1. Passing `oz ci` (compacted!) must still render clickable run URL.
    let mock_pass = MockGitHub::start(MockState {
        check_run_steps: vec![vec![make_check(
            1,
            "cargo test",
            "completed",
            Some("success"),
            901,
            710,
        )]],
        ..Default::default()
    });
    let out_pass = Command::new(oz_bin())
        .current_dir(tmp.path())
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock_pass.base_url())
        .args(["ci"])
        .output()
        .expect("oz ci passing");
    assert_eq!(out_pass.status.code(), Some(0));
    let s_pass = String::from_utf8_lossy(&out_pass.stdout);
    assert!(
        s_pass.contains("url: https://github.com/acme/rocket/actions/runs/901"),
        "passing compacted `oz ci` must still show clickable URL, got:\n{s_pass}"
    );

    // 2. Failing `oz ci` renders both run URL and failing job URL.
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
                number: 1,
            }],
        },
    );
    let mut job_logs = HashMap::new();
    job_logs.insert(701, b"2026-11-28T14:00:00Z error: test boom\n".to_vec());

    let mut jobs_by_run = HashMap::new();
    jobs_by_run.insert(
        501,
        vec![GitHubActionsJob {
            id: 701,
            run_id: Some(501),
            name: "cargo test".to_string(),
            status: "completed".to_string(),
            conclusion: Some("failure".to_string()),
            html_url: Some("https://github.com/acme/rocket/actions/runs/501/job/701".to_string()),
            steps: vec![],
        }],
    );

    let mock_all = MockGitHub::start(MockState {
        workflows: vec![GitHubWorkflow {
            id: 42,
            name: "Release".to_string(),
            path: ".github/workflows/release.yml".to_string(),
            state: "active".to_string(),
        }],
        check_run_steps: vec![vec![make_check(
            1,
            "cargo test",
            "completed",
            Some("failure"),
            900,
            701,
        )]],
        all_runs_steps: vec![vec![make_wf_run(
            501,
            "Release",
            "main",
            &sha,
            "completed",
            Some("failure"),
        )]],
        workflow_run_steps: vec![vec![make_wf_run(
            501,
            "Release",
            "main",
            &sha,
            "completed",
            Some("failure"),
        )]],
        jobs_by_run,
        jobs_by_id,
        job_logs,
        ..Default::default()
    });

    // 2a. Failing `oz ci`
    let out_fail = Command::new(oz_bin())
        .current_dir(tmp.path())
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock_all.base_url())
        .args(["ci"])
        .output()
        .expect("oz ci failing");
    assert_eq!(out_fail.status.code(), Some(1));
    let s_fail = String::from_utf8_lossy(&out_fail.stdout);
    assert!(
        s_fail.contains("https://github.com/acme/rocket/actions/runs/900/job/701"),
        "failing `oz ci` must print job URL, got:\n{s_fail}"
    );

    // 2b. `oz ci --runs`
    let out_runs = Command::new(oz_bin())
        .current_dir(tmp.path())
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock_all.base_url())
        .args(["ci", "--runs"])
        .output()
        .expect("oz ci --runs");
    assert_eq!(out_runs.status.code(), Some(0));
    let s_runs = String::from_utf8_lossy(&out_runs.stdout);
    assert!(
        s_runs.contains("url: https://github.com/acme/rocket/actions/runs/501"),
        "`oz ci --runs` must print run URL, got:\n{s_runs}"
    );

    // 2c. `oz ci --workflow Release`
    let out_wf = Command::new(oz_bin())
        .current_dir(tmp.path())
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock_all.base_url())
        .args(["ci", "--workflow", "Release"])
        .output()
        .expect("oz ci --workflow");
    assert_eq!(out_wf.status.code(), Some(1));
    let s_wf = String::from_utf8_lossy(&out_wf.stdout);
    assert!(
        s_wf.contains("url: https://github.com/acme/rocket/actions/runs/501"),
        "`oz ci --workflow` must print run URL, got:\n{s_wf}"
    );
    assert!(
        s_wf.contains("url: https://github.com/acme/rocket/actions/runs/501/job/701"),
        "`oz ci --workflow` must print failing job URL, got:\n{s_wf}"
    );

    // 2d. `oz ci --job 701`
    let out_job = Command::new(oz_bin())
        .current_dir(tmp.path())
        .env("GITHUB_TOKEN", "test-tok")
        .env("OZ_GITHUB_API_URL", mock_all.base_url())
        .args(["ci", "--job", "701"])
        .output()
        .expect("oz ci --job");
    assert_eq!(out_job.status.code(), Some(0));
    let s_job = String::from_utf8_lossy(&out_job.stdout);
    assert!(
        s_job.contains("url: https://github.com/acme/rocket/actions/runs/900/job/701"),
        "`oz ci --job` must print job URL, got:\n{s_job}"
    );
}
