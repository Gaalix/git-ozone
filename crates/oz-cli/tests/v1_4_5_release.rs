// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end CLI tests for `oz release list|view|create|edit` against a local mock GitHub REST API.

#![allow(clippy::too_many_lines)]

use clap::Parser;
use oz_cli::Cli;
use oz_github::{GitHubRelease, GitHubReleaseAsset, GitHubUser};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn oz_bin() -> &'static str {
    env!("CARGO_BIN_EXE_oz")
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

fn init_github_repo() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    git_ok(&repo, &["init", "-b", "main"]);
    git_ok(&repo, &["config", "user.name", "Ozone Dev"]);
    git_ok(&repo, &["config", "user.email", "dev@example.com"]);
    git_ok(&repo, &["config", "core.autocrlf", "false"]);
    git_ok(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/demo.git",
        ],
    );
    fs::write(repo.join("README.md"), "# demo\n").unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "chore: init"]);
    (tmp, repo)
}

fn run_oz_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(oz_bin());
    cmd.arg("-C").arg(dir).args(args).env("OZ_AGENT", "1");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("run oz")
}

fn run_oz_stdin(dir: &Path, args: &[&str], envs: &[(&str, &str)], stdin_data: &str) -> Output {
    let mut cmd = Command::new(oz_bin());
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .env("OZ_AGENT", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn oz");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin_data.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait oz")
}

