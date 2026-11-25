// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end integration tests with a real `tokio` Mock GitHub API server covering:
//! - Non-interactive token discovery (`hosts.yml`, env priority, `git credential fill` non-TTY guard, `probe_github_auth`)
//! - `pr_create` (push + commit title/body + `Change-Id:` preservation + idempotent update)
//! - 3-branch `pr_stack` (`main <- stack-1 <- stack-2 <- stack-3`, atomic push, cross-linked stack tables with `👉`)
//! - `sync_repo` (`oz sync`: fetch, fast-forward `main`, auto-retarget PR #2 base to `main`, GC merged CoW workspace + prune checkpoints, rebase downstream stack via `oz-integrate`, refresh `oz-warm` baseline, plus squash-merge & offline bare-remote sync)
//! - `query_ci_status` (`oz ci`: check-runs + Actions jobs + plain-text & PKZIP failing log extraction)

use flate2::Compression;
use flate2::write::DeflateEncoder;
use oz_core::{BackendKind, CommitOptions, OzoneRepo, commit_changes};
use oz_cow::{SpawnWorkspaceOptions, list_workspaces, spawn_workspace};
use oz_github::{
    CiQueryOptions, GitHubActionsJob, GitHubCheckRun, GitHubCheckRunOutput, GitHubClient,
    GitHubJobStep, GitHubPrRef, GitHubPullRequest, GitHubWorkflowRun, OZ_STACK_END, OZ_STACK_START,
    PrCreateOptions, PrStackOptions, SyncOptions, TokenDiscoveryOptions, TokenSource,
    discover_github_token_with_options, pr_create, pr_stack, probe_github_auth,
    query_ci_status, query_git_credential_fill, sync_repo,
};
use oz_oplog::{ActorInfo, ActorKind, checkpoint};
use oz_warm::find_baseline;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

fn test_actor() -> ActorInfo {
    ActorInfo {
        kind: ActorKind::Agent,
        client: Some("oz-github-workflow-test".to_string()),
        workspace: None,
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} in {} failed:\nstdout: {}\nstderr: {}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Stateful in-memory GitHub REST/GraphQL mock server state.
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
        if let Some(ref bare) = self.bare_repo {
            let out = Command::new("git")
                .args(["rev-parse", &format!("refs/heads/{branch}")])
                .current_dir(bare)
                .output()
                .ok();
            if let Some(o) = out
                && o.status.success()
            {
                return String::from_utf8_lossy(&o.stdout).trim().to_string();
            }
        }
        "0000000000000000000000000000000000000000".to_string()
    }
}

