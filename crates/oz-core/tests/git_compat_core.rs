// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use oz_core::{
    CommitOptions, OzError, OzoneRepo, commit_changes, compute_diff, compute_status,
    render_unified_diff,
};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed in {}:\nstdout: {}\nstderr: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[cfg(unix)]
fn write_exec(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

#[cfg(unix)]
#[test]
fn hooks_husky_lefthook_pre_commit_style_and_no_verify() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path();
    run_git(repo_dir, &["init", "-b", "main"]);
    run_git(repo_dir, &["config", "user.name", "Alice Hook"]);
    run_git(repo_dir, &["config", "user.email", "alice@example.com"]);
    fs::write(repo_dir.join("README.md"), "hello\n").unwrap();
    run_git(repo_dir, &["add", "README.md"]);
    run_git(repo_dir, &["commit", "-m", "init"]);

    // Configure husky/lefthook-style `core.hooksPath = .husky/_`
    run_git(repo_dir, &["config", "core.hooksPath", ".husky/_"]);
    let log_file = repo_dir.join(".hook_events.log");

    write_exec(
        &repo_dir.join(".husky/_/pre-commit"),
        &format!(
            "#!/bin/sh\n\
             echo \"pre-commit cwd=$(pwd) prefix=\\\"$GIT_PREFIX\\\" idx=$GIT_INDEX_FILE author=$GIT_AUTHOR_NAME\" >> '{}'\n\
             if grep -q BLOCK_PRE_COMMIT blocked.txt 2>/dev/null; then\n\
               echo 'pre-commit rejected blocked.txt' >&2\n\
               exit 1\n\
             fi\n",
            log_file.display()
        ),
    );
    write_exec(
        &repo_dir.join(".husky/_/prepare-commit-msg"),
        &format!(
            "#!/bin/sh\n\
             echo \"prepare-commit-msg $2\" >> '{}'\n\
             printf '\\nHook-Trailer: prepared\\n' >> \"$1\"\n",
            log_file.display()
        ),
    );
    write_exec(
        &repo_dir.join(".husky/_/commit-msg"),
        &format!(
            "#!/bin/sh\n\
             echo \"commit-msg\" >> '{}'\n\
             if grep -q BAD_MSG \"$1\"; then\n\
               echo 'commit-msg rejected BAD_MSG' >&2\n\
               exit 2\n\
             fi\n",
            log_file.display()
        ),
    );
    write_exec(
        &repo_dir.join(".husky/_/post-commit"),
        &format!(
            "#!/bin/sh\n\
             head_sha=$(git rev-parse HEAD)\n\
             echo \"post-commit $head_sha\" >> '{}'\n",
            log_file.display()
        ),
    );

    fs::write(repo_dir.join(".gitignore"), ".hook_events.log\n").unwrap();
    fs::write(repo_dir.join("blocked.txt"), "BLOCK_PRE_COMMIT\n").unwrap();

    let repo = OzoneRepo::discover(Some(repo_dir)).unwrap();
    // 1. Failing pre-commit aborts with OzError::HookFailed (exit code 1)
    let err = commit_changes(&repo, &CommitOptions::new("feat: attempt")).unwrap_err();
    assert!(matches!(err, OzError::HookFailed { .. }), "got {err:?}");
    assert_eq!(err.error_code(), "check_failed");
    assert_eq!(err.exit_code().as_i32(), 1);
    assert!(err.to_string().contains("pre-commit rejected blocked.txt"));

    // 2. Failing commit-msg aborts even if pre-commit passes
    fs::write(repo_dir.join("blocked.txt"), "ok\n").unwrap();
    let err_msg = commit_changes(&repo, &CommitOptions::new("BAD_MSG here")).unwrap_err();
    assert!(matches!(err_msg, OzError::HookFailed { .. }), "got {err_msg:?}");
    assert_eq!(err_msg.exit_code().as_i32(), 1);
    assert!(err_msg.to_string().contains("commit-msg rejected BAD_MSG"));

    // 3. Passing hooks run all 4 phases and propagate env/prepare/post-commit
    let _ = fs::remove_file(&log_file);
    let outcome = commit_changes(&repo, &CommitOptions::new("feat: pass hooks")).unwrap();
    assert!(outcome.message.contains("Hook-Trailer: prepared"));
    assert!(outcome.message.contains("Change-Id: I"));
    let events = fs::read_to_string(&log_file).unwrap();
    assert!(events.contains("pre-commit cwd="), "{events}");
    assert!(events.contains("prefix=\"\""), "{events}");
    assert!(events.contains("author=Alice Hook"), "{events}");
    assert!(events.contains("prepare-commit-msg message"), "{events}");
    assert!(events.contains("commit-msg"), "{events}");
    assert!(
        events.contains(&format!("post-commit {}", outcome.commit_oid)),
        "{events}"
    );

    // 4. `--no-verify` bypasses pre-commit & commit-msg, while still running prepare/post-commit
    let _ = fs::remove_file(&log_file);
    fs::write(repo_dir.join("blocked.txt"), "BLOCK_PRE_COMMIT\n").unwrap();
    let bypassed = commit_changes(
        &repo,
        &CommitOptions::new("BAD_MSG bypassed").with_no_verify(true),
    )
    .unwrap();
    let bypassed_events = fs::read_to_string(&log_file).unwrap();
    let lines: Vec<&str> = bypassed_events.lines().collect();
    assert!(!lines.iter().any(|l| l.starts_with("pre-commit ")), "{bypassed_events}");
    assert!(!lines.contains(&"commit-msg"), "{bypassed_events}");
    assert!(
        lines.contains(&"prepare-commit-msg message"),
        "{bypassed_events}"
    );
    assert!(
        lines
            .iter()
            .any(|l| *l == format!("post-commit {}", bypassed.commit_oid)),
        "{bypassed_events}"
    );
}