fn parse_json_stdout(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "failed to parse JSON stdout ({e}):\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[derive(Debug, Clone)]
struct CapturedRequest {
    method: String,
    path: String,
    query: String,
    body: String,
}

struct MockReleaseServer {
    api_url: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    latest_404: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for MockReleaseServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn spawn_mock_release_server() -> MockReleaseServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_bg = Arc::clone(&stop);
    let requests: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_bg = Arc::clone(&requests);
    let latest_404 = Arc::new(AtomicBool::new(false));
    let latest_404_bg = Arc::clone(&latest_404);

    let handle = thread::spawn(move || {
        let rel_latest = GitHubRelease {
            id: 1001,
            tag_name: "v1.4.5".to_string(),
            target_commitish: Some("main".to_string()),
            name: Some("Ozone v1.4.5".to_string()),
            body: Some("## Highlights\n- Added `oz release` subcommands\n- Improved CI watch".to_string()),
            draft: false,
            prerelease: false,
            html_url: "https://github.com/acme/demo/releases/tag/v1.4.5".to_string(),
            author: Some(GitHubUser {
                login: "gal".to_string(),
            }),
            created_at: Some("2026-11-28T10:00:00Z".to_string()),
            published_at: Some("2026-11-28T12:30:00Z".to_string()),
            assets: vec![
                GitHubReleaseAsset {
                    id: 5001,
                    name: "oz-v1.4.5-x86_64-unknown-linux-gnu.tar.gz".to_string(),
                    content_type: "application/gzip".to_string(),
                    size: 12_582_912,
                    download_count: 42,
                    browser_download_url: "https://github.com/acme/demo/releases/download/v1.4.5/oz-v1.4.5-x86_64-unknown-linux-gnu.tar.gz".to_string(),
                    ..Default::default()
                },
                GitHubReleaseAsset {
                    id: 5002,
                    name: "SHA256SUMS".to_string(),
                    content_type: "text/plain".to_string(),
                    size: 512,
                    download_count: 1,
                    browser_download_url: "https://github.com/acme/demo/releases/download/v1.4.5/SHA256SUMS".to_string(),
                    ..Default::default()
                },
            ],
        };

        let rel_prerelease = GitHubRelease {
            id: 1002,
            tag_name: "pkg/v1.5.0-rc.1".to_string(),
            target_commitish: Some("next".to_string()),
            name: Some("v1.5.0 RC 1".to_string()),
            body: Some("Prerelease testing notes".to_string()),
            draft: false,
            prerelease: true,
            html_url: "https://github.com/acme/demo/releases/tag/pkg%2Fv1.5.0-rc.1".to_string(),
            author: Some(GitHubUser {
                login: "gal".to_string(),
            }),
            created_at: Some("2026-11-27T08:00:00Z".to_string()),
            published_at: Some("2026-11-27T09:00:00Z".to_string()),
            assets: vec![GitHubReleaseAsset {
                id: 5003,
                name: "oz-rc.tar.gz".to_string(),
                content_type: "application/gzip".to_string(),
                size: 2048,
                download_count: 3,
                browser_download_url:
                    "https://github.com/acme/demo/releases/download/pkg%2Fv1.5.0-rc.1/oz-rc.tar.gz"
                        .to_string(),
                ..Default::default()
            }],
        };

        let rel_draft = GitHubRelease {
            id: 1003,
            tag_name: "v1.4.6-draft".to_string(),
            target_commitish: Some("main".to_string()),
            name: Some("Upcoming v1.4.6".to_string()),
            body: Some("Initial draft body".to_string()),
            draft: true,
            prerelease: false,
            html_url: "https://github.com/acme/demo/releases/tag/untagged-draft".to_string(),
            author: Some(GitHubUser {
                login: "gal".to_string(),
            }),
            created_at: Some("2026-11-28T13:00:00Z".to_string()),
            published_at: None,
            assets: Vec::new(),
        };

        while !stop_bg.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let _ = stream.set_nonblocking(false);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut req_line = String::new();
            if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                continue;
            }
            let parts: Vec<&str> = req_line.split_whitespace().collect();
            let method = parts.first().copied().unwrap_or("GET");
            let raw_target = parts.get(1).copied().unwrap_or("/");
            let (path, query) = raw_target.split_once('?').unwrap_or((raw_target, ""));

            let mut content_len = 0usize;
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
            let body_str = String::from_utf8_lossy(&body_bytes).to_string();

            requests_bg.lock().unwrap().push(CapturedRequest {
                method: method.to_string(),
                path: path.to_string(),
                query: query.to_string(),
                body: body_str.clone(),
            });

            let (status, resp_body): (&str, Vec<u8>) = match (method, path) {
                ("GET", "/repos/acme/demo/releases") => (
                    "200 OK",
                    serde_json::to_vec(&vec![
                        rel_draft.clone(),
                        rel_latest.clone(),
                        rel_prerelease.clone(),
                    ])
                    .unwrap(),
                ),
                ("GET", "/repos/acme/demo/releases/latest") => {
                    if latest_404_bg.load(Ordering::Relaxed) {
                        ("404 Not Found", b"{\"message\":\"Not Found\"}".to_vec())
                    } else {
                        ("200 OK", serde_json::to_vec(&rel_latest).unwrap())
                    }
                }
                ("GET", "/repos/acme/demo/releases/tags/v1.4.5") => {
                    ("200 OK", serde_json::to_vec(&rel_latest).unwrap())
                }
                ("GET", "/repos/acme/demo/releases/tags/pkg%2Fv1.5.0-rc.1") => {
                    ("200 OK", serde_json::to_vec(&rel_prerelease).unwrap())
                }
                ("POST", "/repos/acme/demo/releases") => {
                    let req: Value = serde_json::from_str(&body_str).unwrap_or_default();
                    let tag = req["tag_name"].as_str().unwrap_or("v0.0.0").to_string();
                    let draft = req["draft"].as_bool().unwrap_or(false);
                    let prerelease = req["prerelease"].as_bool().unwrap_or(false);
                    let generated = req["generate_release_notes"].as_bool().unwrap_or(false);
                    let body = match (req["body"].as_str(), generated) {
                        (Some(b), true) => Some(format!("{b}\n\n**Full Changelog**: auto")),
                        (Some(b), false) => Some(b.to_string()),
                        (None, true) => Some("**Full Changelog**: auto".to_string()),
                        (None, false) => None,
                    };
                    let created = GitHubRelease {
                        id: 2001,
                        tag_name: tag.clone(),
                        target_commitish: req["target_commitish"]
                            .as_str()
                            .map(str::to_string)
                            .or_else(|| Some("main".to_string())),
                        name: req["name"]
                            .as_str()
                            .map(str::to_string)
                            .or_else(|| Some(tag.clone())),
                        body,
                        draft,
                        prerelease,
                        html_url: format!("https://github.com/acme/demo/releases/tag/{tag}"),
                        author: Some(GitHubUser {
                            login: "gal".to_string(),
                        }),
                        created_at: Some("2026-11-28T14:00:00Z".to_string()),
                        published_at: (!draft).then(|| "2026-11-28T14:00:00Z".to_string()),
                        assets: Vec::new(),
                    };
                    ("201 Created", serde_json::to_vec(&created).unwrap())
                }
                ("PATCH", "/repos/acme/demo/releases/1001") => {
                    let req: Value = serde_json::from_str(&body_str).unwrap_or_default();
                    let mut updated = rel_latest.clone();
                    apply_release_patch(&mut updated, &req);
                    ("200 OK", serde_json::to_vec(&updated).unwrap())
                }
                ("PATCH", "/repos/acme/demo/releases/1003") => {
                    let req: Value = serde_json::from_str(&body_str).unwrap_or_default();
                    let mut updated = rel_draft.clone();
                    apply_release_patch(&mut updated, &req);
                    if !updated.draft && updated.published_at.is_none() {
                        updated.published_at = Some("2026-11-28T14:30:00Z".to_string());
                    }
                    updated.html_url = format!(
                        "https://github.com/acme/demo/releases/tag/{}",
                        updated.tag_name
                    );
                    ("200 OK", serde_json::to_vec(&updated).unwrap())
                }
                _ => ("404 Not Found", b"{\"message\":\"Not Found\"}".to_vec()),
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

    MockReleaseServer {
        api_url: format!("http://{addr}"),
        stop,
        requests,
        latest_404,
        handle: Some(handle),
    }
}

fn apply_release_patch(rel: &mut GitHubRelease, req: &Value) {
    if let Some(t) = req["tag_name"].as_str() {
        rel.tag_name = t.to_string();
    }
    if let Some(n) = req["name"].as_str() {
        rel.name = Some(n.to_string());
    }
    if let Some(b) = req["body"].as_str() {
        rel.body = Some(b.to_string());
    }
    if let Some(d) = req["draft"].as_bool() {
        rel.draft = d;
    }
    if let Some(p) = req["prerelease"].as_bool() {
        rel.prerelease = p;
    }
    if let Some(tgt) = req["target_commitish"].as_str() {
        rel.target_commitish = Some(tgt.to_string());
    }
}

#[test]
fn cli_parses_all_release_subcommands_and_aliases() {
    assert!(
        Cli::try_parse_from([
            "oz",
            "release",
            "list",
            "-L",
            "10",
            "--exclude-drafts",
            "--exclude-prereleases",
            "--remote",
            "origin",
            "--json"
        ])
        .is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "release", "view"]).is_ok());
    assert!(
        Cli::try_parse_from([
            "oz",
            "release",
            "view",
            "--latest",
            "--no-body",
            "--no-assets"
        ])
        .is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "release", "view", "v1.4.5", "--json"]).is_ok());
    assert!(
        Cli::try_parse_from(["oz", "release", "view", "v1.4.5", "--latest"]).is_err(),
        "tag and --latest must conflict"
    );
    assert!(
        Cli::try_parse_from([
            "oz",
            "release",
            "create",
            "v1.4.5",
            "-t",
            "Ozone v1.4.5",
            "-n",
            "Notes",
            "--generate-notes",
            "--target",
            "main",
            "-d",
            "-p",
            "--latest"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from(["oz", "release", "create", "v1.4.5", "--body", "Alias body"]).is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "release", "create", "v1.4.5", "--body-file", "-"]).is_ok());
    assert!(
        Cli::try_parse_from([
            "oz",
            "release",
            "edit",
            "v1.4.5",
            "--tag",
            "v1.4.6",
            "-t",
            "Title",
            "-F",
            "-",
            "--publish",
            "--no-prerelease",
            "--latest",
            "--target",
            "main"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from(["oz", "release", "edit", "v1.4.5", "--draft", "--publish"]).is_err(),
        "--draft and --publish must conflict"
    );
    assert!(
        Cli::try_parse_from([
            "oz",
            "release",
            "edit",
            "v1.4.5",
            "--prerelease",
            "--no-prerelease"
        ])
        .is_err(),
        "--prerelease and --no-prerelease must conflict"
    );
}

#[test]
fn release_list_human_and_json_with_filters() {
    let (_tmp, repo) = init_github_repo();
    let srv = spawn_mock_release_server();
    let envs = [
        ("OZ_GITHUB_API_URL", srv.api_url.as_str()),
        ("GITHUB_TOKEN", "test-token"),
    ];

    // Human mode: all releases
    let human = run_oz_env(&repo, &["release", "list", "--limit", "10"], &envs);
    assert!(
        human.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&human.stderr)
    );
    let out = String::from_utf8_lossy(&human.stdout);
    assert!(out.contains("Releases in acme/demo (3 shown):"), "{out}");
    assert!(out.contains("v1.4.5") && out.contains("[latest]"), "{out}");
    assert!(out.contains("Ozone v1.4.5"), "{out}");
    assert!(out.contains("2026-11-28"), "{out}");
    assert!(out.contains("2 assets"), "{out}");
    assert!(
        out.contains("https://github.com/acme/demo/releases/tag/v1.4.5"),
        "{out}"
    );
    assert!(
        out.contains("v1.4.6-draft") && out.contains("[draft]"),
        "{out}"
    );
    assert!(
        out.contains("pkg/v1.5.0-rc.1") && out.contains("[prerelease]") && out.contains("1 asset"),
        "{out}"
    );
    {
        let reqs = srv.requests.lock().unwrap();
        assert!(
            reqs.iter().any(|r| {
                r.method == "GET"
                    && r.path == "/repos/acme/demo/releases"
                    && r.query.contains("per_page=10")
            }),
            "expected GET /repos/acme/demo/releases?per_page=10, got: {reqs:?}"
        );
    }

    // JSON mode with `--exclude-drafts` and `--exclude-prereleases`
    let filtered = run_oz_env(
        &repo,
        &[
            "release",
            "list",
            "--exclude-drafts",
            "--exclude-prereleases",
            "--json",
        ],
        &envs,
    );
    assert!(filtered.status.success());
    let env = parse_json_stdout(&filtered);
    assert_eq!(env["schema"], "oz.v1");
    assert_eq!(env["ok"], true);
    assert_eq!(env["data"]["owner"], "acme");
    assert_eq!(env["data"]["repo"], "demo");
    assert_eq!(env["data"]["total_returned"], 1);
    let releases = env["data"]["releases"].as_array().unwrap();
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0]["tag_name"], "v1.4.5");
    assert_eq!(releases[0]["is_latest"], true);
    assert_eq!(releases[0]["asset_count"], 2);
}

