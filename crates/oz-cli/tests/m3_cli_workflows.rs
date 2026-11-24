// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Milestone 3 end-to-end CLI workflow tests covering:
//! - `oz commit --sign` + `git verify-commit` and `oz commit --amend` preserving `Change-Id:`
//! - `oz commit --agent` / ambient agent env auto-recording `refs/notes/ai` + `oz blame`
//! - `oz clone --filter=blob:none`, `oz fetch --prune` + `oz undo`, and `oz push --atomic --force-with-lease` (including stale-lease exit code `1`)
//! - `oz pr create`, `oz pr stack`, `oz sync`, and `oz ci` against a local `tokio` mock GitHub API server (`OZ_GITHUB_API_URL`)
//! - `oz agent setup --all` + `--check`
//! - `oz mcp` binary stdio session exercising `initialize`, `tools/list`, `oz_status`, `oz_commit`, and live `oz_ci_logs` (`oz-github` `CiLogProvider`)
//! - `oz doctor --json` reporting `ssh_signing` and `github_auth`

use oz_agent::{AiProvenanceNote, OZ_CHECKPOINT_HOOK_CMD};
use oz_cli::run_with_args;
use oz_core::{ExitCode, OZ_SCHEMA_V1};
use oz_github::{
    GitHubActionsJob, GitHubCheckRun, GitHubCheckRunOutput, GitHubJobStep, GitHubPrRef,
    GitHubPullRequest, GitHubWorkflowRun, OZ_STACK_END, OZ_STACK_START,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader as StdBufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

fn oz_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oz"))
}

struct CmdOut {
    code: i32,
    stdout: String,
    stderr: String,
    json: Option<Value>,
}

impl CmdOut {
    fn unwrap_json(&self) -> &Value {
        self.json.as_ref().unwrap_or_else(|| {
            panic!(
                "expected valid JSON on stdout (exit={}):\nstdout: {}\nstderr: {}",
                self.code, self.stdout, self.stderr
            )
        })
    }
}

fn is_ambient_agent_or_auth_env(key: &str) -> bool {
    key == "OZ_AGENT"
        || key.starts_with("OZ_AGENT_")
        || key == "CLAUDECODE"
        || key.starts_with("CLAUDE_")
        || key == "ANTHROPIC_MODEL"
        || key.starts_with("CURSOR_")
        || key.starts_with("CODEX_")
        || key == "OPENAI_CODEX"
        || key.starts_with("WINDSURF_")
        || key == "CODEIUM_WINDSURF"
        || key.starts_with("ANTIGRAVITY_")
        || key.starts_with("AGY_")
        || key == "OZ_GITHUB_TOKEN"
        || key == "GITHUB_TOKEN"
        || key == "GH_TOKEN"
        || key == "GH_CONFIG_DIR"
        || key == "OZ_GITHUB_API_URL"
        || key == "CI"
}

fn build_clean_oz_command(cwd: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(oz_bin());
    cmd.args(args).current_dir(cwd).stdin(Stdio::null());
    for (k, _) in std::env::vars() {
        if is_ambient_agent_or_auth_env(&k) {
            cmd.env_remove(k);
        }
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd
}

fn run_oz(cwd: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> CmdOut {
    let mut cmd = build_clean_oz_command(cwd, args, extra_env);
    let out = cmd.output().expect("execute oz");
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let json = serde_json::from_str::<Value>(&stdout).ok();
    CmdOut {
        code,
        stdout,
        stderr,
        json,
    }
}

fn sys_git_bin() -> &'static str {
    if Path::new("/usr/bin/git").is_file() {
        "/usr/bin/git"
    } else {
        "git"
    }
}

fn ssh_keygen_bin() -> &'static str {
    if Path::new("/usr/bin/ssh-keygen").is_file() {
        "/usr/bin/ssh-keygen"
    } else {
        "ssh-keygen"
    }
}

fn sys_git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new(sys_git_bin())
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("execute git");
    assert!(
        out.status.success(),
        "git {:?} failed in {}:\nstdout: {}\nstderr: {}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_ozone_repo(dir: &Path) {
    fs::create_dir_all(dir).expect("mkdir repo");
    sys_git(dir, &["init", "-b", "main"]);
    sys_git(dir, &["config", "user.name", "Ozone M3 Dev"]);
    sys_git(dir, &["config", "user.email", "dev@ozone.test"]);
    fs::write(dir.join(".ozone.toml"), "backend = \"cas_hardlink\"\n").expect("write .ozone.toml");
    fs::write(dir.join("README.md"), "# Ozone M3 Test\n").expect("write readme");
    sys_git(dir, &["add", "."]);
    sys_git(dir, &["commit", "-m", "chore: initial commit"]);
}

// ============================================================================
// Mock GitHub REST + GraphQL Server
// ============================================================================

#[derive(Default)]
struct MockGitHubState {
    expected_token: String,
    next_pr_number: u64,
    prs: Vec<GitHubPullRequest>,
    check_runs_by_ref: HashMap<String, Vec<GitHubCheckRun>>,
    workflow_runs: Vec<GitHubWorkflowRun>,
    jobs_by_run: HashMap<u64, Vec<GitHubActionsJob>>,
    job_logs: HashMap<u64, Vec<u8>>,
    bare_repo: Option<PathBuf>,
}

impl MockGitHubState {
    fn new(expected_token: &str, bare_repo: Option<PathBuf>) -> Self {
        Self {
            expected_token: expected_token.to_string(),
            next_pr_number: 1,
            prs: Vec::new(),
            check_runs_by_ref: HashMap::new(),
            workflow_runs: Vec::new(),
            jobs_by_run: HashMap::new(),
            job_logs: HashMap::new(),
            bare_repo,
        }
    }

    fn resolve_branch_sha(&self, branch: &str) -> String {
        if let Some(ref bare) = self.bare_repo
            && let Ok(out) = Command::new(sys_git_bin())
                .args(["rev-parse", &format!("refs/heads/{branch}")])
                .current_dir(bare)
                .output()
            && out.status.success()
        {
            return String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
        "0000000000000000000000000000000000000000".to_string()
    }
}

async fn spawn_mock_github_api(state: Arc<Mutex<MockGitHubState>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock github api");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let st = Arc::clone(&state);
            tokio::spawn(async move {
                let _ = handle_mock_github_conn(stream, st).await;
            });
        }
    });

    format!("http://{addr}")
}

async fn handle_mock_github_conn(
    stream: TcpStream,
    state: Arc<Mutex<MockGitHubState>>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut req_line = String::new();
    if reader.read_line(&mut req_line).await? == 0 {
        return Ok(());
    }
    let parts: Vec<&str> = req_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(());
    }
    let method = parts[0].to_string();
    let raw_target = parts[1].to_string();

    let mut headers: HashMap<String, String> = HashMap::new();
    loop {
        let mut hline = String::new();
        let n = reader.read_line(&mut hline).await?;
        if n == 0 || hline == "\r\n" || hline == "\n" {
            break;
        }
        if let Some((k, v)) = hline.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let body = if let Some(len) = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        && len > 0
    {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await?;
        buf
    } else {
        Vec::new()
    };

    let (path, query) = raw_target
        .split_once('?')
        .map(|(p, q)| (p, Some(q)))
        .unwrap_or((raw_target.as_str(), None));
    let query_map = parse_query_map(query);

    let (status, content_type, resp_bytes) = {
        let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
        let expected_auth = format!("Bearer {}", guard.expected_token);
        if headers.get("authorization") != Some(&expected_auth) {
            (
                "401 Unauthorized",
                "application/json",
                serde_json::to_vec(&json!({ "message": "Bad credentials" })).unwrap(),
            )
        } else {
            route_mock_github(&method, path, &query_map, &body, &mut guard)
        }
    };

    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        resp_bytes.len()
    );
    write_half.write_all(header.as_bytes()).await?;
    write_half.write_all(&resp_bytes).await?;
    write_half.flush().await
}