#[test]
fn gitattributes_mixed_eol_nested_info_and_binary_prevent_phantom_diffs() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path();
    run_git(repo_dir, &["init", "-b", "main"]);
    run_git(repo_dir, &["config", "user.name", "Ada"]);
    run_git(repo_dir, &["config", "user.email", "ada@example.com"]);

    fs::create_dir_all(repo_dir.join("docs/raw")).unwrap();
    fs::write(
        repo_dir.join(".gitattributes"),
        "* text=auto\n*.sh eol=lf\n*.bin binary\n",
    )
    .unwrap();
    fs::write(repo_dir.join("docs/.gitattributes"), "*.md text\n").unwrap();
    fs::write(
        repo_dir.join(".git/info/attributes"),
        "docs/raw/*.md -text -diff\n",
    )
    .unwrap();

    fs::write(repo_dir.join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
    fs::write(repo_dir.join("docs/guide.md"), b"# Title\nbody\n").unwrap();
    fs::write(repo_dir.join("docs/raw/spec.md"), b"raw\nlines\n").unwrap();
    fs::write(repo_dir.join("data.bin"), b"HDR\r\nPAYLOAD\r\n").unwrap();

    run_git(repo_dir, &["add", "."]);
    run_git(repo_dir, &["commit", "-m", "initial LF blobs"]);

    // Simulate Windows checkout CRLF in text files -> zero phantom diffs!
    fs::write(repo_dir.join("run.sh"), b"#!/bin/sh\r\necho hi\r\n").unwrap();
    fs::write(repo_dir.join("docs/guide.md"), b"# Title\r\nbody\r\n").unwrap();

    let repo = OzoneRepo::discover(Some(repo_dir)).unwrap();
    let clean_status = compute_status(&repo).unwrap();
    assert!(
        clean_status.is_clean(),
        "CRLF matching text/eol=lf should produce zero phantom diffs: {:?}",
        clean_status.changes
    );

    // Edit `data.bin` (binary macro) preserving `\r\n` bytes, edit `docs/raw/spec.md` (-text -diff),
    // and add one line to `run.sh` with CRLF -> `oz commit` normalizes `run.sh` to LF, preserves
    // raw bytes on `data.bin`, and `oz diff` marks `-diff`/`binary` files as binary!
    fs::write(repo_dir.join("data.bin"), b"HDR\r\nCHANGED\r\n").unwrap();
    fs::write(repo_dir.join("docs/raw/spec.md"), b"raw\r\nedited\r\n").unwrap();
    fs::write(
        repo_dir.join("run.sh"),
        b"#!/bin/sh\r\necho hi\r\necho two\r\n",
    )
    .unwrap();

    let diff = compute_diff(&repo, &[]).unwrap();
    let rendered = render_unified_diff(&diff);
    let bin_change = diff.files.iter().find(|f| f.path == "data.bin").unwrap();
    assert!(bin_change.is_binary);
    let info_override = diff
        .files
        .iter()
        .find(|f| f.path == "docs/raw/spec.md")
        .unwrap();
    assert!(info_override.is_binary);
    assert!(rendered.contains("Binary files a/data.bin and b/data.bin differ"));
    assert!(rendered.contains("Binary files a/docs/raw/spec.md and b/docs/raw/spec.md differ"));

    let sh_change = diff.files.iter().find(|f| f.path == "run.sh").unwrap();
    assert!(!sh_change.is_binary);
    assert_eq!(sh_change.additions, 1);
    assert_eq!(sh_change.deletions, 0);

    commit_changes(&repo, &CommitOptions::new("chore: update attributes files")).unwrap();
    // Committed `run.sh` in Git ODB must be canonical LF, while `data.bin` & `spec.md` keep CRLF bytes!
    assert_eq!(
        run_git(repo_dir, &["show", "HEAD:run.sh"]),
        "#!/bin/sh\necho hi\necho two"
    );
    let spec_hex = run_git(repo_dir, &["rev-parse", "HEAD:docs/raw/spec.md"]);
    let bin_hex = run_git(repo_dir, &["rev-parse", "HEAD:data.bin"]);
    assert_eq!(
        Command::new("git")
            .args(["cat-file", "blob", &bin_hex])
            .current_dir(repo_dir)
            .output()
            .unwrap()
            .stdout,
        b"HDR\r\nCHANGED\r\n"
    );
    assert_eq!(
        Command::new("git")
            .args(["cat-file", "blob", &spec_hex])
            .current_dir(repo_dir)
            .output()
            .unwrap()
            .stdout,
        b"raw\r\nedited\r\n"
    );
}