#[test]
fn release_view_by_tag_latest_flags_and_404_hint() {
    let (_tmp, repo) = init_github_repo();
    let srv = spawn_mock_release_server();
    let envs = [
        ("OZ_GITHUB_API_URL", srv.api_url.as_str()),
        ("GITHUB_TOKEN", "test-token"),
    ];

    // 1. Default view (omitted tag -> /releases/latest) in human mode
    let view_latest = run_oz_env(&repo, &["release", "view"], &envs);
    assert!(
        view_latest.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&view_latest.stderr)
    );
    let human = String::from_utf8_lossy(&view_latest.stdout);
    assert!(
        human.contains("Release v1.4.5 [PUBLISHED] Ozone v1.4.5"),
        "{human}"
    );
    assert!(
        human.contains("url=https://github.com/acme/demo/releases/tag/v1.4.5"),
        "{human}"
    );
    assert!(
        human.contains("oz-v1.4.5-x86_64-unknown-linux-gnu.tar.gz (12.0 MiB, 42 downloads) -> https://github.com/acme/demo/releases/download/v1.4.5/oz-v1.4.5-x86_64-unknown-linux-gnu.tar.gz"),
        "{human}"
    );
    assert!(
        human.contains("SHA256SUMS (512 B, 1 download) -> https://github.com/acme/demo/releases/download/v1.4.5/SHA256SUMS"),
        "{human}"
    );
    assert!(human.contains("Added `oz release` subcommands"), "{human}");

    // 2. `--latest --json` verifies structured `ReleaseViewOutcome`
    let view_json = run_oz_env(&repo, &["release", "view", "--latest", "--json"], &envs);
    assert!(view_json.status.success());
    let env = parse_json_stdout(&view_json);
    assert_eq!(env["schema"], "oz.v1");
    assert_eq!(env["ok"], true);
    assert_eq!(env["data"]["id"], 1001);
    assert_eq!(env["data"]["tag_name"], "v1.4.5");
    assert_eq!(env["data"]["name"], "Ozone v1.4.5");
    assert_eq!(
        env["data"]["html_url"],
        "https://github.com/acme/demo/releases/tag/v1.4.5"
    );
    assert_eq!(env["data"]["draft"], false);
    assert_eq!(env["data"]["prerelease"], false);
    assert_eq!(env["data"]["target_commitish"], "main");
    assert_eq!(env["data"]["author"], "gal");
    assert_eq!(env["data"]["created_at"], "2026-11-28T10:00:00Z");
    assert_eq!(env["data"]["published_at"], "2026-11-28T12:30:00Z");
    assert!(
        env["data"]["body"]
            .as_str()
            .unwrap()
            .contains("Added `oz release`")
    );
    let assets = env["data"]["assets"].as_array().unwrap();
    assert_eq!(assets.len(), 2);
    assert_eq!(assets[0]["id"], 5001);
    assert_eq!(assets[0]["size"], 12_582_912);
    assert_eq!(assets[0]["download_count"], 42);
    assert_eq!(assets[0]["content_type"], "application/gzip");

    // 3. `--no-body` and `--no-assets` omit body & assets
    let compact = run_oz_env(
        &repo,
        &[
            "release",
            "view",
            "v1.4.5",
            "--no-body",
            "--no-assets",
            "--json",
        ],
        &envs,
    );
    assert!(compact.status.success());
    let compact_env = parse_json_stdout(&compact);
    assert!(compact_env["data"]["body"].is_null());
    assert!(compact_env["data"]["assets"].as_array().unwrap().is_empty());

    // 4. Tag with slash is URL-encoded (`/releases/tags/pkg%2Fv1.5.0-rc.1`)
    let encoded_tag = run_oz_env(
        &repo,
        &["release", "view", "pkg/v1.5.0-rc.1", "--json"],
        &envs,
    );
    assert!(encoded_tag.status.success());
    let enc_env = parse_json_stdout(&encoded_tag);
    assert_eq!(enc_env["data"]["id"], 1002);
    assert_eq!(enc_env["data"]["prerelease"], true);

    // 5. `/releases/latest` 404 yields a helpful `oz release list` recovery hint
    srv.latest_404.store(true, Ordering::Relaxed);
    let missing_latest = run_oz_env(&repo, &["release", "view", "--latest"], &envs);
    assert!(!missing_latest.status.success());
    let stderr = String::from_utf8_lossy(&missing_latest.stderr);
    assert!(stderr.contains("oz release list"), "stderr: {stderr}");

    let missing_latest_json = run_oz_env(&repo, &["release", "view", "--latest", "--json"], &envs);
    assert!(!missing_latest_json.status.success());
    let err_env = parse_json_stdout(&missing_latest_json);
    assert_eq!(err_env["ok"], false);
    assert!(
        err_env["recovery_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "oz release list")
    );
}

