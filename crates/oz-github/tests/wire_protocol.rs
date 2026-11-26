// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for pure-Rust Wire Protocol v2 `clone_repo` (including `--filter=blob:none`
//! and `--branch`), `fetch_remote` (including `--prune` and `OpLog` `ref_deltas`), and `push_refs`
//! (`--atomic`, `--force-with-lease` success + stale-lease rejection, and `--set-upstream`)
//! against both local bare Git repositories and a real `tokio` smart-HTTP Git endpoint.

#![allow(clippy::too_many_lines)]

use base64::Engine;
use gix::prelude::Find;
use oz_core::{
    CommitOptions, OzError, OzoneRepo, SwitchOptions, commit_changes, create_or_reset_local_branch,
    switch_branch,
};
use oz_github::{
    CloneOptions, FetchOptions, ForceLeaseSpec, HttpAuthScheme, PushOptions, clone_repo,
    fetch_remote, push_refs,
};
use oz_oplog::{ActorInfo, ActorKind, load_entry, read_head_op_id};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

fn test_actor() -> ActorInfo {
    ActorInfo {
        kind: ActorKind::Agent,
        client: Some("oz-github-wire-test".to_string()),
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

/// Creates a bare remote repository seeded with:
/// - `c1` adding `legacy.bin` (deleted in `c2`, so a `blob:none` clone at `c2` never downloads it!)
/// - `c2` removing `legacy.bin` and adding `README.md` + `src/lib.rs` on `main`
/// - `feat/alpha` branch with `alpha.txt`
///
/// Returns `(bare_repo_path, legacy_blob_oid_hex, main_tip_hex, alpha_tip_hex)`.
fn seed_bare_remote(root: &Path) -> (PathBuf, String, String, String) {
    let bare_dir = root.join("origin.git");
    std::fs::create_dir_all(&bare_dir).expect("mkdir bare");
    run_git(&bare_dir, &["init", "--bare", "-b", "main"]);
    run_git(&bare_dir, &["config", "uploadpack.allowfilter", "true"]);
    run_git(
        &bare_dir,
        &["config", "uploadpack.allowanysha1inwant", "true"],
    );
    run_git(&bare_dir, &["config", "http.receivepack", "true"]);
    run_git(&bare_dir, &["config", "receive.advertiseAtomic", "true"]);

    let seed_work = root.join("seed-work");
    std::fs::create_dir_all(&seed_work).expect("mkdir seed-work");
    run_git(&seed_work, &["init", "-b", "main"]);
    run_git(&seed_work, &["config", "user.name", "Seed Author"]);
    run_git(&seed_work, &["config", "user.email", "seed@ozone.dev"]);

    let legacy_payload = "HISTORICAL_LARGE_BLOB_PAYLOAD_".repeat(256);
    std::fs::write(seed_work.join("legacy.bin"), &legacy_payload).expect("write legacy.bin");
    run_git(&seed_work, &["add", "legacy.bin"]);
    let legacy_blob_oid = run_git(&seed_work, &["rev-parse", ":legacy.bin"]);
    run_git(&seed_work, &["commit", "-m", "chore: add legacy blob"]);

    std::fs::remove_file(seed_work.join("legacy.bin")).expect("rm legacy.bin");
    std::fs::create_dir_all(seed_work.join("src")).expect("mkdir src");
    std::fs::write(seed_work.join("README.md"), "# Ozone Wire v2\n").expect("write readme");
    std::fs::write(
        seed_work.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .expect("write lib.rs");
    run_git(&seed_work, &["add", "-A"]);
    run_git(&seed_work, &["commit", "-m", "feat: initial active tree"]);
    let main_tip = run_git(&seed_work, &["rev-parse", "HEAD"]);

    run_git(&seed_work, &["checkout", "-b", "feat/alpha"]);
    std::fs::write(seed_work.join("alpha.txt"), "alpha feature\n").expect("write alpha");
    run_git(&seed_work, &["add", "alpha.txt"]);
    run_git(&seed_work, &["commit", "-m", "feat: add alpha"]);
    let alpha_tip = run_git(&seed_work, &["rev-parse", "HEAD"]);

    run_git(
        &seed_work,
        &[
            "push",
            bare_dir.to_str().expect("utf8 path"),
            "main:main",
            "feat/alpha:feat/alpha",
        ],
    );

    (bare_dir, legacy_blob_oid, main_tip, alpha_tip)
}

#[test]
fn local_bare_partial_clone_fetch_prune_and_push_atomic_with_lease() {
    let tmp = TempDir::new().expect("tempdir");
    let (bare_dir, legacy_blob_hex, main_tip, alpha_tip) = seed_bare_remote(tmp.path());

    // 1. Partial clone (`--filter=blob:none`) over Wire Protocol v2!
    let clone_dst = tmp.path().join("cloned-partial");
    let mut clone_opts = CloneOptions::new(format!("file://{}", bare_dir.display()), &clone_dst);
    clone_opts.filter = Some("blob:none".to_string());

    let clone_out = clone_repo(&clone_opts, test_actor()).expect("partial clone_repo");
    assert_eq!(clone_out.protocol_version, 2);
    assert!(clone_out.partial_clone);
    assert!(
        clone_out.filter_negotiated,
        "server must negotiate filter blob:none"
    );
    assert_eq!(clone_out.branch.as_deref(), Some("main"));
    assert_eq!(clone_out.head_oid.as_deref(), Some(main_tip.as_str()));
    assert!(clone_out.op_id.is_some());
    assert_eq!(
        std::fs::read_to_string(clone_dst.join("README.md")).expect("read README"),
        "# Ozone Wire v2\n"
    );
    assert_eq!(
        std::fs::read_to_string(clone_dst.join("src/lib.rs")).expect("read lib.rs"),
        "pub fn answer() -> u32 { 42 }\n"
    );

    // Verify promisor config & `.promisor` pack marker, and verify `legacy_blob_hex` from `c1`
    // was NEVER downloaded into local `.git/objects`!
    let git_cfg = std::fs::read_to_string(clone_dst.join(".git/config")).expect("read config");
    assert!(git_cfg.contains("promisor = true"));
    assert!(git_cfg.contains("partialclonefilter = blob:none"));

    let pack_entries: Vec<String> = std::fs::read_dir(clone_dst.join(".git/objects/pack"))
        .expect("read pack dir")
        .filter_map(|e| e.ok().and_then(|d| d.file_name().into_string().ok()))
        .collect();
    assert!(
        pack_entries.iter().any(|n| n.ends_with(".promisor")),
        "expected .promisor marker in {pack_entries:?}"
    );

    let gix_cloned = gix::open(&clone_dst).expect("open cloned gix");
    let legacy_oid = gix::ObjectId::from_hex(legacy_blob_hex.as_bytes()).expect("legacy oid");
    let mut buf = Vec::new();
    assert!(
        gix_cloned
            .objects
            .try_find(&legacy_oid, &mut buf)
            .expect("odb lookup")
            .is_none(),
        "historical deleted blob {legacy_blob_hex} must NOT be downloaded in blob:none partial clone"
    );

    // Verify colocated `.git/ozone/` + initial OpLog entry.
    let oz_repo = OzoneRepo::discover(Some(&clone_dst)).expect("discover cloned OzoneRepo");
    assert!(oz_repo.layout.is_initialized());
    let head_op = read_head_op_id(&oz_repo.layout)
        .expect("read head op")
        .expect("op present");
    let op_entry = load_entry(&oz_repo.layout, &head_op).expect("load op entry");
    assert!(op_entry.ref_deltas.contains_key("refs/heads/main"));
    assert!(
        op_entry
            .ref_deltas
            .contains_key("refs/remotes/origin/feat/alpha")
    );

    // 2. Clone with explicit `--branch feat/alpha`.
    let branch_clone_dst = tmp.path().join("cloned-branch");
    let mut br_opts = CloneOptions::new(bare_dir.to_string_lossy().into_owned(), &branch_clone_dst);
    br_opts.branch = Some("feat/alpha".to_string());
    let br_out = clone_repo(&br_opts, test_actor()).expect("branch clone");
    assert_eq!(br_out.branch.as_deref(), Some("feat/alpha"));
    assert_eq!(br_out.head_oid.as_deref(), Some(alpha_tip.as_str()));
    assert!(branch_clone_dst.join("alpha.txt").is_file());

    // 3. Push a new commit from the `blob:none` partial clone with `--set-upstream`, then advance
    // remote from `branch_clone_dst` to test `fetch_remote --prune` and `--force-with-lease`!
    switch_branch(
        &oz_repo,
        &SwitchOptions {
            create_branch: Some("feat/work".to_string()),
            ..Default::default()
        },
    )
    .expect("switch -c feat/work");
    let oz_repo = OzoneRepo::discover(Some(&clone_dst)).expect("re-discover");
    std::fs::write(clone_dst.join("work.txt"), "v1 from partial clone\n").expect("write work.txt");
    let c_v1 = commit_changes(
        &oz_repo,
        &CommitOptions {
            message: "feat: add work v1".to_string(),
            ..Default::default()
        },
    )
    .expect("commit v1");

    let oz_repo = OzoneRepo::discover(Some(&clone_dst)).expect("re-discover after commit");
    let push_v1 = push_refs(
        &oz_repo,
        &PushOptions {
            remote: Some("origin".to_string()),
            refspecs: vec!["feat/work".to_string()],
            set_upstream: true,
            ..Default::default()
        },
        test_actor(),
    )
    .expect("push v1");
    assert_eq!(push_v1.pushed_refs.len(), 1);
    assert_eq!(
        push_v1.pushed_refs[0].new_oid.as_deref(),
        Some(c_v1.commit_oid.as_str())
    );
    assert_eq!(push_v1.upstream_configured, vec!["feat/work".to_string()]);
    assert_eq!(
        run_git(&bare_dir, &["rev-parse", "refs/heads/feat/work"]),
        c_v1.commit_oid
    );

    // Advance `feat/work` on remote behind `clone_dst`'s back, and delete `feat/alpha` on remote!
    let side_work = tmp.path().join("side-pusher");
    run_git(
        tmp.path(),
        &[
            "clone",
            bare_dir.to_str().expect("utf8"),
            side_work.to_str().expect("utf8"),
        ],
    );
    run_git(&side_work, &["config", "user.name", "Concurrent Agent"]);
    run_git(
        &side_work,
        &["config", "user.email", "concurrent@ozone.dev"],
    );
    run_git(&side_work, &["checkout", "feat/work"]);
    std::fs::write(side_work.join("work.txt"), "concurrent remote update\n").expect("write side");
    run_git(&side_work, &["add", "work.txt"]);
    run_git(&side_work, &["commit", "-m", "feat: concurrent update"]);
    let remote_concurrent_oid = run_git(&side_work, &["rev-parse", "HEAD"]);
    run_git(&side_work, &["push", "origin", "feat/work"]);
    run_git(&side_work, &["push", "origin", ":feat/alpha"]);

    // Rewrite local `feat/work` in `clone_dst` to diverge from `c_v1`, and test `--force-with-lease`
    // STALE LEASE REJECTION!
    create_or_reset_local_branch(&oz_repo, "local-diverged", Some("main"))
        .expect("create local-diverged");
    switch_branch(
        &oz_repo,
        &SwitchOptions {
            target: Some("local-diverged".to_string()),
            ..Default::default()
        },
    )
    .expect("switch local-diverged");
    let oz_repo = OzoneRepo::discover(Some(&clone_dst)).expect("discover");
    std::fs::write(clone_dst.join("work.txt"), "local rewritten v2\n").expect("write v2");
    let c_rewritten = commit_changes(
        &oz_repo,
        &CommitOptions {
            message: "feat: rewritten v2".to_string(),
            ..Default::default()
        },
    )
    .expect("commit rewritten");
    create_or_reset_local_branch(&oz_repo, "feat/work", Some(c_rewritten.commit_oid.as_str()))
        .expect("reset feat/work to rewritten commit");

    let oz_repo = OzoneRepo::discover(Some(&clone_dst)).expect("discover");
    let stale_err = push_refs(
        &oz_repo,
        &PushOptions {
            remote: Some("origin".to_string()),
            refspecs: vec!["feat/work".to_string()],
            force_with_lease: true,
            ..Default::default()
        },
        test_actor(),
    )
    .expect_err("stale --force-with-lease must be rejected");
    assert!(
        matches!(stale_err, OzError::Conflict { .. }),
        "expected Conflict error on stale lease, got: {stale_err:?}"
    );
    assert!(stale_err.to_string().contains("stale info"));
    assert_eq!(
        run_git(&bare_dir, &["rev-parse", "refs/heads/feat/work"]),
        remote_concurrent_oid,
        "remote ref must remain untouched after stale-lease rejection"
    );

    // 4. Test `--atomic` all-or-nothing guarantee: pushing `[local-diverged, feat/work]` with
    // `atomic: true` where `feat/work` has a stale lease must NOT create `local-diverged` on remote!
    let atomic_err = push_refs(
        &oz_repo,
        &PushOptions {
            remote: Some("origin".to_string()),
            refspecs: vec!["local-diverged".to_string(), "feat/work".to_string()],
            atomic: true,
            lease_specs: vec![ForceLeaseSpec::parse(&format!(
                "feat/work:{}",
                c_v1.commit_oid
            ))],
            ..Default::default()
        },
        test_actor(),
    )
    .expect_err("atomic push with one stale lease must abort all refs");
    assert!(matches!(atomic_err, OzError::Conflict { .. }));
    let remote_refs_after_abort = run_git(&bare_dir, &["for-each-ref", "--format=%(refname)"]);
    assert!(
        !remote_refs_after_abort.contains("refs/heads/local-diverged"),
        "--atomic push must not create sibling ref when another ref fails precondition"
    );

    // 5. Run `fetch_remote` with `prune: true` -> updates `refs/remotes/origin/feat/work` to
    // `remote_concurrent_oid` AND prunes `refs/remotes/origin/feat/alpha`, recording OpLog `ref_deltas`!
    let fetch_out = fetch_remote(
        &oz_repo,
        &FetchOptions {
            remote: Some("origin".to_string()),
            prune: true,
            ..Default::default()
        },
        test_actor(),
    )
    .expect("fetch_remote --prune");
    assert_eq!(fetch_out.protocol_version, 2);
    assert!(fetch_out.pack_received);
    assert!(
        fetch_out
            .pruned_refs
            .contains(&"refs/remotes/origin/feat/alpha".to_string())
    );
    assert!(
        fetch_out
            .updated_refs
            .iter()
            .any(|u| u.local_ref == "refs/remotes/origin/feat/work"
                && u.new_oid == remote_concurrent_oid)
    );
    let fetch_op_id = fetch_out.op_id.expect("fetch op_id");
    let fetch_entry = load_entry(&oz_repo.layout, &fetch_op_id).expect("load fetch op");
    assert_eq!(
        fetch_entry.ref_deltas["refs/remotes/origin/feat/alpha"].new,
        None
    );

    // 6. Now that `refs/remotes/origin/feat/work` matches remote's tip, `--atomic` + `--force-with-lease`
    // succeeds cleanly for BOTH `local-diverged` and `feat/work`!
    let oz_repo = OzoneRepo::discover(Some(&clone_dst)).expect("discover");
    let atomic_ok = push_refs(
        &oz_repo,
        &PushOptions {
            remote: Some("origin".to_string()),
            refspecs: vec!["local-diverged".to_string(), "feat/work".to_string()],
            atomic: true,
            force_with_lease: true,
            ..Default::default()
        },
        test_actor(),
    )
    .expect("atomic force-with-lease push after fetch");
    assert!(atomic_ok.atomic);
    assert_eq!(atomic_ok.pushed_refs.len(), 2);
    assert_eq!(
        run_git(&bare_dir, &["rev-parse", "refs/heads/feat/work"]),
        c_rewritten.commit_oid
    );
    assert_eq!(
        run_git(&bare_dir, &["rev-parse", "refs/heads/local-diverged"]),
        c_rewritten.commit_oid
    );
}

/// Minimal `tokio` Smart-HTTP Git server speaking Protocol v2 (`git-upload-pack --stateless-rpc`)
/// and Smart Push (`git-receive-pack --stateless-rpc`) with HTTP token authentication.
async fn spawn_smart_http_git_server(bare_repo: PathBuf, required_token: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let bare_arc = Arc::new(bare_repo);
    let tok_arc = Arc::new(required_token);

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let repo_path = Arc::clone(&bare_arc);
            let expected_tok = Arc::clone(&tok_arc);
            tokio::spawn(async move {
                let _ = handle_smart_http_conn(stream, &repo_path, &expected_tok).await;
            });
        }
    });

    format!("http://{addr}/acme/demo.git")
}