#[test]
fn submodules_160000_status_diff_commit_and_untracked_nested_repo() {
    let tmp = TempDir::new().unwrap();
    let sub_src = tmp.path().join("sub_src");
    fs::create_dir_all(&sub_src).unwrap();
    run_git(&sub_src, &["init", "-b", "main"]);
    fs::write(sub_src.join("lib.rs"), "pub fn v() -> u32 { 1 }\n").unwrap();
    run_git(&sub_src, &["add", "."]);
    run_git(&sub_src, &["commit", "-m", "sub v1"]);
    let sub_v1 = run_git(&sub_src, &["rev-parse", "HEAD"]);

    fs::write(sub_src.join("lib.rs"), "pub fn v() -> u32 { 2 }\n").unwrap();
    run_git(&sub_src, &["add", "."]);
    run_git(&sub_src, &["commit", "-m", "sub v2"]);
    let sub_v2 = run_git(&sub_src, &["rev-parse", "HEAD"]);

    let parent = tmp.path().join("parent");
    fs::create_dir_all(&parent).unwrap();
    run_git(&parent, &["init", "-b", "main"]);
    run_git(&parent, &["config", "user.name", "Sub Tester"]);
    run_git(&parent, &["config", "user.email", "sub@example.com"]);
    fs::write(parent.join("app.txt"), "root\n").unwrap();
    run_git(&parent, &["add", "app.txt"]);
    run_git(&parent, &["commit", "-m", "root init"]);

    run_git(
        &parent,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub_src.to_str().unwrap(),
            "deps/sub",
        ],
    );
    run_git(&parent, &["-C", "deps/sub", "checkout", "-q", &sub_v1]);
    run_git(&parent, &["add", ".gitmodules", "deps/sub"]);
    run_git(&parent, &["commit", "-m", "add submodule at v1"]);

    // Also create an untracked nested git repository in `vendor/extra` -> must NOT be traversed!
    let untracked_nested = parent.join("vendor/extra");
    fs::create_dir_all(&untracked_nested).unwrap();
    run_git(&untracked_nested, &["init", "-b", "main"]);
    fs::write(untracked_nested.join("secret_inner.txt"), "inner\n").unwrap();

    let repo = OzoneRepo::discover(Some(&parent)).unwrap();
    let status_clean = compute_status(&repo).unwrap();
    assert!(
        status_clean.is_clean(),
        "clean submodule + untracked nested git repo should not leak inner files: {:?}",
        status_clean.changes
    );

    // Advance submodule checkout to v2
    run_git(&parent, &["-C", "deps/sub", "checkout", "-q", &sub_v2]);
    let status_dirty = compute_status(&repo).unwrap();
    assert_eq!(status_dirty.changes.len(), 1);
    assert_eq!(status_dirty.changes[0].path, "deps/sub");

    let diff = compute_diff(&repo, &[]).unwrap();
    let rendered = render_unified_diff(&diff);
    assert!(
        rendered.contains(&format!("-Subproject commit {sub_v1}")),
        "{rendered}"
    );
    assert!(
        rendered.contains(&format!("+Subproject commit {sub_v2}")),
        "{rendered}"
    );

    commit_changes(&repo, &CommitOptions::new("chore: bump submodule to v2")).unwrap();
    let ls_tree = run_git(&parent, &["ls-tree", "HEAD", "deps/sub"]);
    assert!(
        ls_tree.starts_with(&format!("160000 commit {sub_v2}")),
        "expected 160000 commit {sub_v2}, got: {ls_tree}"
    );
    assert!(compute_status(&repo).unwrap().is_clean());
    run_git(&parent, &["fsck", "--full"]);
}