#[test]
fn release_create_with_generate_notes_file_stdin_draft_and_prerelease() {
    let (_tmp, repo) = init_github_repo();
    let srv = spawn_mock_release_server();
    let envs = [
        ("OZ_GITHUB_API_URL", srv.api_url.as_str()),
        ("GITHUB_TOKEN", "test-token"),
    ];

    // 1. `--generate-notes --draft --prerelease --target main --latest` (human mode)
    let created_human = run_oz_env(
        &repo,
        &[
            "release",
            "create",
            "v1.5.0-beta.1",
            "--title",
            "Ozone v1.5.0 Beta 1",
            "--generate-notes",
            "--target",
            "main",
            "--draft",
            "--prerelease",
            "--latest",
        ],
        &envs,
    );
    assert!(
        created_human.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&created_human.stderr)
    );
    let out = String::from_utf8_lossy(&created_human.stdout);
    assert!(
        out.contains("Created release v1.5.0-beta.1 [DRAFT, PRERELEASE]"),
        "{out}"
    );
    assert!(
        out.contains("https://github.com/acme/demo/releases/tag/v1.5.0-beta.1"),
        "{out}"
    );

    // Inspect captured POST payload
    {
        let reqs = srv.requests.lock().unwrap();
        let post = reqs
            .iter()
            .rev()
            .find(|r| r.method == "POST" && r.path == "/repos/acme/demo/releases")
            .expect("POST /releases");
        let payload: Value = serde_json::from_str(&post.body).unwrap();
        assert_eq!(payload["tag_name"], "v1.5.0-beta.1");
        assert_eq!(payload["name"], "Ozone v1.5.0 Beta 1");
        assert_eq!(payload["target_commitish"], "main");
        assert_eq!(payload["draft"], true);
        assert_eq!(payload["prerelease"], true);
        assert_eq!(payload["generate_release_notes"], true);
        assert_eq!(payload["make_latest"], "true");
    }

    // 2. `--notes-file <PATH>` (JSON mode)
    let notes_path = repo.join("release-notes.md");
    fs::write(&notes_path, "## Notes from file\n- Item A\n").unwrap();
    let created_file = run_oz_env(
        &repo,
        &[
            "release",
            "create",
            "v1.5.0",
            "--notes-file",
            notes_path.to_str().unwrap(),
            "--json",
        ],
        &envs,
    );
    assert!(created_file.status.success());
    let file_env = parse_json_stdout(&created_file);
    assert_eq!(file_env["ok"], true);
    assert_eq!(file_env["data"]["tag_name"], "v1.5.0");
    assert_eq!(file_env["data"]["body"], "## Notes from file\n- Item A\n");

    // 3. `--notes-file -` reading from stdin
    let created_stdin = run_oz_stdin(
        &repo,
        &["release", "create", "v1.5.1", "--notes-file", "-", "--json"],
        &envs,
        "## Stdin Notes\n- Piped cleanly\n",
    );
    assert!(created_stdin.status.success());
    let stdin_env = parse_json_stdout(&created_stdin);
    assert_eq!(stdin_env["ok"], true);
    assert_eq!(stdin_env["data"]["tag_name"], "v1.5.1");
    assert_eq!(
        stdin_env["data"]["body"],
        "## Stdin Notes\n- Piped cleanly\n"
    );
}