async fn spawn_mock_github_api(state: Arc<Mutex<MockGitHubState>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock api");
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

    // POST /graphql
    if method == "POST" && segs.as_slice() == ["graphql"] {
        return (
            "200 OK",
            "application/json",
            serde_json::to_vec(&json!({ "data": { "viewer": { "login": "ozone-bot" } } })).unwrap(),
        );
    }

    // /repos/{owner}/{repo}/...
    if segs.len() >= 4 && segs[0] == "repos" {
        let owner = segs[1];
        let repo = segs[2];
        let tail = &segs[3..];

        // GET /repos/:owner/:repo/pulls
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

        // POST /repos/:owner/:repo/pulls
        if method == "POST" && tail == ["pulls"] {
            let val: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
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

        // GET or PATCH /repos/:owner/:repo/pulls/:num
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
                let val: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                let new_base_sha = val["base"]
                    .as_str()
                    .map(|b| st.resolve_branch_sha(b));
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

        // GET /repos/:owner/:repo/commits/:ref/check-runs
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

        // GET /repos/:owner/:repo/actions/runs
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

        // GET /repos/:owner/:repo/actions/runs/:run_id/jobs
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

        // GET /repos/:owner/:repo/actions/jobs/:job_id
        if method == "GET"
            && tail.len() == 3
            && tail[0] == "actions"
            && tail[1] == "jobs"
            && let Ok(job_id) = tail[2].parse::<u64>()
            && let Some(job) = st
                .jobs_by_run
                .values()
                .flatten()
                .find(|j| j.id == job_id)
        {
            return (
                "200 OK",
                "application/json",
                serde_json::to_vec(job).unwrap(),
            );
        }

        // GET /repos/:owner/:repo/actions/jobs/:job_id/logs
        if method == "GET"
            && tail.len() == 4
            && tail[0] == "actions"
            && tail[1] == "jobs"
            && tail[3] == "logs"
            && let Ok(job_id) = tail[2].parse::<u64>()
            && let Some(bytes) = st.job_logs.get(&job_id)
        {
            let ct = if bytes.starts_with(b"PK\x03\x04") {
                "application/zip"
            } else {
                "text/plain; charset=utf-8"
            };
            return ("200 OK", ct, bytes.clone());
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

fn build_zip_deflate(filename: &str, payload: &[u8]) -> Vec<u8> {
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
    enc.write_all(payload).expect("deflate");
    let compressed = enc.finish().expect("finish");

    let name_bytes = filename.as_bytes();
    let comp_len = compressed.len() as u32;
    let uncomp_len = payload.len() as u32;

    let mut zip = Vec::new();
    zip.extend_from_slice(b"PK\x03\x04");
    zip.extend_from_slice(&20u16.to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes());
    zip.extend_from_slice(&8u16.to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes());
    zip.extend_from_slice(&0u32.to_le_bytes());
    zip.extend_from_slice(&comp_len.to_le_bytes());
    zip.extend_from_slice(&uncomp_len.to_le_bytes());
    zip.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes());
    zip.extend_from_slice(name_bytes);
    zip.extend_from_slice(&compressed);

    let cd_offset = zip.len() as u32;
    zip.extend_from_slice(b"PK\x01\x02");
    zip.extend_from_slice(&20u16.to_le_bytes());
    zip.extend_from_slice(&20u16.to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes());
    zip.extend_from_slice(&8u16.to_le_bytes());
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
    zip.extend_from_slice(&0u32.to_le_bytes());
    zip.extend_from_slice(name_bytes);

    let cd_size = (zip.len() as u32) - cd_offset;
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

/// Initializes a local repo + bare `origin.git` configured with `<owner>/<repo>` path so both
/// pure-Rust `wire` (`file:///.../acme/rocket.git`) AND `api` (`owner=acme, repo=rocket`) resolve!
fn init_repo_with_bare_github_named_origin(tmp: &Path) -> (PathBuf, PathBuf) {
    let bare_parent = tmp.join("remotes").join("acme");
    let bare_dir = bare_parent.join("rocket.git");
    std::fs::create_dir_all(&bare_dir).expect("mkdir bare");
    run_git(&bare_dir, &["init", "--bare", "-b", "main"]);
    run_git(&bare_dir, &["config", "receive.advertiseAtomic", "true"]);

    let repo_dir = tmp.join("rocket");
    std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
    run_git(&repo_dir, &["init", "-b", "main"]);
    run_git(&repo_dir, &["config", "user.name", "Ozone Tester"]);
    run_git(&repo_dir, &["config", "user.email", "tester@ozone.dev"]);

    // Include `Cargo.toml` + `target/` artifact so `oz-warm` baseline detection & capture activates!
    std::fs::write(
        repo_dir.join("Cargo.toml"),
        "[package]\nname = \"rocket\"\nversion = \"0.1.0\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::create_dir_all(repo_dir.join("target/debug")).expect("mkdir target");
    std::fs::write(repo_dir.join("target/debug/cached.rlib"), b"warm-artifact")
        .expect("write warm artifact");
    std::fs::write(repo_dir.join("README.md"), "# Rocket\n").expect("write README");

    run_git(&repo_dir, &["add", "Cargo.toml", "README.md"]);
    run_git(&repo_dir, &["commit", "-m", "chore: initial commit"]);

    let file_url = format!("file://{}", bare_dir.display());
    run_git(&repo_dir, &["remote", "add", "origin", &file_url]);
    run_git(&repo_dir, &["push", "-u", "origin", "main"]);

    // Note: `file:///.../acme/rocket.git` is `RemoteScheme::File`, so let's double-check:
    // For GitHub owner/repo resolution on local file remotes OR `git@github.com:acme/rocket.git`
    // with `remote.origin.url` using `insteadOf` or `file://`: wait — let's check if `parse_git_remote`
    // extracts `owner/repo` on `file://` URLs too when path ends with `<owner>/<repo>.git`!
    (repo_dir, bare_dir)
}

#[test]
fn non_interactive_token_discovery_gh_config_dir_and_credential_fill_never_hangs() {
    let tmp = TempDir::new().expect("tempdir");
    let gh_cfg = tmp.path().join("gh-config");
    std::fs::create_dir_all(&gh_cfg).expect("mkdir gh-config");
    let hosts_path = gh_cfg.join("hosts.yml");
    std::fs::write(
        &hosts_path,
        "github.com:\n    oauth_token: gho_test_hosts_yml_token_777\n    user: bot\n",
    )
    .expect("write hosts.yml");

    let mut env = BTreeMap::new();
    env.insert(
        "GH_CONFIG_DIR".to_string(),
        gh_cfg.to_string_lossy().into_owned(),
    );

    let found = discover_github_token_with_options(&TokenDiscoveryOptions {
        host: "github.com".to_string(),
        env_overrides: Some(env),
        use_git_credential_helper: false,
        ..Default::default()
    })
    .expect("token from GH_CONFIG_DIR/hosts.yml");
    assert_eq!(found.token, "gho_test_hosts_yml_token_777");
    assert_eq!(found.source, TokenSource::GhHostsYml { path: hosts_path });
    assert_eq!(found.masked(), "gho_****");

    // Verify `query_git_credential_fill` never blocks on stdin/TTY even for an unknown host!
    let start = Instant::now();
    let cred = query_git_credential_fill(
        "https",
        "nonexistent-host-for-ozone-test.invalid",
        Some(tmp.path()),
        Duration::from_millis(1500),
    );
    assert!(cred.is_none());
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "git credential fill must terminate non-interactively without blocking"
    );

    // Verify `probe_github_auth` diagnostic struct redacts secret tokens.
    let probe = probe_github_auth(None, Some("origin"), Some("ghp_super_secret_987654321"));
    assert!(probe.authenticated);
    assert_eq!(probe.token_source, Some(TokenSource::Explicit));
    assert_eq!(probe.masked_token.as_deref(), Some("ghp_****"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end_pr_create_3_branch_pr_stack_sync_retarget_gc_rebase_and_ci_logs() {
    let tmp = TempDir::new().expect("tempdir");
    let (repo_dir, bare_dir) = init_repo_with_bare_github_named_origin(tmp.path());

    let token = "gho_mock_api_token_milestone3";
    let mock_state = Arc::new(Mutex::new(MockGitHubState::new(
        token,
        Some(bare_dir.clone()),
    )));
    let api_url = spawn_mock_github_api(Arc::clone(&mock_state)).await;

    // Verify GraphQL client endpoint works against mock server too.
    let direct_client = GitHubClient::new(&api_url, Some(token.to_string()));
    let gql_res = direct_client
        .graphql("query { viewer { login } }", json!({}))
        .expect("graphql query");
    assert_eq!(gql_res["data"]["viewer"]["login"], "ozone-bot");

    // 1. Test `pr_create` (push + inferred commit title/body + `Change-Id:` + idempotent update)
    let main_oz = OzoneRepo::discover(Some(&repo_dir)).expect("discover main repo");
    let ws_single = spawn_workspace(
        &main_oz,
        &SpawnWorkspaceOptions {
            name: "ws-single".to_string(),
            branch: Some("feat/single-pr".to_string()),
            from_ref: Some("main".to_string()),
            backend: Some(BackendKind::CasHardlink),
            scope: Vec::new(),
            deny: Vec::new(),
            warm: false,
        },
    )
    .expect("spawn ws-single");

    std::fs::write(
        ws_single.workspace.workspace_path.join("single.txt"),
        "single feature payload\n",
    )
    .expect("write single.txt");
    let ws_single_oz =
        OzoneRepo::discover(Some(&ws_single.workspace.workspace_path)).expect("discover ws-single");
    let single_commit = commit_changes(
        &ws_single_oz,
        &CommitOptions {
            message: "feat(single): implement standalone endpoint\n\nDetailed body context."
                .to_string(),
            ..Default::default()
        },
    )
    .expect("commit single");

    let ws_single_oz =
        OzoneRepo::discover(Some(&ws_single.workspace.workspace_path)).expect("re-discover");
    let pr1_out = pr_create(
        &ws_single_oz,
        &PrCreateOptions {
            base: Some("main".to_string()),
            auth_token: Some(token.to_string()),
            api_url: Some(api_url.clone()),
            ..Default::default()
        },
        test_actor(),
    )
    .expect("pr_create first call");
    assert!(pr1_out.created);
    assert!(pr1_out.pushed);
    assert_eq!(pr1_out.pull_request.number, 1);
    assert_eq!(
        pr1_out.pull_request.title,
        "feat(single): implement standalone endpoint"
    );
    assert!(
        pr1_out
            .pull_request
            .body
            .as_deref()
            .unwrap_or_default()
            .contains(&format!("Change-Id: {}", single_commit.change_id))
    );

    // Re-running `pr_create` updates PR #1 idempotently instead of creating a duplicate!
    let pr1_update = pr_create(
        &ws_single_oz,
        &PrCreateOptions {
            title: Some("feat(single): updated title".to_string()),
            base: Some("main".to_string()),
            no_push: true,
            auth_token: Some(token.to_string()),
            api_url: Some(api_url.clone()),
            ..Default::default()
        },
        test_actor(),
    )
    .expect("pr_create idempotent update");
    assert!(!pr1_update.created);
    assert_eq!(pr1_update.pull_request.number, 1);
    assert_eq!(pr1_update.pull_request.title, "feat(single): updated title");

    // Close PR #1 & clean up `ws-single` before building the 3-branch stack scenario.
    {
        let mut guard = mock_state.lock().unwrap();
        guard.prs[0].state = "closed".to_string();
    }
    oz_cow::remove_workspace(
        &main_oz,
        "ws-single",
        &oz_cow::RemoveWorkspaceOptions {
            force: true,
            delete_branch: true,
        },
    )
    .expect("remove ws-single");

    // 2. Build a 3-branch stack (`main <- stack-1 <- stack-2 <- stack-3`) across 3 CoW workspaces!
    let main_oz = OzoneRepo::discover(Some(&repo_dir)).expect("discover main");
    let ws1 = spawn_workspace(
        &main_oz,
        &SpawnWorkspaceOptions {
            name: "ws-stack-1".to_string(),
            branch: Some("stack-1".to_string()),
            from_ref: Some("main".to_string()),
            backend: Some(BackendKind::CasHardlink),
            scope: Vec::new(),
            deny: Vec::new(),
            warm: true,
        },
    )
    .expect("spawn ws-stack-1");
    std::fs::write(ws1.workspace.workspace_path.join("part1.rs"), "pub const P1: u8 = 1;\n")
        .expect("write part1.rs");
    let ws1_oz = OzoneRepo::discover(Some(&ws1.workspace.workspace_path)).expect("discover ws1");
    // Record a checkpoint on `ws-stack-1` so we can verify `oz sync` prunes merged checkpoints!
    let cp_out = checkpoint(
        &ws1_oz,
        "oz checkpoint",
        Some("before committing stack-1"),
        test_actor(),
        None,
    )
    .expect("checkpoint ws-stack-1");
    assert_eq!(cp_out.checkpoint_ref, "refs/ozone/checkpoints/ws-stack-1");
    let c_s1 = commit_changes(
        &ws1_oz,
        &CommitOptions {
            message: "feat(stack): part 1 auth model".to_string(),
            ..Default::default()
        },
    )
    .expect("commit stack-1");

    let main_oz = OzoneRepo::discover(Some(&repo_dir)).expect("discover main");
    let ws2 = spawn_workspace(
        &main_oz,
        &SpawnWorkspaceOptions {
            name: "ws-stack-2".to_string(),
            branch: Some("stack-2".to_string()),
            from_ref: Some("stack-1".to_string()),
            backend: Some(BackendKind::CasHardlink),
            scope: Vec::new(),
            deny: Vec::new(),
            warm: true,
        },
    )
    .expect("spawn ws-stack-2");
    std::fs::write(ws2.workspace.workspace_path.join("part2.rs"), "pub const P2: u8 = 2;\n")
        .expect("write part2.rs");
    let ws2_oz = OzoneRepo::discover(Some(&ws2.workspace.workspace_path)).expect("discover ws2");
    let c_s2 = commit_changes(
        &ws2_oz,
        &CommitOptions {
            message: "feat(stack): part 2 token handler".to_string(),
            ..Default::default()
        },
    )
    .expect("commit stack-2");

    let main_oz = OzoneRepo::discover(Some(&repo_dir)).expect("discover main");
    let ws3 = spawn_workspace(
        &main_oz,
        &SpawnWorkspaceOptions {
            name: "ws-stack-3".to_string(),
            branch: Some("stack-3".to_string()),
            from_ref: Some("stack-2".to_string()),
            backend: Some(BackendKind::CasHardlink),
            scope: Vec::new(),
            deny: Vec::new(),
            warm: true,
        },
    )
    .expect("spawn ws-stack-3");
    std::fs::write(ws3.workspace.workspace_path.join("part3.rs"), "pub const P3: u8 = 3;\n")
        .expect("write part3.rs");
    let ws3_oz = OzoneRepo::discover(Some(&ws3.workspace.workspace_path)).expect("discover ws3");
    let c_s3 = commit_changes(
        &ws3_oz,
        &CommitOptions {
            message: "feat(stack): part 3 cli command".to_string(),
            ..Default::default()
        },
    )
    .expect("commit stack-3");

    // Run `pr_stack` from the middle workspace (`ws-stack-2`) to verify full stack auto-discovery
    // (`stack-1` ancestor + `stack-3` descendant!), atomic push, base chain, and cross-link tables!
    let ws2_oz = OzoneRepo::discover(Some(&ws2.workspace.workspace_path)).expect("discover ws2");
    let stack_out = pr_stack(
        &ws2_oz,
        &PrStackOptions {
            base: Some("main".to_string()),
            branches: Vec::new(),
            draft: false,
            no_push: false,
            remote: Some("origin".to_string()),
            auth_token: Some(token.to_string()),
            api_url: Some(api_url.clone()),
            ..Default::default()
        },
        test_actor(),
    )
    .expect("3-branch pr_stack");

    assert!(stack_out.pushed);
    assert!(stack_out.atomic_push_op_id.is_some());
    assert_eq!(stack_out.items.len(), 3);
    assert_eq!(stack_out.items[0].head_branch, "stack-1");
    assert_eq!(stack_out.items[0].base_branch, "main");
    assert_eq!(stack_out.items[1].head_branch, "stack-2");
    assert_eq!(stack_out.items[1].base_branch, "stack-1");
    assert_eq!(stack_out.items[2].head_branch, "stack-3");
    assert_eq!(stack_out.items[2].base_branch, "stack-2");

    let pr_s1_num = stack_out.items[0].number;
    let pr_s2_num = stack_out.items[1].number;
    let pr_s3_num = stack_out.items[2].number;

    // Verify all 3 PR bodies on the mock GitHub server contain `<!-- oz-stack-start --> ... <!-- oz-stack-end -->`
    // cross-linking all 3 PRs with `👉` on the owning PR's row!
    {
        let guard = mock_state.lock().unwrap();
        for pr_num in [pr_s1_num, pr_s2_num, pr_s3_num] {
            let pr = guard
                .prs
                .iter()
                .find(|p| p.number == pr_num)
                .expect("stack PR stored");
            let body = pr.body.as_deref().expect("PR body");
            assert!(body.contains(OZ_STACK_START));
            assert!(body.contains(OZ_STACK_END));
            assert!(body.contains(&format!("[#{pr_s1_num}]")));
            assert!(body.contains(&format!("[#{pr_s2_num}]")));
            assert!(body.contains(&format!("[#{pr_s3_num}]")));
            assert!(
                body.contains(&format!("| 👉 | [#{pr_num}](")),
                "PR #{pr_num} body must mark itself with 👉:\n{body}"
            );
        }
    }

    // 3. Simulate GitHub SQUASH-MERGING PR `stack-1` into `main` (plus an extra commit on `main`)!
    // Squash-merge is the hardest real-world GitHub case because `c_s1.commit_oid` is NOT a git
    // ancestor of `new_remote_main`!
    let merger = tmp.path().join("github-squash-merger");
    run_git(
        tmp.path(),
        &[
            "clone",
            bare_dir.to_str().expect("utf8"),
            merger.to_str().expect("utf8"),
        ],
    );
    run_git(&merger, &["config", "user.name", "GitHub Merge Queue"]);
    run_git(&merger, &["config", "user.email", "noreply@github.com"]);
    run_git(&merger, &["checkout", "main"]);
    std::fs::write(merger.join("part1.rs"), "pub const P1: u8 = 1;\n").expect("write squashed p1");
    std::fs::write(merger.join("CHANGELOG.md"), "- Merged stack-1 (#2)\n")
        .expect("write changelog");
    run_git(&merger, &["add", "part1.rs", "CHANGELOG.md"]);
    run_git(
        &merger,
        &[
            "commit",
            "-m",
            &format!("feat(stack): part 1 auth model (#{pr_s1_num})\n\nChange-Id: {}", c_s1.change_id),
        ],
    );
    let new_remote_main = run_git(&merger, &["rev-parse", "HEAD"]);
    run_git(&merger, &["push", "origin", "main"]);

    {
        let mut guard = mock_state.lock().unwrap();
        let pr1 = guard
            .prs
            .iter_mut()
            .find(|p| p.number == pr_s1_num)
            .expect("find pr s1");
        pr1.state = "closed".to_string();
        pr1.merged = true;
        pr1.merged_at = Some("2026-11-18T14:00:00Z".to_string());
        pr1.merge_commit_sha = Some(new_remote_main.clone());
    }

    // 4. Run `sync_repo` (`oz sync`)!
    let main_oz = OzoneRepo::discover(Some(&repo_dir)).expect("discover main before sync");
    let sync_out = sync_repo(
        &main_oz,
        &SyncOptions {
            remote: Some("origin".to_string()),
            base_branch: Some("main".to_string()),
            auth_token: Some(token.to_string()),
            api_url: Some(api_url.clone()),
            ..Default::default()
        },
        test_actor(),
    )
    .expect("sync_repo after PR #1 squash-merge");

    // Verify (a) `main` fast-forwarded to `new_remote_main` & materialized `part1.rs` + `CHANGELOG.md`!
    assert!(sync_out.fetched);
    assert!(sync_out.fast_forwarded);
    assert_eq!(sync_out.base_oid.as_deref(), Some(new_remote_main.as_str()));
    assert!(repo_dir.join("part1.rs").is_file());
    assert!(repo_dir.join("CHANGELOG.md").is_file());

    // Verify (b) PR for `stack-2` had its `base` retargeted on GitHub from `stack-1` -> `main`!
    assert!(sync_out.merged_branches.contains(&"stack-1".to_string()));
    assert_eq!(sync_out.retargeted_prs.len(), 1);
    assert_eq!(sync_out.retargeted_prs[0].number, pr_s2_num);
    assert_eq!(sync_out.retargeted_prs[0].head_branch, "stack-2");
    assert_eq!(sync_out.retargeted_prs[0].old_base, "stack-1");
    assert_eq!(sync_out.retargeted_prs[0].new_base, "main");
    {
        let guard = mock_state.lock().unwrap();
        let pr2 = guard
            .prs
            .iter()
            .find(|p| p.number == pr_s2_num)
            .expect("pr2");
        assert_eq!(pr2.base.ref_name, "main");
        assert!(pr2.body.as_deref().unwrap().contains("Merged"));
    }

    // Verify (c) Merged CoW workspace `ws-stack-1` was GC'd, `stack-1` branch deleted, AND checkpoint pruned!
    assert!(sync_out.removed_workspaces.contains(&"ws-stack-1".to_string()));
    assert!(!ws1.workspace.workspace_path.exists());
    assert!(
        sync_out
            .pruned_checkpoints
            .contains(&"refs/ozone/checkpoints/ws-stack-1".to_string())
    );
    let remaining_ws_names: Vec<String> = list_workspaces(&main_oz)
        .expect("list_workspaces")
        .into_iter()
        .map(|w| w.state.name)
        .collect();
    assert_eq!(
        remaining_ws_names,
        vec!["ws-stack-2".to_string(), "ws-stack-3".to_string()]
    );

    // Verify (d) Downstream branches `stack-2` and `stack-3` rebased cleanly onto `new_remote_main`
    // preserving their `Change-Id:` trailers and syncing their CoW workspaces (`CHANGELOG.md` present)!
    assert!(sync_out.rebased_branches.contains(&"stack-2".to_string()));
    assert!(sync_out.rebased_branches.contains(&"stack-3".to_string()));

    let stack2_parent = run_git(&repo_dir, &["rev-parse", "stack-2^"]);
    let new_stack2_tip = run_git(&repo_dir, &["rev-parse", "stack-2"]);
    let stack3_parent = run_git(&repo_dir, &["rev-parse", "stack-3^"]);
    let new_stack3_tip = run_git(&repo_dir, &["rev-parse", "stack-3"]);
    assert_eq!(
        stack2_parent, new_remote_main,
        "rebased stack-2 must sit directly on top of fast-forwarded main"
    );
    assert_eq!(
        stack3_parent, new_stack2_tip,
        "rebased stack-3 must sit directly on top of rebased stack-2"
    );
    assert_ne!(new_stack2_tip, c_s2.commit_oid);
    assert_ne!(new_stack3_tip, c_s3.commit_oid);
    assert!(
        run_git(&repo_dir, &["log", "-1", "--format=%B", "stack-2"])
            .contains(&format!("Change-Id: {}", c_s2.change_id))
    );
    assert!(
        run_git(&repo_dir, &["log", "-1", "--format=%B", "stack-3"])
            .contains(&format!("Change-Id: {}", c_s3.change_id))
    );
    assert!(
        ws2.workspace.workspace_path.join("CHANGELOG.md").is_file(),
        "CoW workspace ws-stack-2 must receive CHANGELOG.md from updated main"
    );
    assert!(
        ws3.workspace.workspace_path.join("CHANGELOG.md").is_file(),
        "CoW workspace ws-stack-3 must receive CHANGELOG.md from updated main"
    );

    // Verify (e) Quiescent `oz-warm` baseline refreshed at `new_remote_main`!
    assert!(sync_out.baseline_refreshed);
    let captured_baseline = find_baseline(&main_oz.layout.ozone_dir, &new_remote_main)
        .expect("find_baseline")
        .expect("baseline present for new main");
    assert_eq!(captured_baseline.metadata.commit_oid, new_remote_main);

    // 5. Test `query_ci_status` (`oz ci`) with passing + plain-text failing + PKZIP failing job logs!
    {
        let mut guard = mock_state.lock().unwrap();
        guard.check_runs_by_ref.insert(
            new_stack2_tip.clone(),
            vec![
                GitHubCheckRun {
                    id: 1001,
                    name: "pure-rust-guard".to_string(),
                    status: "completed".to_string(),
                    conclusion: Some("success".to_string()),
                    details_url: None,
                    html_url: None,
                    output: GitHubCheckRunOutput::default(),
                },
                GitHubCheckRun {
                    id: 1002,
                    name: "Build & Test (ubuntu-latest)".to_string(),
                    status: "completed".to_string(),
                    conclusion: Some("failure".to_string()),
                    details_url: Some(
                        "https://github.com/acme/rocket/actions/runs/500/job/9001".to_string(),
                    ),
                    html_url: None,
                    output: GitHubCheckRunOutput::default(),
                },
                GitHubCheckRun {
                    id: 1003,
                    name: "Build & Test (macos-latest)".to_string(),
                    status: "completed".to_string(),
                    conclusion: Some("failure".to_string()),
                    details_url: Some(
                        "https://github.com/acme/rocket/actions/runs/500/job/9002".to_string(),
                    ),
                    html_url: None,
                    output: GitHubCheckRunOutput::default(),
                },
            ],
        );
        guard.workflow_runs.push(GitHubWorkflowRun {
            id: 500,
            name: Some("CI".to_string()),
            head_branch: Some("stack-2".to_string()),
            head_sha: new_stack2_tip.clone(),
            status: "completed".to_string(),
            conclusion: Some("failure".to_string()),
            html_url: None,
            ..Default::default()
        });
        guard.jobs_by_run.insert(
            500,
            vec![
                GitHubActionsJob {
                    id: 9001,
                    run_id: Some(500),
                    name: "Build & Test (ubuntu-latest)".to_string(),
                    status: "completed".to_string(),
                    conclusion: Some("failure".to_string()),
                    html_url: None,
                    steps: vec![
                        GitHubJobStep {
                            name: "Checkout".to_string(),
                            status: "completed".to_string(),
                            conclusion: Some("success".to_string()),
                            number: 1,
                        },
                        GitHubJobStep {
                            name: "Run cargo test".to_string(),
                            status: "completed".to_string(),
                            conclusion: Some("failure".to_string()),
                            number: 2,
                        },
                    ],
                },
                GitHubActionsJob {
                    id: 9002,
                    run_id: Some(500),
                    name: "Build & Test (macos-latest)".to_string(),
                    status: "completed".to_string(),
                    conclusion: Some("failure".to_string()),
                    html_url: None,
                    steps: Vec::new(),
                },
            ],
        );

        // Plain-text log with timestamps, ANSI escapes, and Rust compiler error for job 9001.
        let plain_log = b"2026-11-18T14:10:01.0000001Z ##[group]Run cargo test\n\
2026-11-18T14:10:02.0000002Z    Compiling rocket v0.1.0\n\
2026-11-18T14:10:03.0000003Z \x1b[38;5;9merror[E0308]\x1b[0m: mismatched types\n\
2026-11-18T14:10:03.0000004Z   --> src/part2.rs:19:12\n\
2026-11-18T14:10:04.0000005Z ##[endgroup]\n\
2026-11-18T14:10:04.0000006Z ##[error]Process completed with exit code 101.\n";
        guard.job_logs.insert(9001, plain_log.to_vec());

        // ZIP archive log with `##[group]` step detection + test panic for job 9002.
        let zip_inner = b"2026-11-18T14:11:00.0000000Z ##[group]Verify integration suite\n\
2026-11-18T14:11:01.0000000Z running 1 test\n\
2026-11-18T14:11:02.0000000Z thread 'auth_roundtrip' panicked at src/part2.rs:44:5:\n\
2026-11-18T14:11:02.0000001Z assertion `left == right` failed\n\
2026-11-18T14:11:02.0000002Z test auth_roundtrip ... FAILED\n\
2026-11-18T14:11:03.0000000Z ##[endgroup]\n";
        let zip_bytes = build_zip_deflate("macos/5_Verify integration suite.txt", zip_inner);
        guard.job_logs.insert(9002, zip_bytes);
    }

    let ws2_refreshed =
        OzoneRepo::discover(Some(&ws2.workspace.workspace_path)).expect("discover ws2");
    let ci_report = query_ci_status(
        &ws2_refreshed,
        &CiQueryOptions {
            auth_token: Some(token.to_string()),
            api_url: Some(api_url.clone()),
            ..Default::default()
        },
    )
    .expect("query_ci_status");

    assert_eq!(ci_report.ref_name, "stack-2");
    assert_eq!(ci_report.commit_sha, new_stack2_tip);
    assert_eq!(ci_report.pr_number, Some(pr_s2_num));
    assert_eq!(ci_report.overall_status, "failing");
    assert!(!ci_report.all_passed);
    assert_eq!(ci_report.total_count, 3);
    assert_eq!(ci_report.failing_count, 2);

    let ubuntu_check = ci_report
        .checks
        .iter()
        .find(|c| c.name == "Build & Test (ubuntu-latest)")
        .expect("ubuntu check");
    assert_eq!(ubuntu_check.failing_step.as_deref(), Some("Run cargo test"));
    let ubuntu_excerpt = ubuntu_check.log_excerpt.as_deref().expect("ubuntu excerpt");
    assert!(ubuntu_excerpt.contains("error[E0308]: mismatched types"));
    assert!(ubuntu_excerpt.contains("--> src/part2.rs:19:12"));
    assert!(!ubuntu_excerpt.contains('\x1b'));
    assert!(!ubuntu_excerpt.contains("2026-11-18T"));

    let macos_check = ci_report
        .checks
        .iter()
        .find(|c| c.name == "Build & Test (macos-latest)")
        .expect("macos check");
    assert_eq!(
        macos_check.failing_step.as_deref(),
        Some("Verify integration suite")
    );
    let macos_excerpt = macos_check.log_excerpt.as_deref().expect("macos excerpt");
    assert!(macos_excerpt.contains("thread 'auth_roundtrip' panicked at src/part2.rs:44:5"));
    assert!(macos_excerpt.contains("test auth_roundtrip ... FAILED"));

    // 6. Finally, verify graceful OFFLINE `sync_repo` (no token, no API server) when `stack-2` merges
    // into `origin/main` via fast-forward on the bare repo!
    run_git(
        &ws2.workspace.workspace_path,
        &["push", "--force", "origin", "stack-2:main"],
    );
    let main_oz = OzoneRepo::discover(Some(&repo_dir)).expect("discover main for offline sync");
    let offline_sync = sync_repo(
        &main_oz,
        &SyncOptions {
            remote: Some("origin".to_string()),
            base_branch: Some("main".to_string()),
            ..Default::default()
        },
        test_actor(),
    )
    .expect("offline sync_repo");

    assert!(offline_sync.fast_forwarded);
    assert_eq!(
        offline_sync.base_oid.as_deref(),
        Some(new_stack2_tip.as_str())
    );
    assert!(
        offline_sync
            .removed_workspaces
            .contains(&"ws-stack-2".to_string())
    );
    assert_eq!(
        run_git(&repo_dir, &["rev-parse", "stack-3^"]),
        new_stack2_tip,
        "stack-3 remains cleanly based on fast-forwarded main"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_repo_merge_commit_rebases_via_oz_integrate_and_preserves_untouched_workspace() {
    let tmp = TempDir::new().expect("tempdir");
    let (repo_dir, bare_dir) = init_repo_with_bare_github_named_origin(tmp.path());

    let token = "gho_merge_commit_test_token";
    let mock_state = Arc::new(Mutex::new(MockGitHubState::new(
        token,
        Some(bare_dir.clone()),
    )));
    let api_url = spawn_mock_github_api(Arc::clone(&mock_state)).await;

    let main_oz = OzoneRepo::discover(Some(&repo_dir)).expect("discover main");
    // Spawn an untouched workspace with 0 commits — `sync_repo` MUST NOT falsely delete it!
    let ws_untouched = spawn_workspace(
        &main_oz,
        &SpawnWorkspaceOptions {
            name: "ws-untouched".to_string(),
            branch: Some("feat/not-started".to_string()),
            from_ref: Some("main".to_string()),
            backend: Some(BackendKind::CasHardlink),
            scope: Vec::new(),
            deny: Vec::new(),
            warm: false,
        },
    )
    .expect("spawn ws-untouched");

    // Spawn 2-branch stack `main <- s1 <- s2`
    let ws_s1 = spawn_workspace(
        &main_oz,
        &SpawnWorkspaceOptions {
            name: "ws-s1".to_string(),
            branch: Some("s1".to_string()),
            from_ref: Some("main".to_string()),
            backend: Some(BackendKind::CasHardlink),
            scope: Vec::new(),
            deny: Vec::new(),
            warm: false,
        },
    )
    .expect("spawn ws-s1");
    std::fs::write(ws_s1.workspace.workspace_path.join("m1.txt"), "m1\n").expect("write m1");
    let ws_s1_oz = OzoneRepo::discover(Some(&ws_s1.workspace.workspace_path)).expect("discover s1");
    let _ = commit_changes(
        &ws_s1_oz,
        &CommitOptions {
            message: "feat: s1".to_string(),
            ..Default::default()
        },
    )
    .expect("commit s1");

    let main_oz = OzoneRepo::discover(Some(&repo_dir)).expect("discover main");
    let ws_s2 = spawn_workspace(
        &main_oz,
        &SpawnWorkspaceOptions {
            name: "ws-s2".to_string(),
            branch: Some("s2".to_string()),
            from_ref: Some("s1".to_string()),
            backend: Some(BackendKind::CasHardlink),
            scope: Vec::new(),
            deny: Vec::new(),
            warm: false,
        },
    )
    .expect("spawn ws-s2");
    std::fs::write(ws_s2.workspace.workspace_path.join("m2.txt"), "m2\n").expect("write m2");
    let ws_s2_oz = OzoneRepo::discover(Some(&ws_s2.workspace.workspace_path)).expect("discover s2");
    let _ = commit_changes(
        &ws_s2_oz,
        &CommitOptions {
            message: "feat: s2".to_string(),
            ..Default::default()
        },
    )
    .expect("commit s2");

    let ws_s2_oz = OzoneRepo::discover(Some(&ws_s2.workspace.workspace_path)).expect("discover s2");
    let stack_out = pr_stack(
        &ws_s2_oz,
        &PrStackOptions {
            base: Some("main".to_string()),
            branches: vec!["s1".to_string(), "s2".to_string()],
            auth_token: Some(token.to_string()),
            api_url: Some(api_url.clone()),
            ..Default::default()
        },
        test_actor(),
    )
    .expect("pr_stack s1, s2");
    assert_eq!(stack_out.items.len(), 2);

    // Merge `s1` into `main` using a real `--no-ff` merge commit on remote!
    let merger = tmp.path().join("noff-merger");
    run_git(
        tmp.path(),
        &[
            "clone",
            bare_dir.to_str().expect("utf8"),
            merger.to_str().expect("utf8"),
        ],
    );
    run_git(&merger, &["config", "user.name", "Merger"]);
    run_git(&merger, &["config", "user.email", "m@ozone.dev"]);
    run_git(&merger, &["checkout", "main"]);
    run_git(
        &merger,
        &["merge", "--no-ff", "origin/s1", "-m", "Merge pull request #1 from acme/s1"],
    );
    let merged_main_oid = run_git(&merger, &["rev-parse", "HEAD"]);
    run_git(&merger, &["push", "origin", "main"]);

    {
        let mut guard = mock_state.lock().unwrap();
        guard.prs[0].state = "closed".to_string();
        guard.prs[0].merged = true;
    }

    let main_oz = OzoneRepo::discover(Some(&repo_dir)).expect("discover main");
    let sync_out = sync_repo(
        &main_oz,
        &SyncOptions {
            remote: Some("origin".to_string()),
            base_branch: Some("main".to_string()),
            auth_token: Some(token.to_string()),
            api_url: Some(api_url),
            ..Default::default()
        },
        test_actor(),
    )
    .expect("sync_repo after --no-ff merge");

    assert!(sync_out.fast_forwarded);
    assert_eq!(sync_out.base_oid.as_deref(), Some(merged_main_oid.as_str()));
    assert_eq!(sync_out.retargeted_prs.len(), 1);
    assert_eq!(sync_out.retargeted_prs[0].head_branch, "s2");
    assert_eq!(sync_out.retargeted_prs[0].new_base, "main");
    assert!(sync_out.removed_workspaces.contains(&"ws-s1".to_string()));
    assert!(!sync_out.removed_workspaces.contains(&"ws-untouched".to_string()));
    assert!(ws_untouched.workspace.workspace_path.is_dir());
    assert!(sync_out.rebased_branches.contains(&"s2".to_string()));
    assert_eq!(run_git(&repo_dir, &["rev-parse", "s2^"]), merged_main_oid);
}