#[test]
fn real_git_lfs_track_status_diff_commit_and_missing_filter_safety() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path();
    run_git(repo_dir, &["init", "-b", "main"]);
    run_git(repo_dir, &["config", "user.name", "LFS Tester"]);
    run_git(repo_dir, &["config", "user.email", "lfs@example.com"]);

    let lfs_install = Command::new("git")
        .args(["lfs", "install", "--local"])
        .current_dir(repo_dir)
        .output()
        .expect("git lfs install");
    assert!(
        lfs_install.status.success(),
        "git lfs install failed: {}",
        String::from_utf8_lossy(&lfs_install.stderr)
    );
    run_git(repo_dir, &["lfs", "track", "*.bin"]);

    let initial_payload = vec![0xABu8; 4096];
    fs::write(repo_dir.join("model.bin"), &initial_payload).unwrap();

    let repo = OzoneRepo::discover(Some(repo_dir)).unwrap();
    let dirty_new = compute_status(&repo).unwrap();
    assert!(dirty_new.changes.iter().any(|c| c.path == ".gitattributes"));
    assert!(dirty_new.changes.iter().any(|c| c.path == "model.bin"));

    commit_changes(&repo, &CommitOptions::new("feat: add LFS tracked asset")).unwrap();

    // Tree blob in Git MUST be an LFS pointer, NOT raw 4096 bytes!
    let head_blob = run_git(repo_dir, &["show", "HEAD:model.bin"]);
    assert!(
        head_blob.starts_with("version https://git-lfs.github.com/spec/v1"),
        "expected LFS pointer in HEAD, got: {head_blob}"
    );
    assert!(head_blob.contains("size 4096"));

    // Status right after commit must be clean (clean filter reproduces identical pointer)!
    let status_after = compute_status(&repo).unwrap();
    assert!(status_after.is_clean(), "{:?}", status_after.changes);

    // Modifying LFS file produces pointer diff in `oz diff` and commits updated pointer!
    let updated_payload = vec![0xCDu8; 8192];
    fs::write(repo_dir.join("model.bin"), &updated_payload).unwrap();
    let diff = compute_diff(&repo, &[]).unwrap();
    let rendered = render_unified_diff(&diff);
    assert!(rendered.contains("size 4096"), "{rendered}");
    assert!(rendered.contains("size 8192"), "{rendered}");

    // Removing `filter.lfs.clean` MUST make `oz commit` fail safely instead of committing raw binary!
    run_git(repo_dir, &["config", "--local", "--unset", "filter.lfs.clean"]);
    // Pass isolated empty global config via `GIT_CONFIG_GLOBAL`? Wait: if user's real ~/.gitconfig
    // has `filter.lfs.clean`, `-c filter.lfs.clean=` overrides it with empty string cleanly!
    let err = commit_changes(
        &repo,
        &CommitOptions::new("unsafe commit").with_config_overrides(vec![
            ("filter.lfs.clean".to_string(), String::new()),
        ]),
    )
    .unwrap_err();
    assert_eq!(err.error_code(), "check_failed", "got {err:?}");
}