#[test]
fn release_edit_published_and_draft_fallback_human_and_json() {
    let (_tmp, repo) = init_github_repo();
    let srv = spawn_mock_release_server();
    let envs = [
        ("OZ_GITHUB_API_URL", srv.api_url.as_str()),
        ("GITHUB_TOKEN", "test-token"),
    ];

    // 1. Draft release edit: `GET /releases/tags/v1.4.6-draft` 404s -> falls back to `GET /releases` -> `PATCH /releases/1003`
    let edited_draft = run_oz_env(
        &repo,
        &[
            "release",
            "edit",
            "v1.4.6-draft",
            "--tag",
            "v1.4.6",
            "--title",
            "Ozone v1.4.6",
            "--notes",
            "Final release notes",
            "--publish",
            "--latest",
        ],
        &envs,
    );
    assert!(
        edited_draft.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&edited_draft.stderr)
    );
    let human = String::from_utf8_lossy(&edited_draft.stdout);
    assert!(
        human.contains("Updated release v1.4.6 [PUBLISHED]"),
        "{human}"
    );
    assert!(
        human.contains("https://github.com/acme/demo/releases/tag/v1.4.6"),
        "{human}"
    );

    // Verify 404 -> list fallback -> PATCH sequence
    {
        let reqs = srv.requests.lock().unwrap();
        assert!(
            reqs.iter()
                .any(|r| r.method == "GET"
                    && r.path == "/repos/acme/demo/releases/tags/v1.4.6-draft")
        );
        assert!(
            reqs.iter()
                .any(|r| r.method == "GET" && r.path == "/repos/acme/demo/releases")
        );
        let patch = reqs
            .iter()
            .find(|r| r.method == "PATCH" && r.path == "/repos/acme/demo/releases/1003")
            .expect("PATCH /releases/1003");
        let payload: Value = serde_json::from_str(&patch.body).unwrap();
        assert_eq!(payload["tag_name"], "v1.4.6");
        assert_eq!(payload["name"], "Ozone v1.4.6");
        assert_eq!(payload["body"], "Final release notes");
        assert_eq!(payload["draft"], false);
        assert_eq!(payload["make_latest"], "true");
    }

    // 2. Published release edit in JSON mode (with `--notes-file -` via stdin and `--prerelease`)
    let edited_pub = run_oz_stdin(
        &repo,
        &[
            "release",
            "edit",
            "v1.4.5",
            "--body-file",
            "-",
            "--prerelease",
            "--target",
            "release-1.4",
            "--json",
        ],
        &envs,
        "Updated body from stdin",
    );
    assert!(
        edited_pub.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&edited_pub.stderr)
    );
    let env = parse_json_stdout(&edited_pub);
    assert_eq!(env["schema"], "oz.v1");
    assert_eq!(env["ok"], true);
    assert_eq!(env["data"]["id"], 1001);
    assert_eq!(env["data"]["body"], "Updated body from stdin");
    assert_eq!(env["data"]["prerelease"], true);
    assert_eq!(env["data"]["target_commitish"], "release-1.4");
    assert_eq!(
        env["data"]["updated_fields"],
        serde_json::json!(["notes", "prerelease", "target"])
    );

    // 3. Edit with no fields fails with exit code 2
    let no_fields = run_oz_env(&repo, &["release", "edit", "v1.4.5", "--json"], &envs);
    assert_eq!(no_fields.status.code(), Some(2));
    let nf_env = parse_json_stdout(&no_fields);
    assert_eq!(nf_env["ok"], false);
}

#[test]
fn help_lists_release_under_remotes_and_github_group() {
    let out = Command::new(oz_bin())
        .arg("--help")
        .output()
        .expect("run oz --help");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("Remotes & GitHub (PR / Issue / CI / Release):"),
        "{text}"
    );
    assert!(!text.contains("Other Commands:"), "{text}");
    let rel_line = text
        .lines()
        .find(|l| l.trim_start().starts_with("release "))
        .expect("release line in oz --help");
    for sub in ["list", "view", "create", "edit"] {
        assert!(rel_line.contains(sub), "missing '{sub}' in: {rel_line}");
    }
}