fn route_mock_github(
    method: &str,
    path: &str,
    query: &HashMap<String, String>,
    body: &[u8],
    st: &mut MockGitHubState,
) -> (&'static str, &'static str, Vec<u8>) {
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();

    if method == "POST" && segs.as_slice() == ["graphql"] {
        return (
            "200 OK",
            "application/json",
            serde_json::to_vec(&json!({ "data": { "viewer": { "login": "ozone-bot" } } })).unwrap(),
        );
    }

    if segs.len() >= 4 && segs[0] == "repos" {
        let owner = segs[1];
        let repo = segs[2];
        let tail = &segs[3..];

        if method == "GET" && tail == ["pulls"] {
            let state_filter = query.get("state").map(String::as_str).unwrap_or("open");
            let head_filter = query.get("head").map(String::as_str);
            let base_filter = query.get("base").map(String::as_str);

            let filtered: Vec<&GitHubPullRequest> = st
                .prs
                .iter()
                .filter(|pr| {
                    (state_filter == "all" || pr.state == state_filter)
                        && head_filter.is_none_or(|hf| {
                            let short = hf.split_once(':').map(|(_, b)| b).unwrap_or(hf);
                            pr.head.ref_name == short
                        })
                        && base_filter.is_none_or(|bf| pr.base.ref_name == bf)
                })
                .collect();
            return (
                "200 OK",
                "application/json",
                serde_json::to_vec(&filtered).unwrap(),
            );
        }

        if method == "POST" && tail == ["pulls"] {
            let val: Value = serde_json::from_slice(body).unwrap_or_default();
            let head_branch = val["head"].as_str().unwrap_or_default().to_string();
            let base_branch = val["base"].as_str().unwrap_or_default().to_string();
            let num = st.next_pr_number;
            st.next_pr_number += 1;
            let head_sha = st.resolve_branch_sha(&head_branch);
            let base_sha = st.resolve_branch_sha(&base_branch);

            let pr = GitHubPullRequest {
                number: num,
                html_url: format!("https://github.com/{owner}/{repo}/pull/{num}"),
                state: "open".to_string(),
                draft: val["draft"].as_bool().unwrap_or(false),
                merged: false,
                merged_at: None,
                merge_commit_sha: None,
                title: val["title"].as_str().unwrap_or_default().to_string(),
                body: val["body"].as_str().map(str::to_string),
                head: GitHubPrRef {
                    ref_name: head_branch.clone(),
                    sha: head_sha,
                    label: Some(format!("{owner}:{head_branch}")),
                },
                base: GitHubPrRef {
                    ref_name: base_branch.clone(),
                    sha: base_sha,
                    label: Some(format!("{owner}:{base_branch}")),
                },
                ..Default::default()
            };
            st.prs.push(pr.clone());
            return (
                "201 Created",
                "application/json",
                serde_json::to_vec(&pr).unwrap(),
            );
        }

        if tail.len() == 2
            && tail[0] == "pulls"
            && let Ok(num) = tail[1].parse::<u64>()
        {
            if method == "GET" {
                if let Some(pr) = st.prs.iter().find(|p| p.number == num) {
                    return (
                        "200 OK",
                        "application/json",
                        serde_json::to_vec(pr).unwrap(),
                    );
                }
            } else if method == "PATCH" {
                let val: Value = serde_json::from_slice(body).unwrap_or_default();
                let new_base_sha = val["base"].as_str().map(|b| st.resolve_branch_sha(b));
                if let Some(pr) = st.prs.iter_mut().find(|p| p.number == num) {
                    if let Some(t) = val["title"].as_str() {
                        pr.title = t.to_string();
                    }
                    if let Some(b) = val["body"].as_str() {
                        pr.body = Some(b.to_string());
                    }
                    if let Some(base_str) = val["base"].as_str() {
                        pr.base.ref_name = base_str.to_string();
                        if let Some(sha) = new_base_sha {
                            pr.base.sha = sha;
                        }
                    }
                    if let Some(s) = val["state"].as_str() {
                        pr.state = s.to_string();
                    }
                    return (
                        "200 OK",
                        "application/json",
                        serde_json::to_vec(pr).unwrap(),
                    );
                }
            }
        }

        if method == "GET" && tail.len() == 3 && tail[0] == "commits" && tail[2] == "check-runs" {
            let git_ref = tail[1];
            let runs = st
                .check_runs_by_ref
                .get(git_ref)
                .cloned()
                .unwrap_or_default();
            return (
                "200 OK",
                "application/json",
                serde_json::to_vec(&json!({
                    "total_count": runs.len(),
                    "check_runs": runs,
                }))
                .unwrap(),
            );
        }

        if method == "GET" && tail == ["actions", "runs"] {
            let head_sha = query.get("head_sha").map(String::as_str);
            let runs: Vec<&GitHubWorkflowRun> = st
                .workflow_runs
                .iter()
                .filter(|r| head_sha.is_none_or(|s| r.head_sha == s))
                .collect();
            return (
                "200 OK",
                "application/json",
                serde_json::to_vec(&json!({
                    "total_count": runs.len(),
                    "workflow_runs": runs,
                }))
                .unwrap(),
            );
        }

        if method == "GET"
            && tail.len() == 4
            && tail[0] == "actions"
            && tail[1] == "runs"
            && tail[3] == "jobs"
            && let Ok(run_id) = tail[2].parse::<u64>()
        {
            let jobs = st.jobs_by_run.get(&run_id).cloned().unwrap_or_default();
            return (
                "200 OK",
                "application/json",
                serde_json::to_vec(&json!({
                    "total_count": jobs.len(),
                    "jobs": jobs,
                }))
                .unwrap(),
            );
        }

        if method == "GET"
            && tail.len() == 3
            && tail[0] == "actions"
            && tail[1] == "jobs"
            && let Ok(job_id) = tail[2].parse::<u64>()
            && let Some(job) = st.jobs_by_run.values().flatten().find(|j| j.id == job_id)
        {
            return (
                "200 OK",
                "application/json",
                serde_json::to_vec(job).unwrap(),
            );
        }

        if method == "GET"
            && tail.len() == 4
            && tail[0] == "actions"
            && tail[1] == "jobs"
            && tail[3] == "logs"
            && let Ok(job_id) = tail[2].parse::<u64>()
            && let Some(bytes) = st.job_logs.get(&job_id)
        {
            return ("200 OK", "text/plain; charset=utf-8", bytes.clone());
        }
    }

    (
        "404 Not Found",
        "application/json",
        serde_json::to_vec(&json!({ "message": format!("not found: {method} {path}") })).unwrap(),
    )
}