async fn handle_smart_http_conn(
    stream: TcpStream,
    bare_repo: &Path,
    expected_token: &str,
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

    let body = read_http_body(&mut reader, &headers).await?;

    // Check `Authorization` header (`Basic base64("x-access-token:<tok>")` or `Bearer <tok>`).
    let expected_basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD
            .encode(format!("x-access-token:{expected_token}"))
    );
    let expected_bearer = format!("Bearer {expected_token}");
    let auth_ok = headers
        .get("authorization")
        .is_some_and(|v| v == &expected_basic || v == &expected_bearer);

    if !auth_ok {
        let resp = b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"Git\"\r\nContent-Length: 12\r\nConnection: close\r\n\r\nUnauthorized";
        write_half.write_all(resp).await?;
        return Ok(());
    }

    let git_protocol = headers.get("git-protocol").cloned();
    let (path, query) = raw_target
        .split_once('?')
        .map_or((raw_target.as_str(), None), |(p, q)| (p, Some(q)));

    if method == "GET" && path.ends_with("/info/refs") {
        let service = query
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("service=")))
            .unwrap_or_default();
        let subcmd = match service {
            "git-upload-pack" => "upload-pack",
            "git-receive-pack" => "receive-pack",
            _ => {
                write_http_response(
                    &mut write_half,
                    "400 Bad Request",
                    "text/plain",
                    b"bad service",
                )
                .await?;
                return Ok(());
            }
        };

        let mut cmd = Command::new("git");
        cmd.args([subcmd, "--stateless-rpc", "--advertise-refs"])
            .arg(bare_repo);
        if let Some(ref proto) = git_protocol {
            cmd.env("GIT_PROTOCOL", proto);
        }
        let out = cmd.output()?;
        let is_v2 = service == "git-upload-pack"
            && git_protocol
                .as_deref()
                .is_some_and(|p| p.contains("version=2"));

        let mut payload = Vec::new();
        if !is_v2 {
            let announcement = format!("# service={service}\n");
            gix_packetline::blocking_io::encode::data_to_write(
                announcement.as_bytes(),
                &mut payload,
            )
            .expect("pktline");
            gix_packetline::blocking_io::encode::flush_to_write(&mut payload).expect("flush");
        }
        payload.extend_from_slice(&out.stdout);

        let content_type = format!("application/x-{service}-advertisement");
        write_http_response(&mut write_half, "200 OK", &content_type, &payload).await?;
        return Ok(());
    }

    if method == "POST"
        && (path.ends_with("/git-upload-pack") || path.ends_with("/git-receive-pack"))
    {
        let (service, subcmd) = if path.ends_with("/git-upload-pack") {
            ("git-upload-pack", "upload-pack")
        } else {
            ("git-receive-pack", "receive-pack")
        };

        let mut cmd = Command::new("git");
        cmd.args([subcmd, "--stateless-rpc"])
            .arg(bare_repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(ref proto) = git_protocol {
            cmd.env("GIT_PROTOCOL", proto);
        }

        let mut child = cmd.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&body)?;
            stdin.flush()?;
        }
        let out = child.wait_with_output()?;
        let content_type = format!("application/x-{service}-result");
        write_http_response(&mut write_half, "200 OK", &content_type, &out.stdout).await?;
        return Ok(());
    }

    write_http_response(&mut write_half, "404 Not Found", "text/plain", b"not found").await
}

