// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use oz_core::{
    CommitOptions, OzError, OzoneRepo, SwitchOptions, commit_changes, create_or_reset_local_branch,
    switch_branch,
};
use oz_github::{
    CloneOptions, FetchOptions, PushOptions, ResolvedSshHost, SshConfigSearchContext, clone_repo,
    fetch_remote, format_openssh_command, push_refs, push_refs_with_options,
    resolve_ssh_host_with_context,
};
use oz_oplog::{ActorInfo, ActorKind};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn test_actor() -> ActorInfo {
    ActorInfo {
        kind: ActorKind::Human,
        client: Some("enterprise-test".to_string()),
        workspace: None,
    }
}

fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Enterprise Tester")
        .env("GIT_AUTHOR_EMAIL", "ent@example.com")
        .env("GIT_COMMITTER_NAME", "Enterprise Tester")
        .env("GIT_COMMITTER_EMAIL", "ent@example.com")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed:\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write_exec(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

#[test]
fn include_if_and_instead_of_push_instead_of_control_fetch_and_push_targets() {
    let tmp = TempDir::new().unwrap();
    let bare_fetch = tmp.path().join("mirror_fetch/app.git");
    let bare_push = tmp.path().join("mirror_push/app.git");
    fs::create_dir_all(&bare_fetch).unwrap();
    fs::create_dir_all(&bare_push).unwrap();
    run_git(&bare_fetch, &["init", "--bare", "-b", "main"]);
    run_git(&bare_push, &["init", "--bare", "-b", "main"]);

    // Seed bare_fetch with initial commit & replicate into bare_push
    let seed = tmp.path().join("seed");
    fs::create_dir_all(&seed).unwrap();
    run_git(&seed, &["init", "-b", "main"]);
    fs::write(seed.join("README.md"), "enterprise\n").unwrap();
    run_git(&seed, &["add", "README.md"]);
    run_git(&seed, &["commit", "-m", "init"]);
    run_git(&seed, &["push", bare_fetch.to_str().unwrap(), "main"]);
    run_git(&seed, &["push", bare_push.to_str().unwrap(), "main"]);

    let work = tmp.path().join("work_repo");
    clone_repo(
        &CloneOptions::new(bare_fetch.to_string_lossy().into_owned(), work.clone()),
        test_actor(),
    )
    .expect("clone initial repo");

    // Configure includeIf (gitdir/i + onbranch) and url insteadOf / pushInsteadOf in local repo config
    let corp_inc = tmp.path().join("corp.gitconfig");
    fs::write(
        &corp_inc,
        format!(
            "[url \"{}/\"]\n\tinsteadOf = \"corp:\"\n",
            tmp.path().join("mirror_fetch").display()
        ),
    )
    .unwrap();
    let release_inc = tmp.path().join("release.gitconfig");
    fs::write(
        &release_inc,
        format!(
            "[url \"{}/\"]\n\tpushInsteadOf = \"corp:\"\n",
            tmp.path().join("mirror_push").display()
        ),
    )
    .unwrap();

    let cfg_path = work.join(".git/config");
    let mut cfg_txt = fs::read_to_string(&cfg_path).unwrap();
    cfg_txt.push_str(&format!(
        "\n[remote \"corp-origin\"]\n\turl = \"corp:app.git\"\n\tfetch = +refs/heads/*:refs/remotes/corp-origin/*\n\
         [includeIf \"gitdir/i:{}/\"]\n\tpath = {}\n\
         [includeIf \"onbranch:release/\"]\n\tpath = {}\n",
        work.display().to_string().to_ascii_uppercase(),
        corp_inc.display(),
        release_inc.display()
    ));
    fs::write(&cfg_path, cfg_txt).unwrap();

    // Create a new commit in bare_fetch to verify `insteadOf` fetch on `corp-origin`
    fs::write(seed.join("v2.txt"), "from mirror_fetch\n").unwrap();
    run_git(&seed, &["add", "v2.txt"]);
    run_git(&seed, &["commit", "-m", "v2 on fetch remote"]);
    run_git(&seed, &["push", bare_fetch.to_str().unwrap(), "main"]);

    let oz_work = OzoneRepo::discover(Some(&work)).unwrap();
    let fetch_out = fetch_remote(
        &oz_work,
        &FetchOptions {
            remote: Some("corp-origin".to_string()),
            ..Default::default()
        },
        test_actor(),
    )
    .expect("fetch via insteadOf");
    assert!(
        fetch_out
            .remote_url
            .contains(&bare_fetch.to_string_lossy().into_owned()),
        "got {}",
        fetch_out.remote_url
    );

    // Switch to `release/v1` -> activates `[includeIf "onbranch:release/"]` (`pushInsteadOf`),
    // routing `push_refs` to `bare_push` while keeping `bare_fetch` untouched for `release/v1`!
    switch_branch(
        &oz_work,
        &SwitchOptions {
            create_branch: Some("release/v1".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    let oz_release = OzoneRepo::discover(Some(&work)).unwrap();
    fs::write(work.join("release.txt"), "rel1\n").unwrap();
    let commit = commit_changes(&oz_release, &CommitOptions::new("feat: release v1")).unwrap();

    let push_out = push_refs(
        &oz_release,
        &PushOptions {
            remote: Some("corp-origin".to_string()),
            refspecs: vec!["release/v1".to_string()],
            ..Default::default()
        },
        test_actor(),
    )
    .expect("push via onbranch pushInsteadOf");
    assert!(
        push_out
            .remote_url
            .contains(&bare_push.to_string_lossy().into_owned()),
        "got {}",
        push_out.remote_url
    );

    let pushed_tip = run_git(&bare_push, &["rev-parse", "refs/heads/release/v1"]);
    assert_eq!(pushed_tip, commit.commit_oid);
    let absent_on_fetch = Command::new("git")
        .args(["rev-parse", "--verify", "refs/heads/release/v1"])
        .current_dir(&bare_fetch)
        .output()
        .unwrap();
    assert!(!absent_on_fetch.status.success());
}

#[test]
fn pre_push_hook_receives_remote_args_and_stdin_and_respects_no_verify() {
    let tmp = TempDir::new().unwrap();
    let bare = tmp.path().join("remote.git");
    fs::create_dir_all(&bare).unwrap();
    run_git(&bare, &["init", "--bare", "-b", "main"]);

    let local = tmp.path().join("local");
    fs::create_dir_all(&local).unwrap();
    run_git(&local, &["init", "-b", "main"]);
    fs::write(local.join("a.txt"), "one\n").unwrap();
    run_git(&local, &["add", "a.txt"]);
    run_git(&local, &["commit", "-m", "v1"]);
    run_git(&local, &["remote", "add", "origin", bare.to_str().unwrap()]);
    run_git(&local, &["push", "-u", "origin", "main"]);

    // Make commit v2
    fs::write(local.join("a.txt"), "two\n").unwrap();
    let oz = OzoneRepo::discover(Some(&local)).unwrap();
    let c2 = commit_changes(&oz, &CommitOptions::new("feat: v2")).unwrap();

    let hook_log = tmp.path().join("pre_push_capture.log");
    write_exec(
        &local.join(".git/hooks/pre-push"),
        &format!(
            "#!/bin/sh\n\
             stdin_line=$(cat)\n\
             printf 'remote=%s url=%s stdin=[%s]\\n' \"$1\" \"$2\" \"$stdin_line\" > '{}'\n\
             echo 'blocked by policy' >&2\n\
             exit 1\n",
            hook_log.display()
        ),
    );

    // 1. Failing pre-push aborts with HookFailed (exit 1) BEFORE updating remote
    let err = push_refs(
        &oz,
        &PushOptions {
            remote: Some("origin".to_string()),
            refspecs: vec!["main".to_string()],
            ..Default::default()
        },
        test_actor(),
    )
    .unwrap_err();
    assert!(matches!(err, OzError::HookFailed { .. }), "got {err:?}");
    assert_eq!(err.error_code(), "check_failed");
    assert_eq!(err.exit_code().as_i32(), 1);
    assert!(err.to_string().contains("blocked by policy"));

    let logged = fs::read_to_string(&hook_log).unwrap();
    assert!(logged.contains("remote=origin "), "{logged}");
    assert!(logged.contains(&c2.commit_oid), "{logged}");
    assert!(logged.contains("refs/heads/main"), "{logged}");
    // Remote tip must still be at v1, not v2!
    let remote_tip = run_git(&bare, &["rev-parse", "refs/heads/main"]);
    assert_ne!(remote_tip, c2.commit_oid);

    // 2. `--no-verify` bypasses pre-push cleanly
    let ok = push_refs_with_options(
        &oz,
        &PushOptions {
            remote: Some("origin".to_string()),
            refspecs: vec!["main".to_string()],
            ..Default::default()
        },
        true,
        &[],
        test_actor(),
    )
    .expect("push --no-verify");
    assert_eq!(ok.pushed_refs.len(), 1);
    assert_eq!(run_git(&bare, &["rev-parse", "refs/heads/main"]), c2.commit_oid);
    let _ = (create_or_reset_local_branch, ResolvedSshHost::default(), SshConfigSearchContext::default(), format_openssh_command, resolve_ssh_host_with_context);
}