fn parse_query_map(query: Option<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(q) = query else {
        return map;
    };
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(percent_decode(k), percent_decode(v));
        }
    }
    map
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex_str) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(val) = u8::from_str_radix(hex_str, 16)
        {
            out.push(val);
            i += 3;
            continue;
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ============================================================================
// Test 1: `oz commit --sign` + `git verify-commit` & `oz commit --amend`
// ============================================================================

#[test]
fn commit_sign_verify_commit_and_amend_preserves_change_id() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    init_ozone_repo(&repo_dir);

    let key_path = tmp.path().join("id_ed25519_test");
    let keygen = Command::new(ssh_keygen_bin())
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "dev@ozone.test",
            "-f",
            key_path.to_str().unwrap(),
        ])
        .output()
        .expect("ssh-keygen");
    assert!(keygen.status.success(), "ssh-keygen failed");

    let pub_key = fs::read_to_string(key_path.with_extension("pub")).expect("read pub key");
    let allowed_signers = tmp.path().join("allowed_signers");
    fs::write(
        &allowed_signers,
        format!("dev@ozone.test {}\n", pub_key.trim()),
    )
    .expect("write allowed_signers");

    sys_git(&repo_dir, &["config", "gpg.format", "ssh"]);
    sys_git(
        &repo_dir,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            allowed_signers.to_str().unwrap(),
        ],
    );

    // 1. Signed commit via `oz commit --sign --signing-key <key>`
    fs::write(repo_dir.join("auth.rs"), "pub fn login() -> bool {\n    true\n}\n").unwrap();
    let c1 = run_oz(
        &repo_dir,
        &[
            "commit",
            "--sign",
            "--signing-key",
            key_path.to_str().unwrap(),
            "-m",
            "feat: signed auth",
            "--json",
        ],
        &[],
    );
    assert_eq!(c1.code, 0, "stderr: {}", c1.stderr);
    let c1_json = c1.unwrap_json();
    assert_eq!(c1_json["schema"], OZ_SCHEMA_V1);
    assert_eq!(c1_json["ok"], true);
    assert_eq!(c1_json["data"]["signed"], true);
    assert_eq!(c1_json["data"]["amended"], false);
    assert!(
        c1_json["data"]["signature_fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("SHA256:")
    );
    let original_change_id = c1_json["data"]["change_id"].as_str().unwrap().to_string();
    let c1_oid = c1_json["data"]["commit_oid"].as_str().unwrap().to_string();

    // Verify with `git verify-commit`!
    let verify1 = Command::new(sys_git_bin())
        .args(["verify-commit", &c1_oid])
        .current_dir(&repo_dir)
        .output()
        .expect("git verify-commit");
    assert!(
        verify1.status.success(),
        "git verify-commit failed: {}",
        String::from_utf8_lossy(&verify1.stderr)
    );

    // 2. Amend with `-S` + `-m`, verifying `Change-Id:` is preserved and commit count does not grow!
    fs::write(
        repo_dir.join("auth.rs"),
        "pub fn login() -> bool {\n    true\n}\npub fn logout() {}\n",
    )
    .unwrap();
    let c2 = run_oz(
        &repo_dir,
        &[
            "commit",
            "--amend",
            "-S",
            "--signing-key",
            key_path.to_str().unwrap(),
            "-m",
            "feat: signed auth with logout",
            "--json",
        ],
        &[],
    );
    assert_eq!(c2.code, 0, "stderr: {}", c2.stderr);
    let c2_json = c2.unwrap_json();
    assert_eq!(c2_json["data"]["amended"], true);
    assert_eq!(c2_json["data"]["signed"], true);
    assert_eq!(c2_json["data"]["change_id"], original_change_id);
    let c2_oid = c2_json["data"]["commit_oid"].as_str().unwrap().to_string();
    assert_ne!(c1_oid, c2_oid);
    assert_eq!(sys_git(&repo_dir, &["rev-list", "--count", "HEAD"]), "2");

    // 3. Amend without `-m` (`--no-edit`) reusing HEAD's message and preserving `Change-Id:`!
    fs::write(
        repo_dir.join("auth.rs"),
        "pub fn login() -> bool {\n    true\n}\npub fn logout() {}\npub fn refresh() {}\n",
    )
    .unwrap();
    let c3 = run_oz(
        &repo_dir,
        &[
            "commit",
            "--amend",
            "--no-edit",
            "-S",
            "--signing-key",
            key_path.to_str().unwrap(),
            "--json",
        ],
        &[],
    );
    assert_eq!(c3.code, 0, "stderr: {}", c3.stderr);
    let c3_json = c3.unwrap_json();
    assert_eq!(c3_json["data"]["amended"], true);
    assert_eq!(c3_json["data"]["change_id"], original_change_id);
    let c3_oid = c3_json["data"]["commit_oid"].as_str().unwrap().to_string();
    assert_eq!(sys_git(&repo_dir, &["rev-list", "--count", "HEAD"]), "2");
    assert!(
        c3_json["data"]["message"]
            .as_str()
            .unwrap()
            .contains("feat: signed auth with logout")
    );

    let verify3 = Command::new(sys_git_bin())
        .args(["verify-commit", &c3_oid])
        .current_dir(&repo_dir)
        .output()
        .expect("git verify-commit amended");
    assert!(
        verify3.status.success(),
        "git verify-commit failed after amend: {}",
        String::from_utf8_lossy(&verify3.stderr)
    );
    sys_git(&repo_dir, &["fsck", "--full"]);
}

// ============================================================================
// Test 2: `oz commit --agent` `refs/notes/ai` + `oz blame <file>`
// ============================================================================