async fn read_http_body<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    headers: &HashMap<String, String>,
) -> std::io::Result<Vec<u8>> {
    if headers
        .get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        let mut out = Vec::new();
        loop {
            let mut size_line = String::new();
            reader.read_line(&mut size_line).await?;
            let hex_part = size_line.trim().split(';').next().unwrap_or("0");
            let chunk_len = usize::from_str_radix(hex_part, 16).unwrap_or(0);
            if chunk_len == 0 {
                let mut crlf = String::new();
                let _ = reader.read_line(&mut crlf).await?;
                break;
            }
            let mut buf = vec![0u8; chunk_len];
            reader.read_exact(&mut buf).await?;
            out.extend_from_slice(&buf);
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf).await?;
        }
        return Ok(out);
    }

    if let Some(len) = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        && len > 0
    {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await?;
        return Ok(buf);
    }

    Ok(Vec::new())
}

async fn write_http_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smart_http_clone_partial_fetch_and_push_atomic_with_auth() {
    let tmp = TempDir::new().expect("tempdir");
    let (bare_dir, legacy_blob_hex, main_tip, _) = seed_bare_remote(tmp.path());
    let secret = "ghp_smart_http_secret_token_42";
    let remote_url = spawn_smart_http_git_server(bare_dir.clone(), secret.to_string()).await;

    // 1. Unauthenticated clone must fail cleanly with auth error!
    let unauth_dst = tmp.path().join("http-unauth");
    let mut bad_opts = CloneOptions::new(&remote_url, &unauth_dst);
    bad_opts.auth_token = Some("wrong_token".to_string());
    let bad_err = clone_repo(&bad_opts, test_actor()).expect_err("wrong token must fail");
    assert!(matches!(
        bad_err,
        OzError::CheckFailed { .. } | OzError::Git(_) | OzError::GitSource { .. }
    ));

    // 2. Authenticated partial clone (`--filter=blob:none`) over Smart HTTP v2!
    let http_dst = tmp.path().join("http-cloned");
    let mut http_clone_opts = CloneOptions::new(&remote_url, &http_dst);
    http_clone_opts.filter = Some("blob:none".to_string());
    http_clone_opts.auth_token = Some(secret.to_string());
    http_clone_opts.auth_scheme = HttpAuthScheme::Bearer;

    let out = clone_repo(&http_clone_opts, test_actor()).expect("smart HTTP partial clone");
    assert_eq!(out.protocol_version, 2);
    assert!(out.filter_negotiated);
    assert_eq!(out.head_oid.as_deref(), Some(main_tip.as_str()));
    assert!(http_dst.join("src/lib.rs").is_file());

    // Confirm historical blob from `c1` was NOT downloaded over HTTP either!
    let gix_http = gix::open(&http_dst).expect("open gix");
    let legacy_oid = gix::ObjectId::from_hex(legacy_blob_hex.as_bytes()).expect("oid");
    let mut buf = Vec::new();
    assert!(
        gix_http
            .objects
            .try_find(&legacy_oid, &mut buf)
            .expect("lookup")
            .is_none()
    );

    // 3. Create stacked branches locally and push them atomically over Smart HTTP (`git-receive-pack`)!
    let oz_repo = OzoneRepo::discover(Some(&http_dst)).expect("discover");
    switch_branch(
        &oz_repo,
        &SwitchOptions {
            create_branch: Some("http-stack-1".to_string()),
            ..Default::default()
        },
    )
    .expect("switch http-stack-1");
    let oz_repo = OzoneRepo::discover(Some(&http_dst)).expect("discover");
    std::fs::write(http_dst.join("s1.txt"), "http stack 1\n").expect("write s1");
    let s1 = commit_changes(
        &oz_repo,
        &CommitOptions {
            message: "feat: http stack 1".to_string(),
            ..Default::default()
        },
    )
    .expect("commit s1");

    switch_branch(
        &oz_repo,
        &SwitchOptions {
            create_branch: Some("http-stack-2".to_string()),
            ..Default::default()
        },
    )
    .expect("switch http-stack-2");
    let oz_repo = OzoneRepo::discover(Some(&http_dst)).expect("discover");
    std::fs::write(http_dst.join("s2.txt"), "http stack 2\n").expect("write s2");
    let s2 = commit_changes(
        &oz_repo,
        &CommitOptions {
            message: "feat: http stack 2".to_string(),
            ..Default::default()
        },
    )
    .expect("commit s2");

    let oz_repo = OzoneRepo::discover(Some(&http_dst)).expect("discover");
    let push_out = push_refs(
        &oz_repo,
        &PushOptions {
            remote: Some("origin".to_string()),
            refspecs: vec!["http-stack-1".to_string(), "http-stack-2".to_string()],
            atomic: true,
            set_upstream: true,
            auth_token: Some(secret.to_string()),
            auth_scheme: HttpAuthScheme::BasicToken,
            ..Default::default()
        },
        test_actor(),
    )
    .expect("smart HTTP atomic push_refs");

    assert!(push_out.atomic);
    assert_eq!(push_out.pushed_refs.len(), 2);
    assert_eq!(
        run_git(&bare_dir, &["rev-parse", "refs/heads/http-stack-1"]),
        s1.commit_oid
    );
    assert_eq!(
        run_git(&bare_dir, &["rev-parse", "refs/heads/http-stack-2"]),
        s2.commit_oid
    );

    // 4. Verify stale `--force-with-lease` rejection over Smart HTTP!
    let stale = push_refs(
        &oz_repo,
        &PushOptions {
            remote: Some("origin".to_string()),
            refspecs: vec!["http-stack-2".to_string()],
            lease_specs: vec![ForceLeaseSpec::parse(&format!("http-stack-2:{main_tip}"))],
            auth_token: Some(secret.to_string()),
            ..Default::default()
        },
        test_actor(),
    )
    .expect_err("stale lease over smart HTTP must fail");
    assert!(matches!(stale, OzError::Conflict { .. }));

    // 5. Push a remote commit to `main` and verify `fetch_remote` over Smart HTTP v2!
    let updater = tmp.path().join("http-updater");
    run_git(
        tmp.path(),
        &[
            "clone",
            bare_dir.to_str().expect("utf8"),
            updater.to_str().expect("utf8"),
        ],
    );
    run_git(&updater, &["config", "user.name", "Updater"]);
    run_git(&updater, &["config", "user.email", "up@ozone.dev"]);
    std::fs::write(updater.join("remote_new.txt"), "from remote\n").expect("write");
    run_git(&updater, &["add", "remote_new.txt"]);
    run_git(&updater, &["commit", "-m", "feat: remote main advance"]);
    let new_remote_main = run_git(&updater, &["rev-parse", "HEAD"]);
    run_git(&updater, &["push", "origin", "main"]);

    let fetch_http = fetch_remote(
        &oz_repo,
        &FetchOptions {
            remote: Some("origin".to_string()),
            auth_token: Some(secret.to_string()),
            auth_scheme: HttpAuthScheme::Bearer,
            ..Default::default()
        },
        test_actor(),
    )
    .expect("smart HTTP fetch_remote");
    assert_eq!(fetch_http.protocol_version, 2);
    assert!(fetch_http.pack_received);
    assert!(
        fetch_http
            .updated_refs
            .iter()
            .any(|u| u.local_ref == "refs/remotes/origin/main" && u.new_oid == new_remote_main)
    );
}

#[test]
fn clone_empty_unborn_remote_push_initial_commit_and_delete_remote_branch() {
    let tmp = TempDir::new().expect("tempdir");
    let empty_bare = tmp.path().join("empty.git");
    std::fs::create_dir_all(&empty_bare).expect("mkdir empty bare");
    run_git(&empty_bare, &["init", "--bare", "-b", "main"]);

    // 1. Clone completely empty unborn bare repository.
    let dst = tmp.path().join("cloned-empty");
    let clone_out = clone_repo(
        &CloneOptions::new(empty_bare.to_string_lossy().into_owned(), &dst),
        test_actor(),
    )
    .expect("clone empty repo");
    assert_eq!(clone_out.branch.as_deref(), Some("main"));
    assert_eq!(clone_out.head_oid, None);
    assert!(dst.join(".git/ozone").is_dir());

    // 2. Author initial commit via `oz-core` and push to empty bare remote via `push_refs`!
    let oz_repo = OzoneRepo::discover(Some(&dst)).expect("discover unborn clone");
    std::fs::write(dst.join("hello.txt"), "hello unborn world\n").expect("write hello.txt");
    let c1 = commit_changes(
        &oz_repo,
        &CommitOptions {
            message: "feat: initial root commit".to_string(),
            ..Default::default()
        },
    )
    .expect("commit root");

    let oz_repo = OzoneRepo::discover(Some(&dst)).expect("discover after root commit");
    let push_root = push_refs(
        &oz_repo,
        &PushOptions {
            remote: Some("origin".to_string()),
            set_upstream: true,
            ..Default::default()
        },
        test_actor(),
    )
    .expect("push initial commit to empty bare repo");
    assert_eq!(push_root.pushed_refs.len(), 1);
    assert_eq!(
        run_git(&empty_bare, &["rev-parse", "refs/heads/main"]),
        c1.commit_oid
    );

    // 3. Push a temporary branch and then delete it remotely with `delete: true`!
    create_or_reset_local_branch(&oz_repo, "temp-branch", Some("main"))
        .expect("create temp-branch");
    let _ = push_refs(
        &oz_repo,
        &PushOptions {
            remote: Some("origin".to_string()),
            refspecs: vec!["temp-branch".to_string()],
            ..Default::default()
        },
        test_actor(),
    )
    .expect("push temp-branch");
    assert_eq!(
        run_git(&empty_bare, &["rev-parse", "refs/heads/temp-branch"]),
        c1.commit_oid
    );

    let del_out = push_refs(
        &oz_repo,
        &PushOptions {
            remote: Some("origin".to_string()),
            refspecs: vec!["temp-branch".to_string()],
            delete: true,
            ..Default::default()
        },
        test_actor(),
    )
    .expect("delete remote temp-branch");
    assert!(del_out.pushed_refs[0].deleted);
    let refs_now = run_git(&empty_bare, &["for-each-ref", "--format=%(refname)"]);
    assert!(!refs_now.contains("refs/heads/temp-branch"));
}