#[test]
fn commit_agent_auto_records_ai_notes_and_blame_reports_human_vs_agent() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    init_ozone_repo(&repo_dir);

    // 1. Human commit (clean env, no `--agent`).
    fs::write(
        repo_dir.join("math.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .unwrap();
    let human_c = run_oz(
        &repo_dir,
        &["commit", "-m", "feat: add function", "--json"],
        &[],
    );
    assert_eq!(human_c.code, 0, "stderr: {}", human_c.stderr);
    let human_oid = human_c.unwrap_json()["data"]["commit_oid"]
        .as_str()
        .unwrap()
        .to_string();

    // Human commit must NOT create a `refs/notes/ai` entry.
    let human_note = Command::new(sys_git_bin())
        .args(["notes", "--ref=ai", "show", &human_oid])
        .current_dir(&repo_dir)
        .output()
        .unwrap();
    assert!(!human_note.status.success());

    // 2. Explicit `--agent` commit -> auto-records `refs/notes/ai` while commit message stays clean!
    fs::write(
        repo_dir.join("math.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\npub fn mul(a: i32, b: i32) -> i32 {\n    a * b\n}\n",
    )
    .unwrap();
    let agent_c = run_oz(
        &repo_dir,
        &["commit", "--agent", "-m", "feat: mul function", "--json"],
        &[
            ("OZ_AGENT_CLIENT", "claude-code"),
            ("OZ_AGENT_SESSION", "claude-sess-77"),
            ("OZ_AGENT_MODEL", "claude-sonnet-4-6"),
        ],
    );
    assert_eq!(agent_c.code, 0, "stderr: {}", agent_c.stderr);
    let agent_oid = agent_c.unwrap_json()["data"]["commit_oid"]
        .as_str()
        .unwrap()
        .to_string();

    let commit_body = sys_git(&repo_dir, &["log", "-1", "--format=%B", &agent_oid]);
    assert!(commit_body.starts_with("feat: mul function"));
    assert!(commit_body.contains("Change-Id: I"));
    assert!(!commit_body.contains("claude-sess-77"));

    let note_raw = sys_git(&repo_dir, &["notes", "--ref=ai", "show", &agent_oid]);
    let note: AiProvenanceNote = serde_json::from_str(&note_raw).expect("parse AiProvenanceNote");
    assert_eq!(note.client, "claude-code");
    assert_eq!(note.session_id, "claude-sess-77");
    assert_eq!(note.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(note.line_ranges.len(), 1);
    assert_eq!(note.line_ranges[0].start_line, 4);
    assert_eq!(note.line_ranges[0].end_line, 6);

    // 3. Ambient agent env detection (`CURSOR_TRACE_ID` without `--agent` flag).
    fs::write(
        repo_dir.join("math.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\npub fn mul(a: i32, b: i32) -> i32 {\n    a * b\n}\npub fn square(x: i32) -> i32 {\n    x * x\n}\n",
    )
    .unwrap();
    let cursor_c = run_oz(
        &repo_dir,
        &["commit", "-m", "feat: square helper", "--json"],
        &[("CURSOR_TRACE_ID", "cur-trace-88")],
    );
    assert_eq!(cursor_c.code, 0, "stderr: {}", cursor_c.stderr);

    // 4. `oz blame math.rs --json`
    let blame_json_out = run_oz(&repo_dir, &["blame", "math.rs", "--json"], &[]);
    assert_eq!(blame_json_out.code, 0, "stderr: {}", blame_json_out.stderr);
    let bj = blame_json_out.unwrap_json();
    assert_eq!(bj["schema"], OZ_SCHEMA_V1);
    assert_eq!(bj["ok"], true);
    assert_eq!(bj["data"]["total_lines"], 9);
    assert_eq!(bj["data"]["human_lines"], 3);
    assert_eq!(bj["data"]["agent_lines"], 6);
    assert_eq!(bj["data"]["lines"][0]["author_kind"], "human");
    assert_eq!(bj["data"]["lines"][3]["author_kind"], "agent");
    assert_eq!(bj["data"]["lines"][3]["client"], "claude-code");
    assert_eq!(bj["data"]["lines"][6]["author_kind"], "agent");
    assert_eq!(bj["data"]["lines"][6]["client"], "cursor");
    assert_eq!(bj["data"]["lines"][6]["session_id"], "cur-trace-88");

    // 5. `oz blame math.rs` human text output
    let blame_txt = run_oz(&repo_dir, &["blame", "math.rs"], &[]);
    assert_eq!(blame_txt.code, 0);
    assert!(blame_txt.stdout.contains("[human]"));
    assert!(blame_txt.stdout.contains("[AI:claude-code]"));
    assert!(blame_txt.stdout.contains("[AI:cursor]"));
    assert!(blame_txt.stdout.contains("3 human, 6 agent"));
}

// ============================================================================
// Test 3: `oz clone --filter=blob:none`, `oz fetch`, `oz push --atomic --force-with-lease`
// ============================================================================

#[test]
fn clone_partial_fetch_undo_and_push_atomic_force_with_lease_stale_rejection() {
    let tmp = TempDir::new().expect("tempdir");

    // Prepare bare remote with `receive.advertiseAtomic=true` and partial-clone filter support.
    let bare_dir = tmp.path().join("origin.git");
    fs::create_dir_all(&bare_dir).unwrap();
    sys_git(&bare_dir, &["init", "--bare", "-b", "main"]);
    sys_git(&bare_dir, &["config", "receive.advertiseAtomic", "true"]);
    sys_git(&bare_dir, &["config", "uploadpack.allowFilter", "true"]);
    sys_git(
        &bare_dir,
        &["config", "uploadpack.allowAnySHA1InWant", "true"],
    );

    // Seed upstream history via a staging repo.
    let seed_dir = tmp.path().join("seed");
    init_ozone_repo(&seed_dir);
    fs::write(seed_dir.join("v1.txt"), "historical blob\n").unwrap();
    sys_git(&seed_dir, &["add", "v1.txt"]);
    sys_git(&seed_dir, &["commit", "-m", "feat: v1"]);
    fs::write(seed_dir.join("v1.txt"), "latest blob\n").unwrap();
    fs::write(seed_dir.join("app.rs"), "fn main() {}\n").unwrap();
    sys_git(&seed_dir, &["add", "."]);
    sys_git(&seed_dir, &["commit", "-m", "feat: v2"]);

    let bare_url = format!("file://{}", bare_dir.display());
    sys_git(&seed_dir, &["remote", "add", "origin", &bare_url]);
    sys_git(&seed_dir, &["push", "-u", "origin", "main"]);
    sys_git(&seed_dir, &["checkout", "-b", "ephemeral"]);
    sys_git(&seed_dir, &["push", "origin", "ephemeral"]);
    sys_git(&seed_dir, &["checkout", "main"]);

    // 1. `oz clone --filter=blob:none <url> <dest> --json`
    let cloned_dir = tmp.path().join("cloned");
    let clone_out = run_oz(
        tmp.path(),
        &[
            "clone",
            "--filter=blob:none",
            &bare_url,
            cloned_dir.to_str().unwrap(),
            "--json",
        ],
        &[],
    );
    assert_eq!(clone_out.code, 0, "stderr: {}", clone_out.stderr);
    let cj = clone_out.unwrap_json();
    assert_eq!(cj["schema"], OZ_SCHEMA_V1);
    assert_eq!(cj["ok"], true);
    assert_eq!(cj["data"]["partial_clone"], true);
    assert_eq!(cj["data"]["filter"], "blob:none");
    assert!(cj["op_id"].as_str().is_some());
    assert!(cloned_dir.join(".git/ozone").is_dir());
    assert_eq!(
        fs::read_to_string(cloned_dir.join("app.rs")).unwrap(),
        "fn main() {}\n"
    );
    assert_eq!(
        sys_git(&cloned_dir, &["config", "--get", "remote.origin.promisor"]),
        "true"
    );
    sys_git(&cloned_dir, &["config", "user.name", "Clone User"]);
    sys_git(&cloned_dir, &["config", "user.email", "clone@ozone.test"]);

    // 2. Advance upstream `main` + delete `ephemeral`, then `oz fetch --prune --json` + `oz undo`!
    let pre_fetch_oid = sys_git(&cloned_dir, &["rev-parse", "refs/remotes/origin/main"]);
    fs::write(seed_dir.join("upstream.txt"), "new upstream commit\n").unwrap();
    sys_git(&seed_dir, &["add", "upstream.txt"]);
    sys_git(&seed_dir, &["commit", "-m", "feat: upstream advance"]);
    let new_upstream_oid = sys_git(&seed_dir, &["rev-parse", "HEAD"]);
    sys_git(&seed_dir, &["push", "origin", "main"]);
    sys_git(&seed_dir, &["push", "origin", "--delete", "ephemeral"]);

    let fetch_out = run_oz(&cloned_dir, &["fetch", "--prune", "--json"], &[]);
    assert_eq!(fetch_out.code, 0, "stderr: {}", fetch_out.stderr);
    let fj = fetch_out.unwrap_json();
    assert_eq!(fj["ok"], true);
    let fetch_op_id = fj["op_id"].as_str().expect("fetch op_id").to_string();
    assert!(
        fj["recovery_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c.as_str() == Some(&format!("oz undo {fetch_op_id}")))
    );
    assert!(
        fj["data"]["pruned_refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str() == Some("refs/remotes/origin/ephemeral"))
    );
    assert_eq!(
        sys_git(&cloned_dir, &["rev-parse", "refs/remotes/origin/main"]),
        new_upstream_oid
    );

    // Undo and redo the fetch via `run_with_args`!
    let undo_code = run_with_args(vec![
        OsString::from("oz"),
        OsString::from("-C"),
        cloned_dir.as_os_str().to_os_string(),
        OsString::from("undo"),
        OsString::from(&fetch_op_id),
        OsString::from("--json"),
    ]);
    assert_eq!(undo_code, ExitCode::Success);
    assert_eq!(
        sys_git(&cloned_dir, &["rev-parse", "refs/remotes/origin/main"]),
        pre_fetch_oid
    );
    let redo_out = run_oz(&cloned_dir, &["redo", "--json"], &[]);
    assert_eq!(redo_out.code, 0);
    assert_eq!(
        sys_git(&cloned_dir, &["rev-parse", "refs/remotes/origin/main"]),
        new_upstream_oid
    );

    // 3. `oz push --atomic -u` across two local branches
    sys_git(&cloned_dir, &["checkout", "-b", "feat-a"]);
    fs::write(cloned_dir.join("a.txt"), "branch a v1\n").unwrap();
    assert_eq!(
        run_oz(&cloned_dir, &["commit", "-m", "feat: a v1", "--json"], &[]).code,
        0
    );

    sys_git(&cloned_dir, &["checkout", "-b", "feat-b"]);
    fs::write(cloned_dir.join("b.txt"), "branch b v1\n").unwrap();
    assert_eq!(
        run_oz(&cloned_dir, &["commit", "-m", "feat: b v1", "--json"], &[]).code,
        0
    );

    let push1 = run_oz(
        &cloned_dir,
        &[
            "push",
            "origin",
            "feat-a",
            "feat-b",
            "--atomic",
            "-u",
            "--json",
        ],
        &[],
    );
    assert_eq!(push1.code, 0, "stderr: {}", push1.stderr);
    let pj1 = push1.unwrap_json();
    assert_eq!(pj1["ok"], true);
    assert_eq!(pj1["data"]["atomic"], true);
    assert_eq!(pj1["data"]["pushed_refs"].as_array().unwrap().len(), 2);

    // 4. Amend `feat-b` and push with `--atomic --force-with-lease` (fresh lease -> exit 0)
    fs::write(cloned_dir.join("b.txt"), "branch b v2\n").unwrap();
    assert_eq!(
        run_oz(
            &cloned_dir,
            &["commit", "--amend", "-m", "feat: b v2", "--json"],
            &[],
        )
        .code,
        0
    );
    let push_lease_ok = run_oz(
        &cloned_dir,
        &[
            "push",
            "origin",
            "feat-b",
            "--atomic",
            "--force-with-lease",
            "--json",
        ],
        &[],
    );
    assert_eq!(push_lease_ok.code, 0, "stderr: {}", push_lease_ok.stderr);
    assert_eq!(
        push_lease_ok.unwrap_json()["data"]["pushed_refs"][0]["forced"],
        true
    );

    // 5. Simulate concurrent remote advance on `feat-b` behind `cloned_dir`'s back -> stale lease!
    sys_git(&seed_dir, &["fetch", "origin", "feat-b"]);
    sys_git(
        &seed_dir,
        &["checkout", "-B", "feat-b", "refs/remotes/origin/feat-b"],
    );
    fs::write(seed_dir.join("concurrent.txt"), "teammate push\n").unwrap();
    sys_git(&seed_dir, &["add", "concurrent.txt"]);
    sys_git(&seed_dir, &["commit", "-m", "feat: concurrent remote update"]);
    sys_git(&seed_dir, &["push", "origin", "feat-b"]);

    fs::write(cloned_dir.join("b.txt"), "branch b v3 local\n").unwrap();
    assert_eq!(
        run_oz(
            &cloned_dir,
            &["commit", "--amend", "-m", "feat: b v3", "--json"],
            &[],
        )
        .code,
        0
    );

    let push_stale = run_oz(
        &cloned_dir,
        &[
            "push",
            "origin",
            "feat-b",
            "--atomic",
            "--force-with-lease",
            "--json",
        ],
        &[],
    );
    assert_eq!(push_stale.code, 1, "expected exit code 1 on stale lease");
    let stale_json = push_stale.unwrap_json();
    assert_eq!(stale_json["schema"], OZ_SCHEMA_V1);
    assert_eq!(stale_json["ok"], false);
    assert_eq!(stale_json["error"]["exit_code"], 1);
    assert_eq!(stale_json["conflicts"][0]["id"], "stale-lease");
    assert!(
        !stale_json["recovery_commands"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

// ============================================================================
// Test 4: `oz pr create`, `oz pr stack`, `oz ci`, and `oz sync`
// ============================================================================

#[test]
fn pr_create_pr_stack_ci_and_sync_against_mock_github_api() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let tmp = TempDir::new().expect("tempdir");
    let bare_dir = tmp.path().join("remotes/acme/rocket.git");
    fs::create_dir_all(&bare_dir).unwrap();
    sys_git(&bare_dir, &["init", "--bare", "-b", "main"]);
    sys_git(&bare_dir, &["config", "receive.advertiseAtomic", "true"]);

    let repo_dir = tmp.path().join("rocket");
    init_ozone_repo(&repo_dir);
    let bare_url = format!("file://{}", bare_dir.display());
    sys_git(&repo_dir, &["remote", "add", "origin", &bare_url]);
    sys_git(&repo_dir, &["push", "-u", "origin", "main"]);

    let token = "ghp_mock_m3_workflow_token";
    let state = Arc::new(Mutex::new(MockGitHubState::new(
        token,
        Some(bare_dir.clone()),
    )));
    let api_url = rt.block_on(spawn_mock_github_api(Arc::clone(&state)));
    let gh_env = [
        ("GITHUB_TOKEN", token),
        ("OZ_GITHUB_API_URL", api_url.as_str()),
    ];

    // 1. Spawn `ws-1` (`feat-1`), commit, and run `oz pr create --json`.
    let sp1 = run_oz(
        &repo_dir,
        &[
            "ws", "spawn", "ws-1", "--branch", "feat-1", "--no-warm", "--json",
        ],
        &gh_env,
    );
    assert_eq!(sp1.code, 0, "stderr: {}", sp1.stderr);
    let ws1_path = PathBuf::from(sp1.unwrap_json()["data"]["worktree_path"].as_str().unwrap());

    fs::write(ws1_path.join("feat1.rs"), "pub fn f1() -> u32 { 1 }\n").unwrap();
    let c1 = run_oz(&ws1_path, &["commit", "-m", "feat: part 1", "--json"], &gh_env);
    assert_eq!(c1.code, 0);
    let cid1 = c1.unwrap_json()["data"]["change_id"]
        .as_str()
        .unwrap()
        .to_string();

    let pr1 = run_oz(&ws1_path, &["pr", "create", "--base", "main", "--json"], &gh_env);
    assert_eq!(pr1.code, 0, "stderr: {}", pr1.stderr);
    let pr1_json = pr1.unwrap_json();
    assert_eq!(pr1_json["ok"], true);
    assert_eq!(pr1_json["data"]["created"], true);
    assert_eq!(pr1_json["data"]["pull_request"]["number"], 1);
    assert!(
        pr1_json["data"]["pull_request"]["body"]
            .as_str()
            .unwrap()
            .contains(&cid1)
    );

    // 2. Spawn `ws-2` (`feat-2` stacked onto `feat-1`), commit, and run `oz pr stack --json`.
    let sp2 = run_oz(
        &repo_dir,
        &[
            "ws", "spawn", "ws-2", "--branch", "feat-2", "--from", "feat-1", "--no-warm",
            "--json",
        ],
        &gh_env,
    );
    assert_eq!(sp2.code, 0, "stderr: {}", sp2.stderr);
    let ws2_path = PathBuf::from(sp2.unwrap_json()["data"]["worktree_path"].as_str().unwrap());

    fs::write(ws2_path.join("feat2.rs"), "pub fn f2() -> u32 { 2 }\n").unwrap();
    let c2 = run_oz(&ws2_path, &["commit", "-m", "feat: part 2", "--json"], &gh_env);
    assert_eq!(c2.code, 0);
    let feat2_sha = c2.unwrap_json()["data"]["commit_oid"]
        .as_str()
        .unwrap()
        .to_string();

    let pr_stack_out = run_oz(&ws2_path, &["pr", "stack", "--base", "main", "--json"], &gh_env);
    assert_eq!(pr_stack_out.code, 0, "stderr: {}", pr_stack_out.stderr);
    let ps_json = pr_stack_out.unwrap_json();
    assert_eq!(ps_json["ok"], true);
    let items = ps_json["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["head_branch"], "feat-1");
    assert_eq!(items[0]["base_branch"], "main");
    assert_eq!(items[1]["head_branch"], "feat-2");
    assert_eq!(items[1]["base_branch"], "feat-1");

    {
        let guard = state.lock().unwrap();
        assert_eq!(guard.prs.len(), 2);
        for pr in &guard.prs {
            let body = pr.body.as_deref().unwrap_or_default();
            assert!(body.contains(OZ_STACK_START));
            assert!(body.contains(OZ_STACK_END));
            assert!(body.contains("👉"));
        }
    }

    // 3. Configure failing CI on `feat-2` -> `oz ci --json` returns exit code `1` with full `oz.v1` envelope!
    {
        let mut guard = state.lock().unwrap();
        guard.check_runs_by_ref.insert(
            feat2_sha.clone(),
            vec![GitHubCheckRun {
                id: 501,
                name: "cargo-test".to_string(),
                status: "completed".to_string(),
                conclusion: Some("failure".to_string()),
                html_url: Some("https://github.com/acme/rocket/actions/runs/900/job/701".into()),
                details_url: None,
                output: GitHubCheckRunOutput {
                    title: Some("test failed".into()),
                    summary: None,
                    text: None,
                },
            }],
        );
        guard.workflow_runs.push(GitHubWorkflowRun {
            id: 900,
            name: Some("CI".into()),
            head_branch: Some("feat-2".into()),
            head_sha: feat2_sha.clone(),
            status: "completed".into(),
            conclusion: Some("failure".into()),
            html_url: None,
        });
        guard.jobs_by_run.insert(
            900,
            vec![GitHubActionsJob {
                id: 701,
                run_id: Some(900),
                name: "cargo-test".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                html_url: None,
                steps: vec![GitHubJobStep {
                    name: "Run cargo test".into(),
                    status: "completed".into(),
                    conclusion: Some("failure".into()),
                    number: 3,
                }],
            }],
        );
        guard.job_logs.insert(
            701,
            b"2026-11-18T12:00:01.0000000Z \x1b[31m##[error]error[E0308]: mismatched types in feat2.rs:1\x1b[0m\n"
                .to_vec(),
        );
    }

    let ci_fail = run_oz(&ws2_path, &["ci", "--json"], &gh_env);
    assert_eq!(ci_fail.code, 1, "expected exit code 1 when CI is failing");
    let cif_json = ci_fail.unwrap_json();
    assert_eq!(cif_json["schema"], OZ_SCHEMA_V1);
    assert_eq!(cif_json["ok"], false);
    assert_eq!(cif_json["error"]["exit_code"], 1);
    assert_eq!(cif_json["data"]["overall_status"], "failing");
    assert_eq!(cif_json["data"]["failing_count"], 1);
    assert_eq!(
        cif_json["data"]["checks"][0]["failing_step"],
        "Run cargo test"
    );
    let excerpt = cif_json["data"]["checks"][0]["log_excerpt"]
        .as_str()
        .unwrap();
    assert!(excerpt.contains("error[E0308]: mismatched types in feat2.rs:1"));
    assert!(!excerpt.contains("\x1b["));

    // Flip CI check to passing -> `oz ci --json` returns exit code `0` and `ok == true`!
    {
        let mut guard = state.lock().unwrap();
        guard.check_runs_by_ref.insert(
            feat2_sha.clone(),
            vec![GitHubCheckRun {
                id: 502,
                name: "cargo-test".to_string(),
                status: "completed".to_string(),
                conclusion: Some("success".to_string()),
                html_url: None,
                details_url: None,
                output: GitHubCheckRunOutput::default(),
            }],
        );
        guard.workflow_runs.clear();
        guard.jobs_by_run.clear();
    }
    let ci_pass = run_oz(&ws2_path, &["ci", "--pr", "2", "--json"], &gh_env);
    assert_eq!(ci_pass.code, 0, "stderr: {}", ci_pass.stderr);
    let cip_json = ci_pass.unwrap_json();
    assert_eq!(cip_json["ok"], true);
    assert_eq!(cip_json["data"]["all_passed"], true);
    assert_eq!(cip_json["data"]["overall_status"], "passing");

    // 4. Simulate merging PR #1 (`feat-1`) into `main` on upstream + GitHub mock, then `oz sync --json`!
    let merger_clone = tmp.path().join("upstream-merger");
    sys_git(
        tmp.path(),
        &["clone", &bare_url, merger_clone.to_str().unwrap()],
    );
    sys_git(&merger_clone, &["config", "user.name", "GitHub Merge"]);
    sys_git(&merger_clone, &["config", "user.email", "noreply@github.com"]);
    sys_git(
        &merger_clone,
        &["merge", "--no-ff", "origin/feat-1", "-m", "Merge PR #1"],
    );
    let merged_main_sha = sys_git(&merger_clone, &["rev-parse", "HEAD"]);
    sys_git(&merger_clone, &["push", "origin", "main"]);

    {
        let mut guard = state.lock().unwrap();
        if let Some(pr1_mut) = guard.prs.iter_mut().find(|p| p.number == 1) {
            pr1_mut.state = "closed".to_string();
            pr1_mut.merged = true;
            pr1_mut.merged_at = Some("2026-11-18T12:05:00Z".to_string());
            pr1_mut.merge_commit_sha = Some(merged_main_sha.clone());
        }
    }

    let sync_out = run_oz(&repo_dir, &["sync", "--json"], &gh_env);
    assert_eq!(sync_out.code, 0, "stderr: {}", sync_out.stderr);
    let sj = sync_out.unwrap_json();
    assert_eq!(sj["ok"], true);
    assert_eq!(sj["data"]["fast_forwarded"], true);
    assert_eq!(sj["data"]["base_oid"], merged_main_sha);
    assert!(
        sj["data"]["merged_branches"]
            .as_array()
            .unwrap()
            .contains(&json!("feat-1"))
    );
    assert_eq!(sj["data"]["retargeted_prs"][0]["number"], 2);
    assert_eq!(sj["data"]["retargeted_prs"][0]["old_base"], "feat-1");
    assert_eq!(sj["data"]["retargeted_prs"][0]["new_base"], "main");
    assert!(
        sj["data"]["rebased_branches"]
            .as_array()
            .unwrap()
            .contains(&json!("feat-2"))
    );
    assert!(
        sj["data"]["removed_workspaces"]
            .as_array()
            .unwrap()
            .contains(&json!("ws-1"))
    );
    assert!(!ws1_path.exists(), "merged CoW workspace ws-1 must be GC'd");
}

// ============================================================================
// Test 5: `oz agent setup --all --json` + `--check`
// ============================================================================

#[test]
fn agent_setup_all_and_check_is_idempotent_and_non_destructive() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    init_ozone_repo(&repo_dir);

    fs::create_dir_all(repo_dir.join(".claude")).unwrap();
    fs::write(
        repo_dir.join(".claude/settings.json"),
        r#"{"permissions":{"allow":["Bash(cargo test:*)"]}}"#,
    )
    .unwrap();

    // 1. `--check` dry-run writes nothing to disk.
    let pre_check = run_oz(
        &repo_dir,
        &["agent", "setup", "--all", "--check", "--json"],
        &[],
    );
    assert_eq!(pre_check.code, 0, "stderr: {}", pre_check.stderr);
    let pcj = pre_check.unwrap_json();
    assert_eq!(pcj["ok"], true);
    assert_eq!(pcj["data"]["check_only"], true);
    assert_eq!(pcj["data"]["harnesses"].as_array().unwrap().len(), 5);
    assert!(!pcj["data"]["created"].as_array().unwrap().is_empty());
    assert!(!repo_dir.join(".claude/skills/oz/SKILL.md").exists());

    // 2. Apply `--all`.
    let apply = run_oz(&repo_dir, &["agent", "setup", "--all", "--json"], &[]);
    assert_eq!(apply.code, 0, "stderr: {}", apply.stderr);
    let aj = apply.unwrap_json();
    assert_eq!(aj["ok"], true);
    assert_eq!(aj["data"]["check_only"], false);
    assert!(repo_dir.join(".claude/skills/oz/SKILL.md").is_file());
    assert!(repo_dir.join(".cursor/rules/oz.mdc").is_file());
    assert!(repo_dir.join("AGENTS.md").is_file());
    assert!(repo_dir.join(".windsurf/rules/oz.md").is_file());
    assert!(repo_dir.join(".agent/skills/oz/SKILL.md").is_file());

    let claude_settings: Value = serde_json::from_str(
        &fs::read_to_string(repo_dir.join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        claude_settings["permissions"]["allow"][0],
        "Bash(cargo test:*)"
    );
    assert!(
        serde_json::to_string(&claude_settings["hooks"])
            .unwrap()
            .contains(OZ_CHECKPOINT_HOOK_CMD)
    );

    // 3. Re-running `--all --check` reports 0 created, 0 updated, all unchanged!
    let post_check = run_oz(
        &repo_dir,
        &["agent", "setup", "--all", "--check", "--json"],
        &[],
    );
    assert_eq!(post_check.code, 0);
    let post_json = post_check.unwrap_json();
    assert!(post_json["data"]["created"].as_array().unwrap().is_empty());
    assert!(post_json["data"]["updated"].as_array().unwrap().is_empty());
    assert!(!post_json["data"]["unchanged"].as_array().unwrap().is_empty());
}

// ============================================================================
// Test 6: `oz mcp` Binary Stdio Session + Wired `oz_ci_logs` Provider
// ============================================================================

#[test]
fn mcp_binary_stdio_session_exercises_status_commit_and_live_ci_logs() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("mcp-repo");
    init_ozone_repo(&repo_dir);
    sys_git(
        &repo_dir,
        &["remote", "add", "origin", "https://github.com/acme/mcp-app.git"],
    );

    fs::write(
        repo_dir.join("lib.rs"),
        "pub fn one() -> i32 {\n    1\n}\n\n\n\n\n\n\npub fn two() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    sys_git(&repo_dir, &["add", "lib.rs"]);
    sys_git(&repo_dir, &["commit", "-m", "chore: seed lib.rs"]);

    // Dirty two separate hunks (`h1a` and `h1b`) in `lib.rs`.
    fs::write(
        repo_dir.join("lib.rs"),
        "pub fn one() -> i32 {\n    10\n}\n\n\n\n\n\n\npub fn two() -> i32 {\n    20\n}\n",
    )
    .unwrap();

    let token = "ghp_mcp_live_ci_token";
    let state = Arc::new(Mutex::new(MockGitHubState::new(token, None)));
    let api_url = rt.block_on(spawn_mock_github_api(Arc::clone(&state)));

    let mut cmd = build_clean_oz_command(
        &repo_dir,
        &["mcp", "--session-id", "mcp-cli-session-1"],
        &[
            ("GITHUB_TOKEN", token),
            ("OZ_GITHUB_API_URL", api_url.as_str()),
        ],
    );
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn oz mcp");
    let mut child_stdin = child.stdin.take().expect("child stdin");
    let mut child_stdout = StdBufReader::new(child.stdout.take().expect("child stdout"));

    fn rpc(
        stdin: &mut std::process::ChildStdin,
        stdout: &mut StdBufReader<std::process::ChildStdout>,
        req: Value,
    ) -> Value {
        writeln!(stdin, "{}", serde_json::to_string(&req).unwrap()).unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        let n = stdout.read_line(&mut line).unwrap();
        assert!(n > 0, "unexpected EOF from oz mcp");
        serde_json::from_str(&line).expect("valid JSON-RPC response")
    }

    // 1. initialize
    let init_resp = rpc(
        &mut child_stdin,
        &mut child_stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "clientInfo": { "name": "claude-code", "version": "1.0" }
            }
        }),
    );
    assert_eq!(init_resp["result"]["protocolVersion"], "2024-11-05");

    writeln!(
        child_stdin,
        "{}",
        serde_json::to_string(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).unwrap()
    )
    .unwrap();
    child_stdin.flush().unwrap();

    // 2. tools/list
    let list_resp = rpc(
        &mut child_stdin,
        &mut child_stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    );
    let tool_names: Vec<&str> = list_resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "oz_status",
        "oz_diff",
        "oz_commit",
        "oz_ws_spawn",
        "oz_radar",
        "oz_checkpoint",
        "oz_undo",
        "oz_ci_logs",
        "oz_log",
        "oz_restore",
        "oz_show",
    ] {
        assert!(tool_names.contains(&expected), "missing MCP tool {expected}");
    }

    // 3. oz_status
    let status_resp = rpc(
        &mut child_stdin,
        &mut child_stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "oz_status", "arguments": {} }
        }),
    );
    assert_eq!(status_resp["result"]["isError"], false);
    let status_payload: Value =
        serde_json::from_str(status_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(status_payload["changes"][0]["id"], "f1");
    assert_eq!(status_payload["changes"][0]["hunks"][0]["id"], "h1a");
    assert_eq!(status_payload["changes"][0]["hunks"][1]["id"], "h1b");

    // 4. oz_commit (`pick: ["h1a"]`) -> partial commit + automatic `refs/notes/ai`!
    let commit_resp = rpc(
        &mut child_stdin,
        &mut child_stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "oz_commit",
                "arguments": {
                    "message": "feat(mcp): update one()",
                    "pick": ["h1a"]
                }
            }
        }),
    );
    assert_eq!(commit_resp["result"]["isError"], false);
    let commit_payload: Value =
        serde_json::from_str(commit_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(commit_payload["ok"], true);
    assert_eq!(commit_payload["remaining_dirty_count"], 1);
    let mcp_commit_oid = commit_payload["commit_oid"].as_str().unwrap().to_string();
    assert_eq!(commit_payload["provenance"]["client"], "claude-code");
    assert_eq!(
        commit_payload["provenance"]["session_id"],
        "mcp-cli-session-1"
    );

    // Register a failing CI check + log for `mcp_commit_oid` in Mock GitHub API!
    {
        let mut guard = state.lock().unwrap();
        guard.check_runs_by_ref.insert(
            mcp_commit_oid.clone(),
            vec![GitHubCheckRun {
                id: 801,
                name: "clippy".to_string(),
                status: "completed".to_string(),
                conclusion: Some("failure".to_string()),
                html_url: Some("https://github.com/acme/mcp-app/actions/runs/910/job/811".into()),
                details_url: None,
                output: GitHubCheckRunOutput::default(),
            }],
        );
        guard.workflow_runs.push(GitHubWorkflowRun {
            id: 910,
            name: Some("Lint".into()),
            head_branch: Some("main".into()),
            head_sha: mcp_commit_oid.clone(),
            status: "completed".into(),
            conclusion: Some("failure".into()),
            html_url: None,
        });
        guard.jobs_by_run.insert(
            910,
            vec![GitHubActionsJob {
                id: 811,
                run_id: Some(910),
                name: "clippy".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                html_url: None,
                steps: vec![GitHubJobStep {
                    name: "Run clippy -D warnings".into(),
                    status: "completed".into(),
                    conclusion: Some("failure".into()),
                    number: 2,
                }],
            }],
        );
        guard.job_logs.insert(
            811,
            b"2026-11-18T12:10:00.0000000Z ##[error]warning: unused variable `x` promoted to error\n"
                .to_vec(),
        );
    }

    // 5. oz_ci_logs -> hits live `oz-github` CiLogProvider against `OZ_GITHUB_API_URL`!
    let ci_resp = rpc(
        &mut child_stdin,
        &mut child_stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "oz_ci_logs",
                "arguments": { "failing_only": true }
            }
        }),
    );
    assert_eq!(ci_resp["result"]["isError"], false);
    let ci_payload: Value =
        serde_json::from_str(ci_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(ci_payload["ok"], true);
    assert_eq!(ci_payload["all_passed"], false);
    assert_eq!(ci_payload["commit_oid"], mcp_commit_oid);
    assert_eq!(ci_payload["checks"][0]["name"], "clippy");
    assert_eq!(
        ci_payload["checks"][0]["failing_step"],
        "Run clippy -D warnings"
    );
    assert!(
        ci_payload["checks"][0]["log_excerpt"]
            .as_str()
            .unwrap()
            .contains("unused variable `x`")
    );

    drop(child_stdin);
    let status = child.wait().expect("wait oz mcp");
    assert!(status.success());
}

// ============================================================================
// Test 7: `oz doctor --json` (`ssh_signing` + `github_auth`)
// ============================================================================

#[test]
fn doctor_json_verifies_ssh_signing_and_github_auth_fields() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("doctor-repo");
    init_ozone_repo(&repo_dir);

    let key_path = tmp.path().join("doctor_ed25519");
    let keygen = Command::new(ssh_keygen_bin())
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "dev@ozone.test",
            "-f",
            key_path.to_str().unwrap(),
        ])
        .output()
        .expect("ssh-keygen");
    assert!(keygen.status.success());

    fs::write(
        repo_dir.join(".ozone.toml"),
        format!(
            "backend = \"cas_hardlink\"\n\n[signing]\nenabled = true\nkey = \"{}\"\n",
            key_path.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    sys_git(
        &repo_dir,
        &["remote", "add", "origin", "https://github.com/acme/rocket.git"],
    );

    let secret_tok = "ghp_super_secret_doctor_token_987654";
    let doc = run_oz(
        &repo_dir,
        &["doctor", "--json"],
        &[("GITHUB_TOKEN", secret_tok)],
    );
    assert_eq!(doc.code, 0, "stderr: {}", doc.stderr);
    let dj = doc.unwrap_json();
    assert_eq!(dj["schema"], OZ_SCHEMA_V1);
    assert_eq!(dj["ok"], true);

    // Existing DoctorReport keys remain intact:
    assert_eq!(dj["data"]["repo"]["in_repo"], true);
    assert!(dj["data"]["filesystem"]["type"].is_string());
    assert!(dj["data"]["recommended_backend"].is_string());

    // `ssh_signing` fields:
    assert_eq!(dj["data"]["ssh_signing"]["enabled_by_default"], true);
    assert_eq!(dj["data"]["ssh_signing"]["ready"], true);
    assert_eq!(dj["data"]["ssh_signing"]["key_algorithm"], "ssh-ed25519");
    assert!(
        dj["data"]["ssh_signing"]["fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("SHA256:")
    );

    // `github_auth` fields:
    assert_eq!(dj["data"]["github_auth"]["authenticated"], true);
    assert_eq!(dj["data"]["github_auth"]["host"], "github.com");
    assert_eq!(dj["data"]["github_auth"]["owner"], "acme");
    assert_eq!(dj["data"]["github_auth"]["repo"], "rocket");
    assert_eq!(
        dj["data"]["github_auth"]["token_source"]["kind"],
        "github_token_env"
    );
    let masked = dj["data"]["github_auth"]["masked_token"].as_str().unwrap();
    assert!(!masked.contains("super_secret_doctor_token"));
}

// ============================================================================
// Test 8: Non-Interactive Guardrails, Default Clone Dir & Explicit Lease Spec
// ============================================================================

#[test]
fn non_interactive_usage_envelopes_default_clone_dir_and_explicit_lease_spec() {
    let tmp = TempDir::new().expect("tempdir");
    let bare_dir = tmp.path().join("my-service.git");
    fs::create_dir_all(&bare_dir).unwrap();
    sys_git(&bare_dir, &["init", "--bare", "-b", "main"]);

    let seed = tmp.path().join("seed");
    init_ozone_repo(&seed);
    let bare_url = format!("file://{}", bare_dir.display());
    sys_git(&seed, &["remote", "add", "origin", &bare_url]);
    sys_git(&seed, &["push", "-u", "origin", "main"]);

    // 1. `oz clone <url>` without `<directory>` derives `my-service` automatically!
    let work_parent = tmp.path().join("work");
    fs::create_dir_all(&work_parent).unwrap();
    let clone_default = run_oz(&work_parent, &["clone", &bare_url, "--json"], &[]);
    assert_eq!(clone_default.code, 0, "stderr: {}", clone_default.stderr);
    let derived_repo = work_parent.join("my-service");
    assert!(derived_repo.join(".git/ozone").is_dir());
    sys_git(&derived_repo, &["config", "user.name", "Clone User"]);
    sys_git(&derived_repo, &["config", "user.email", "clone@ozone.test"]);

    // 2. Non-interactive `oz commit` without `-m` fails immediately with exit code 2 (never launches $EDITOR).
    fs::write(derived_repo.join("README.md"), "dirty\n").unwrap();
    let missing_msg = run_oz(&derived_repo, &["commit", "--json"], &[]);
    assert_eq!(missing_msg.code, 2);
    assert_eq!(missing_msg.unwrap_json()["error"]["code"], "invalid_usage");

    // 3. Conflicting `oz ci --ref main --branch main` returns exit code 2.
    let bad_ci = run_oz(
        &derived_repo,
        &["ci", "--ref", "main", "--branch", "main", "--json"],
        &[],
    );
    assert_eq!(bad_ci.code, 2);
    assert_eq!(bad_ci.unwrap_json()["error"]["code"], "invalid_usage");

    // 4. Unknown `--harness` returns exit code 2 with `oz.v1` JSON envelope; valid subset works cleanly.
    let bad_harness = run_oz(
        &derived_repo,
        &["agent", "setup", "--harness", "not-a-real-harness", "--json"],
        &[],
    );
    assert_eq!(bad_harness.code, 2);
    assert_eq!(bad_harness.unwrap_json()["error"]["code"], "invalid_usage");

    let subset_setup = run_oz(
        &derived_repo,
        &[
            "agent",
            "setup",
            "--harness",
            "claude-code,antigravity",
            "--json",
        ],
        &[],
    );
    assert_eq!(subset_setup.code, 0, "stderr: {}", subset_setup.stderr);
    assert_eq!(
        subset_setup.unwrap_json()["data"]["harnesses"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // 5. Explicit `--force-with-lease=main:<oid>` succeeds when matching and rejects non-FF without lease!
    let old_main_oid = sys_git(&bare_dir, &["rev-parse", "refs/heads/main"]);
    sys_git(&seed, &["commit", "--allow-empty", "-m", "chore: advance remote"]);
    sys_git(&seed, &["push", "origin", "main"]);
    let actual_remote_oid = sys_git(&bare_dir, &["rev-parse", "refs/heads/main"]);

    assert_eq!(
        run_oz(&derived_repo, &["commit", "-m", "feat: local diverged", "--json"], &[]).code,
        0
    );

    // Plain `oz push` without force/lease rejects non-fast-forward with exit code 1!
    let non_ff = run_oz(&derived_repo, &["push", "origin", "main", "--json"], &[]);
    assert_eq!(non_ff.code, 1);
    assert_eq!(non_ff.unwrap_json()["conflicts"][0]["id"], "push-non-ff");

    // Explicit stale OID `--force-with-lease=main:<old_main_oid>` rejects with exit code 1!
    let stale_explicit = run_oz(
        &derived_repo,
        &[
            "push",
            "origin",
            "main",
            &format!("--force-with-lease=main:{old_main_oid}"),
            "--json",
        ],
        &[],
    );
    assert_eq!(stale_explicit.code, 1);
    assert_eq!(
        stale_explicit.unwrap_json()["conflicts"][0]["id"],
        "stale-lease"
    );

    // Explicit matching OID `--force-with-lease=main:<actual_remote_oid>` succeeds with exit code 0!
    let valid_explicit = run_oz(
        &derived_repo,
        &[
            "push",
            "origin",
            "main",
            &format!("--force-with-lease=main:{actual_remote_oid}"),
            "--json",
        ],
        &[],
    );
    assert_eq!(valid_explicit.code, 0, "stderr: {}", valid_explicit.stderr);
    assert_eq!(
        valid_explicit.unwrap_json()["data"]["pushed_refs"][0]["forced"],
        true
    );
}