#[test]
fn cleartext_non_loopback_http_with_credentials_fails_closed_with_guardrail() {
    let tmp = TempDir::new().expect("tempdir");

    let assert_guardrail = |err: OzError| match err {
        OzError::GuardrailViolation {
            kind,
            message,
            recovery,
        } => {
            assert_eq!(kind, "insecure_http_credentials");
            assert!(message.contains("unencrypted HTTP"), "got: {message}");
            assert_eq!(
                recovery,
                vec!["Switch the remote URL from http:// to https:// or ssh://.".to_string()]
            );
        }
        other => panic!("expected GuardrailViolation(insecure_http_credentials), got {other:?}"),
    };

    // 1. Explicit `auth_token` on non-loopback `http://` clone fails closed immediately.
    let mut clone_opts = CloneOptions::new(
        "http://github.com/acme/demo.git",
        tmp.path().join("blocked-clone"),
    );
    clone_opts.auth_token = Some("ghp_explicit_secret".to_string());
    assert_guardrail(clone_repo(&clone_opts, test_actor()).unwrap_err());

    // 2. Inline URL password (`http://user:pass@non-loopback/...`) also fails closed.
    let inline_opts = CloneOptions::new(
        "http://x-access-token:ghp_inline_secret@git.example.com/acme/demo.git",
        tmp.path().join("blocked-inline"),
    );
    assert_guardrail(clone_repo(&inline_opts, test_actor()).unwrap_err());

    // 3. `fetch_remote` and `push_refs` on a repo with a non-loopback `http://` remote and explicit token fail closed.
    let repo_dir = tmp.path().join("local-repo");
    std::fs::create_dir_all(&repo_dir).expect("mkdir local-repo");
    run_git(&repo_dir, &["init", "-b", "main"]);
    run_git(&repo_dir, &["config", "user.name", "Test"]);
    run_git(&repo_dir, &["config", "user.email", "test@ozone.dev"]);
    run_git(
        &repo_dir,
        &[
            "remote",
            "add",
            "origin",
            "http://git.example.com/acme/demo.git",
        ],
    );
    std::fs::write(repo_dir.join("README.md"), "hi\n").expect("write");
    let oz_initial = OzoneRepo::discover(Some(&repo_dir)).expect("discover");
    let _ = commit_changes(&oz_initial, &CommitOptions::new("chore: init")).expect("commit");
    let oz = OzoneRepo::discover(Some(&repo_dir)).expect("rediscover");

    let fetch_err = fetch_remote(
        &oz,
        &FetchOptions {
            remote: Some("origin".to_string()),
            auth_token: Some("ghp_fetch_secret".to_string()),
            auth_scheme: HttpAuthScheme::Bearer,
            ..Default::default()
        },
        test_actor(),
    )
    .unwrap_err();
    assert_guardrail(fetch_err);

    let push_err = push_refs(
        &oz,
        &PushOptions {
            remote: Some("origin".to_string()),
            refspecs: vec!["main".to_string()],
            auth_token: Some("ghp_push_secret".to_string()),
            auth_scheme: HttpAuthScheme::BasicToken,
            ..Default::default()
        },
        test_actor(),
    )
    .unwrap_err();
    assert_guardrail(push_err);
}
