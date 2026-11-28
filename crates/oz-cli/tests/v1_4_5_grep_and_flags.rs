// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end v1.4.5 tests for Track 1:
//! - Top-level-only `-C` (unscoped from subcommands so `oz grep -C <N>` works) + global `--directory`
//! - Global `--no-pager` / `-P` wired into `pager.rs` (including real Unix PTY verification)
//! - Positional path auto-detection without `--` in `oz grep` and `oz log`
//! - `oz grep` `-c/--count`, `-A/-B/-C` context lines (working-tree & rev-tree), `--limit`/`--max-per-file`/`-m`,
//!   accurate omitted counts & truncation footer, smart `--agent` default cap, and `-E`/`-H` parity flags
//! - `oz log` `--decorate[=short|full|auto|no]`, `--no-decorate`, and `--abbrev-commit`

#![allow(clippy::too_many_lines)]

use clap::Parser;
use oz_cli::Cli;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn oz_bin() -> &'static str {
    env!("CARGO_BIN_EXE_oz")
}

fn run_oz_agent(dir: &Path, args: &[&str]) -> Output {
    Command::new(oz_bin())
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("OZ_AGENT", "1")
        .output()
        .expect("run oz")
}

fn run_oz_human(dir: &Path, args: &[&str]) -> Output {
    Command::new(oz_bin())
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("OZ_AGENT")
        .env_remove("CI")
        .output()
        .expect("run oz")
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

fn init_sample_repo() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("crates/alpha/src")).unwrap();
    fs::create_dir_all(repo.join("crates/beta/src")).unwrap();
    fs::create_dir_all(repo.join("docs")).unwrap();

    git_ok(&repo, &["init", "-b", "main"]);
    git_ok(&repo, &["config", "user.name", "Ozone Dev"]);
    git_ok(&repo, &["config", "user.email", "dev@ozone.test"]);
    git_ok(&repo, &["config", "core.autocrlf", "false"]);

    fs::write(
        repo.join("crates/alpha/src/lib.rs"),
        [
            "line 1 header",
            "line 2 before alpha",
            "pub fn alpha_token() -> i32 { 1 }",
            "line 4 after alpha",
            "line 5 middle",
            "line 6 middle",
            "line 7 before second",
            "pub fn beta_token() -> i32 { 2 }",
            "line 9 trailing",
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();

    fs::write(
        repo.join("crates/beta/src/mod.rs"),
        [
            "beta 1",
            "pub fn alpha_token() -> i32 { 10 }",
            "beta 3",
            "pub fn beta_token() -> i32 { 20 }",
            "beta 5",
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();

    fs::write(
        repo.join("docs/guide.md"),
        "Working tree docs mention alpha_token here.\n",
    )
    .unwrap();

    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "feat: initial alpha and beta"]);

    // Second commit touching only crates/alpha
    fs::write(
        repo.join("crates/alpha/src/extra.rs"),
        "pub const EXTRA: &str = \"alpha_extra\";\n",
    )
    .unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "feat(alpha): add extra module"]);
    git_ok(&repo, &["tag", "v0.1.0"]);

    // Third commit touching only crates/beta
    fs::write(
        repo.join("crates/beta/src/extra.rs"),
        "pub const BETA_EXTRA: &str = \"beta_extra\";\n",
    )
    .unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "feat(beta): add beta extra"]);

    // Create a branch `old-rev` at HEAD~2 with a unique token
    git_ok(&repo, &["checkout", "-b", "old-rev", "HEAD~2"]);
    fs::write(
        repo.join("crates/alpha/src/lib.rs"),
        [
            "old 1",
            "old 2 before",
            "pub fn rev_only_token() -> i32 { 99 }",
            "old 4 after",
            "old 5",
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "feat: old-rev token"]);

    // Create an ambiguous branch named `docs` (same name as the `docs/` directory on disk!)
    git_ok(&repo, &["checkout", "-b", "docs", "old-rev"]);
    fs::write(
        repo.join("crates/alpha/src/from_docs_branch.rs"),
        "pub const DOCS_BRANCH_ONLY: &str = \"alpha_token\";\n",
    )
    .unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "feat: commit on docs branch"]);

    git_ok(&repo, &["checkout", "main"]);
    (tmp, repo)
}

#[test]
fn cli_parses_all_v1_4_5_flags_and_unscoped_short_c() {
    // Top-level `-C <dir>` + `grep -C 3` work together without conflict
    let cli = Cli::try_parse_from([
        "oz",
        "-C",
        "/tmp/repo",
        "--no-pager",
        "grep",
        "-C",
        "3",
        "-E",
        "-H",
        "-n",
        "--limit",
        "50",
        "--max-per-file",
        "5",
        "foo|bar",
        "crates/a",
        "crates/b",
    ])
    .expect("parse full grep invocation");
    assert_eq!(cli.effective_directory(), Some(Path::new("/tmp/repo")));
    assert!(cli.no_pager);

    // `-C` after a non-grep subcommand fails (strictly top-level), while `--directory` remains global
    assert!(Cli::try_parse_from(["oz", "log", "-C", "/tmp/repo"]).is_err());
    assert!(Cli::try_parse_from(["oz", "log", "--directory", "/tmp/repo"]).is_ok());

    // Global `-P` / `--no-pager` accepted before or after any subcommand
    assert!(Cli::try_parse_from(["oz", "-P", "log", "--oneline"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "log", "-P", "--oneline"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "grep", "-P", "-c", "foo"]).is_ok());

    // `oz grep` `-A`, `-B`, `-C`, `-c`, `-m`, `--max-count`
    assert!(Cli::try_parse_from(["oz", "grep", "-A", "2", "-B", "1", "foo", "src"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "grep", "-m", "3", "foo"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "grep", "--max-count", "3", "foo"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "grep", "-c", "-l", "foo"]).is_err());

    // `oz log` `--decorate` bare + `=short|full|auto|no`, `--no-decorate`, `--abbrev-commit`
    assert!(
        Cli::try_parse_from([
            "oz",
            "log",
            "--oneline",
            "--decorate",
            "--abbrev-commit",
            "main",
            "crates/alpha"
        ])
        .is_ok()
    );
    assert!(Cli::try_parse_from(["oz", "log", "--decorate=full", "crates/alpha"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "log", "--decorate=auto"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "log", "--decorate=no"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "log", "--no-decorate", "--abbrev-commit"]).is_ok());
    assert!(Cli::try_parse_from(["oz", "log", "--decorate", "--no-decorate"]).is_err());
}

#[test]
fn grep_positional_path_auto_detection_and_disambiguation() {
    let (_tmp, repo) = init_sample_repo();

    // 1. Single path without `--`
    let single = run_oz_agent(
        &repo,
        &["grep", "alpha_token|beta_token", "crates/alpha", "--json"],
    );
    assert!(
        single.status.success(),
        "single path grep failed: {}",
        String::from_utf8_lossy(&single.stderr)
    );
    let single_json: Value = serde_json::from_slice(&single.stdout).unwrap();
    assert_eq!(single_json["data"]["total_matches"], 2);
    assert_eq!(
        single_json["data"]["matched_files"],
        serde_json::json!(["crates/alpha/src/lib.rs"])
    );
    assert!(single_json["data"].get("rev").is_none());

    // 2. Multiple paths without `--`
    let multi = run_oz_agent(
        &repo,
        &[
            "grep",
            "alpha_token",
            "crates/alpha",
            "crates/beta",
            "--json",
        ],
    );
    assert!(multi.status.success());
    let multi_json: Value = serde_json::from_slice(&multi.stdout).unwrap();
    assert_eq!(multi_json["data"]["total_matches"], 2);
    assert_eq!(
        multi_json["data"]["matched_files"],
        serde_json::json!(["crates/alpha/src/lib.rs", "crates/beta/src/mod.rs"])
    );

    // 3. Revision only (`old-rev`) without `--`
    let rev_only = run_oz_agent(&repo, &["grep", "rev_only_token", "old-rev", "--json"]);
    assert!(rev_only.status.success());
    let rev_only_json: Value = serde_json::from_slice(&rev_only.stdout).unwrap();
    assert_eq!(rev_only_json["data"]["rev"], "old-rev");
    assert_eq!(rev_only_json["data"]["total_matches"], 1);
    assert_eq!(
        rev_only_json["data"]["matches"][0]["path"],
        "crates/alpha/src/lib.rs"
    );

    // 4. Revision + path without `--`
    let rev_and_path = run_oz_agent(
        &repo,
        &[
            "grep",
            "rev_only_token",
            "old-rev",
            "crates/alpha",
            "--json",
        ],
    );
    assert!(rev_and_path.status.success());
    let rev_and_path_json: Value = serde_json::from_slice(&rev_and_path.stdout).unwrap();
    assert_eq!(rev_and_path_json["data"]["rev"], "old-rev");
    assert_eq!(rev_and_path_json["data"]["total_matches"], 1);

    let rev_wrong_path = run_oz_agent(
        &repo,
        &["grep", "rev_only_token", "old-rev", "crates/beta", "--json"],
    );
    assert!(rev_wrong_path.status.success());
    let rev_wrong_json: Value = serde_json::from_slice(&rev_wrong_path.stdout).unwrap();
    assert_eq!(rev_wrong_json["data"]["rev"], "old-rev");
    assert_eq!(rev_wrong_json["data"]["total_matches"], 0);

    // 5. Revision + explicit `-- <path>`
    let rev_explicit = run_oz_agent(
        &repo,
        &[
            "grep",
            "--json",
            "rev_only_token",
            "old-rev",
            "--",
            "crates/alpha",
        ],
    );
    assert!(rev_explicit.status.success());
    let rev_explicit_json: Value = serde_json::from_slice(&rev_explicit.stdout).unwrap();
    assert_eq!(rev_explicit_json["data"]["rev"], "old-rev");
    assert_eq!(rev_explicit_json["data"]["total_matches"], 1);

    // 6. Ambiguous name (`docs` exists BOTH as a branch and as a directory on disk):
    //    without `--`, prefer the existing working-tree directory `docs/`;
    //    with `--`, treat `docs` before `--` as the revision;
    //    with `refs/heads/docs`, resolve the branch unambiguously.
    let ambig_disk = run_oz_agent(&repo, &["grep", "alpha_token", "docs", "--json"]);
    assert!(ambig_disk.status.success());
    let ambig_disk_json: Value = serde_json::from_slice(&ambig_disk.stdout).unwrap();
    assert!(ambig_disk_json["data"].get("rev").is_none());
    assert_eq!(
        ambig_disk_json["data"]["matched_files"],
        serde_json::json!(["docs/guide.md"])
    );

    let ambig_rev_dash = run_oz_agent(
        &repo,
        &[
            "grep",
            "--json",
            "alpha_token",
            "docs",
            "--",
            "crates/alpha",
        ],
    );
    assert!(ambig_rev_dash.status.success());
    let ambig_rev_json: Value = serde_json::from_slice(&ambig_rev_dash.stdout).unwrap();
    assert_eq!(ambig_rev_json["data"]["rev"], "docs");
    assert_eq!(
        ambig_rev_json["data"]["matched_files"],
        serde_json::json!(["crates/alpha/src/from_docs_branch.rs"])
    );

    let ambig_full_ref = run_oz_agent(
        &repo,
        &["grep", "DOCS_BRANCH_ONLY", "refs/heads/docs", "--json"],
    );
    assert!(ambig_full_ref.status.success());
    let ambig_full_json: Value = serde_json::from_slice(&ambig_full_ref.stdout).unwrap();
    assert_eq!(ambig_full_json["data"]["rev"], "refs/heads/docs");
    assert_eq!(ambig_full_json["data"]["total_matches"], 1);

    // 7. Pathspec exclusion (`:!docs`) and glob (`*.md`) work without `--`
    let glob_out = run_oz_agent(&repo, &["grep", "alpha_token", "*.md", "--json"]);
    assert!(glob_out.status.success());
    let glob_json: Value = serde_json::from_slice(&glob_out.stdout).unwrap();
    assert_eq!(
        glob_json["data"]["matched_files"],
        serde_json::json!(["docs/guide.md"])
    );

    let excl_out = run_oz_agent(&repo, &["grep", "alpha_token", ":!docs", "--json"]);
    assert!(excl_out.status.success());
    let excl_json: Value = serde_json::from_slice(&excl_out.stdout).unwrap();
    assert_eq!(
        excl_json["data"]["matched_files"],
        serde_json::json!(["crates/alpha/src/lib.rs", "crates/beta/src/mod.rs"])
    );
}

#[test]
fn grep_count_context_limits_parity_flags_and_agent_cap() {
    let (_tmp, repo) = init_sample_repo();

    // 1. `-c` / `--count` in human & JSON modes (working tree and rev tree)
    let count_human = run_oz_agent(&repo, &["grep", "-c", "alpha_token|beta_token", "-E", "-H"]);
    assert!(
        count_human.status.success(),
        "grep -c failed: {}",
        String::from_utf8_lossy(&count_human.stderr)
    );
    let count_txt = String::from_utf8_lossy(&count_human.stdout);
    assert!(
        count_txt.contains("crates/alpha/src/lib.rs:2"),
        "got:\n{count_txt}"
    );
    assert!(
        count_txt.contains("crates/beta/src/mod.rs:2"),
        "got:\n{count_txt}"
    );
    assert!(count_txt.contains("docs/guide.md:1"), "got:\n{count_txt}");

    let count_json_out = run_oz_agent(
        &repo,
        &["grep", "--count", "alpha_token|beta_token", "--json"],
    );
    assert!(count_json_out.status.success());
    let count_json: Value = serde_json::from_slice(&count_json_out.stdout).unwrap();
    assert_eq!(count_json["data"]["total_matches"], 5);
    assert_eq!(
        count_json["data"]["file_counts"]["crates/alpha/src/lib.rs"],
        2
    );
    assert_eq!(
        count_json["data"]["file_counts"]["crates/beta/src/mod.rs"],
        2
    );
    assert_eq!(count_json["data"]["file_counts"]["docs/guide.md"], 1);
    assert_eq!(count_json["data"]["matches"], serde_json::json!([]));

    // Rev-tree `-c`
    let rev_count = run_oz_agent(&repo, &["grep", "-c", "rev_only_token", "old-rev"]);
    assert!(rev_count.status.success());
    let rev_count_txt = String::from_utf8_lossy(&rev_count.stdout);
    assert_eq!(rev_count_txt.trim(), "old-rev:crates/alpha/src/lib.rs:1");

    // 2. Context lines `-C 1` in working tree: `:` for matches, `-` for context, `--` between hunks & files
    let ctx_human = run_oz_agent(
        &repo,
        &[
            "grep",
            "-C",
            "1",
            "alpha_token|beta_token",
            "crates/alpha",
            "crates/beta",
        ],
    );
    assert!(
        ctx_human.status.success(),
        "grep -C 1 failed: {}",
        String::from_utf8_lossy(&ctx_human.stderr)
    );
    let ctx_txt = String::from_utf8_lossy(&ctx_human.stdout);
    let ctx_lines: Vec<&str> = ctx_txt.lines().collect();
    // In crates/alpha/src/lib.rs: matches at 3 and 8 with -C 1 -> hunk 1 (2,3,4), `--`, hunk 2 (7,8,9),
    // then `--` before crates/beta/src/mod.rs where matches at 2 and 4 share line 3 (merged without duplicate!).
    assert_eq!(
        ctx_lines,
        vec![
            "crates/alpha/src/lib.rs-2-line 2 before alpha",
            "crates/alpha/src/lib.rs:3:pub fn alpha_token() -> i32 { 1 }",
            "crates/alpha/src/lib.rs-4-line 4 after alpha",
            "--",
            "crates/alpha/src/lib.rs-7-line 7 before second",
            "crates/alpha/src/lib.rs:8:pub fn beta_token() -> i32 { 2 }",
            "crates/alpha/src/lib.rs-9-line 9 trailing",
            "--",
            "crates/beta/src/mod.rs-1-beta 1",
            "crates/beta/src/mod.rs:2:pub fn alpha_token() -> i32 { 10 }",
            "crates/beta/src/mod.rs-3-beta 3",
            "crates/beta/src/mod.rs:4:pub fn beta_token() -> i32 { 20 }",
            "crates/beta/src/mod.rs-5-beta 5",
        ]
    );

    // 3. Context lines `-A 1 -B 1` on a committed revision (`old-rev`) in both human and JSON modes
    let rev_ctx_human = run_oz_agent(
        &repo,
        &["grep", "-B", "1", "-A", "1", "rev_only_token", "old-rev"],
    );
    assert!(rev_ctx_human.status.success());
    let rev_ctx_txt = String::from_utf8_lossy(&rev_ctx_human.stdout);
    let rev_ctx_lines: Vec<&str> = rev_ctx_txt.lines().collect();
    assert_eq!(
        rev_ctx_lines,
        vec![
            "old-rev:crates/alpha/src/lib.rs-2-old 2 before",
            "old-rev:crates/alpha/src/lib.rs:3:pub fn rev_only_token() -> i32 { 99 }",
            "old-rev:crates/alpha/src/lib.rs-4-old 4 after",
        ]
    );

    let rev_ctx_json_out = run_oz_agent(
        &repo,
        &["grep", "-C", "1", "rev_only_token", "old-rev", "--json"],
    );
    assert!(rev_ctx_json_out.status.success());
    let rev_ctx_json: Value = serde_json::from_slice(&rev_ctx_json_out.stdout).unwrap();
    assert_eq!(
        rev_ctx_json["data"]["matches"][0]["before_context"][0]["line_number"],
        2
    );
    assert_eq!(
        rev_ctx_json["data"]["matches"][0]["after_context"][0]["line_number"],
        4
    );

    // Without context flags, `before_context` / `after_context` are omitted from JSON for backward compatibility
    let no_ctx_json_out = run_oz_agent(&repo, &["grep", "rev_only_token", "old-rev", "--json"]);
    assert!(no_ctx_json_out.status.success());
    let no_ctx_json: Value = serde_json::from_slice(&no_ctx_json_out.stdout).unwrap();
    assert!(
        no_ctx_json["data"]["matches"][0]
            .get("before_context")
            .is_none()
    );
    assert!(
        no_ctx_json["data"]["matches"][0]
            .get("after_context")
            .is_none()
    );

    // 4. `--limit` & `--max-per-file` (`-m`) with exact omitted counts & truncation footer
    let limited_human = run_oz_agent(&repo, &["grep", "--limit", "2", "alpha_token|beta_token"]);
    assert!(limited_human.status.success());
    let limited_txt = String::from_utf8_lossy(&limited_human.stdout);
    assert!(
        limited_txt
            .contains("... truncated: +3 more matches across 2 files (pass --limit 0 for all)"),
        "expected exact truncation footer, got:\n{limited_txt}"
    );

    let limited_json_out = run_oz_agent(
        &repo,
        &["grep", "--limit", "2", "alpha_token|beta_token", "--json"],
    );
    assert!(limited_json_out.status.success());
    let limited_json: Value = serde_json::from_slice(&limited_json_out.stdout).unwrap();
    assert_eq!(limited_json["data"]["truncated"], true);
    assert_eq!(limited_json["data"]["total_matches"], 5);
    assert_eq!(limited_json["data"]["omitted_matches"], 3);
    assert_eq!(limited_json["data"]["omitted_files"], 2);
    assert_eq!(limited_json["data"]["matches"].as_array().unwrap().len(), 2);

    let per_file_human = run_oz_agent(&repo, &["grep", "-m", "1", "alpha_token|beta_token"]);
    assert!(per_file_human.status.success());
    let per_file_txt = String::from_utf8_lossy(&per_file_human.stdout);
    assert!(
        per_file_txt.contains(
            "... truncated: +2 more matches across 2 files (pass --max-per-file 0 for all)"
        ),
        "expected per-file truncation footer, got:\n{per_file_txt}"
    );

    // 5. Smart `--agent` default cap (100 matches) vs `--limit 0` override vs human uncapped vs `-c`/`-l` uncapped
    let many_a: String = (1..=50).map(|i| format!("BULK_HIT_LINE_{i}\n")).collect();
    let many_b: String = (1..=50).map(|i| format!("BULK_HIT_LINE_{i}\n")).collect();
    let many_c: String = (1..=35).map(|i| format!("BULK_HIT_LINE_{i}\n")).collect();
    fs::write(repo.join("crates/alpha/src/bulk_a.txt"), many_a).unwrap();
    fs::write(repo.join("crates/alpha/src/bulk_b.txt"), many_b).unwrap();
    fs::write(repo.join("crates/beta/src/bulk_c.txt"), many_c).unwrap();

    // Agent mode defaults to 100 matches with exact omitted count (+35 across 1 file)
    let agent_default_human = run_oz_agent(&repo, &["grep", "BULK_HIT_LINE_"]);
    assert!(agent_default_human.status.success());
    let agent_txt = String::from_utf8_lossy(&agent_default_human.stdout);
    assert!(
        agent_txt
            .contains("... truncated: +35 more matches across 1 file (pass --limit 0 for all)"),
        "expected agent cap footer, got tail:\n{}",
        agent_txt
            .lines()
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .join("\n")
    );

    let agent_default_json_out = run_oz_agent(&repo, &["grep", "BULK_HIT_LINE_", "--json"]);
    assert!(agent_default_json_out.status.success());
    let agent_default_json: Value = serde_json::from_slice(&agent_default_json_out.stdout).unwrap();
    assert_eq!(agent_default_json["data"]["truncated"], true);
    assert_eq!(agent_default_json["data"]["total_matches"], 135);
    assert_eq!(agent_default_json["data"]["omitted_matches"], 35);
    assert_eq!(agent_default_json["data"]["omitted_files"], 1);
    assert_eq!(
        agent_default_json["data"]["matches"]
            .as_array()
            .unwrap()
            .len(),
        100
    );

    // `--limit 0` disables the agent default cap
    let agent_uncapped = run_oz_agent(&repo, &["grep", "BULK_HIT_LINE_", "--limit", "0", "--json"]);
    assert!(agent_uncapped.status.success());
    let agent_uncapped_json: Value = serde_json::from_slice(&agent_uncapped.stdout).unwrap();
    assert_eq!(agent_uncapped_json["data"]["truncated"], false);
    assert_eq!(agent_uncapped_json["data"]["total_matches"], 135);
    assert_eq!(
        agent_uncapped_json["data"]["matches"]
            .as_array()
            .unwrap()
            .len(),
        135
    );

    // `-c` in agent mode is NOT artificially capped at 100 matches
    let agent_count = run_oz_agent(&repo, &["grep", "-c", "BULK_HIT_LINE_", "--json"]);
    assert!(agent_count.status.success());
    let agent_count_json: Value = serde_json::from_slice(&agent_count.stdout).unwrap();
    assert_eq!(agent_count_json["data"]["truncated"], false);
    assert_eq!(agent_count_json["data"]["total_matches"], 135);

    // Non-agent (human) mode is uncapped by default
    let human_uncapped = run_oz_human(&repo, &["grep", "BULK_HIT_LINE_", "--json"]);
    assert!(human_uncapped.status.success());
    let human_uncapped_json: Value = serde_json::from_slice(&human_uncapped.stdout).unwrap();
    assert_eq!(human_uncapped_json["data"]["truncated"], false);
    assert_eq!(
        human_uncapped_json["data"]["matches"]
            .as_array()
            .unwrap()
            .len(),
        135
    );
}

#[test]
fn log_path_auto_detection_and_git_flag_aliases() {
    let (_tmp, repo) = init_sample_repo();

    // 1. `oz log -n 5 crates/alpha` WITHOUT `--` filters commits touching crates/alpha
    let alpha_log = run_oz_agent(&repo, &["log", "-n", "5", "crates/alpha", "--json"]);
    assert!(
        alpha_log.status.success(),
        "log path auto-detect failed: {}",
        String::from_utf8_lossy(&alpha_log.stderr)
    );
    let alpha_json: Value = serde_json::from_slice(&alpha_log.stdout).unwrap();
    let alpha_subjects: Vec<&str> = alpha_json["data"]["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["subject"].as_str().unwrap())
        .collect();
    assert_eq!(
        alpha_subjects,
        vec![
            "feat(alpha): add extra module",
            "feat: initial alpha and beta"
        ]
    );

    // 2. Rev + path without `--` (`oz log main crates/beta`)
    let rev_path_log = run_oz_agent(&repo, &["log", "main", "crates/beta", "--json"]);
    assert!(rev_path_log.status.success());
    let rev_path_json: Value = serde_json::from_slice(&rev_path_log.stdout).unwrap();
    let beta_subjects: Vec<&str> = rev_path_json["data"]["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["subject"].as_str().unwrap())
        .collect();
    assert_eq!(
        beta_subjects,
        vec!["feat(beta): add beta extra", "feat: initial alpha and beta"]
    );

    // 3. Range + path without `--` (`oz log HEAD~2..HEAD crates/alpha`)
    let range_path_log = run_oz_agent(&repo, &["log", "HEAD~2..HEAD", "crates/alpha", "--json"]);
    assert!(range_path_log.status.success());
    let range_json: Value = serde_json::from_slice(&range_path_log.stdout).unwrap();
    let range_subjects: Vec<&str> = range_json["data"]["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["subject"].as_str().unwrap())
        .collect();
    assert_eq!(range_subjects, vec!["feat(alpha): add extra module"]);

    // 4. Unknown non-rev non-path still errors cleanly with exit code 2
    let bogus = run_oz_agent(&repo, &["log", "totally_nonexistent_ref_xyz", "--json"]);
    assert_eq!(bogus.status.code(), Some(2));

    // 5. `--decorate` (bare, does not swallow positional `crates/alpha`), `--decorate=full`,
    //    `--decorate=no`, `--no-decorate`, and `--abbrev-commit`
    let dec_bare = run_oz_agent(
        &repo,
        &[
            "log",
            "--oneline",
            "--decorate",
            "--abbrev-commit",
            "crates/alpha",
        ],
    );
    assert!(
        dec_bare.status.success(),
        "log --decorate failed: {}",
        String::from_utf8_lossy(&dec_bare.stderr)
    );
    let dec_bare_txt = String::from_utf8_lossy(&dec_bare.stdout);
    assert!(dec_bare_txt.contains("tag: v0.1.0"), "got:\n{dec_bare_txt}");
    assert!(!dec_bare_txt.contains("feat(beta): add beta extra"));

    let dec_full = run_oz_agent(&repo, &["log", "-n", "2", "--oneline", "--decorate=full"]);
    assert!(dec_full.status.success());
    let dec_full_txt = String::from_utf8_lossy(&dec_full.stdout);
    assert!(
        dec_full_txt.contains("HEAD -> refs/heads/main"),
        "expected full ref decoration, got:\n{dec_full_txt}"
    );
    assert!(
        dec_full_txt.contains("tag: refs/tags/v0.1.0"),
        "expected full tag decoration, got:\n{dec_full_txt}"
    );

    let no_dec = run_oz_agent(&repo, &["log", "-n", "2", "--oneline", "--no-decorate"]);
    assert!(no_dec.status.success());
    let no_dec_txt = String::from_utf8_lossy(&no_dec.stdout);
    assert!(
        !no_dec_txt.contains("HEAD -> main") && !no_dec_txt.contains("tag: v0.1.0"),
        "--no-decorate must hide ref decorations, got:\n{no_dec_txt}"
    );

    let dec_no = run_oz_agent(&repo, &["log", "-n", "2", "--oneline", "--decorate=no"]);
    assert!(dec_no.status.success());
    let dec_no_txt = String::from_utf8_lossy(&dec_no.stdout);
    assert!(
        !dec_no_txt.contains("HEAD -> main") && !dec_no_txt.contains("tag: v0.1.0"),
        "--decorate=no must hide ref decorations, got:\n{dec_no_txt}"
    );
}

#[cfg(unix)]
#[test]
fn no_pager_and_short_p_disable_pager_even_on_real_pty() {
    use std::io::Read as _;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;

    let (tmp, repo) = init_sample_repo();
    let marker = tmp.path().join("pager_ran");
    let spy_pager = tmp.path().join("spy-pager.sh");
    fs::write(
        &spy_pager,
        format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&spy_pager, fs::Permissions::from_mode(0o755)).unwrap();

    let run_on_pty = |extra_args: &[&str]| -> String {
        let mut master_fd: libc::c_int = -1;
        let mut slave_fd: libc::c_int = -1;
        #[cfg(all(
            any(target_os = "linux", target_os = "android"),
            not(target_env = "uclibc")
        ))]
        let (termp, winp) = (std::ptr::null(), std::ptr::null());
        #[cfg(not(all(
            any(target_os = "linux", target_os = "android"),
            not(target_env = "uclibc")
        )))]
        let (termp, winp) = (std::ptr::null_mut(), std::ptr::null_mut());

        // SAFETY: `master_fd` and `slave_fd` are valid writable out-pointers and null pointers
        // are permitted by `openpty(3)`.
        let rc = unsafe {
            libc::openpty(
                &raw mut master_fd,
                &raw mut slave_fd,
                std::ptr::null_mut(),
                termp,
                winp,
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        // SAFETY: `openpty(3)` returned 0 and handed over newly opened file descriptors.
        let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
        // SAFETY: `openpty(3)` returned 0 and handed over newly opened file descriptors.
        let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };

        let mut cmd = Command::new(oz_bin());
        cmd.arg("-C")
            .arg(&repo)
            .args(extra_args)
            .env("OZ_PAGER", spy_pager.to_str().unwrap())
            .env_remove("OZ_AGENT")
            .env_remove("CI")
            .stdin(Stdio::null())
            .stdout(Stdio::from(slave))
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("spawn oz on pty");
        // Drop `cmd` immediately so the parent closes its copy of the slave fd and master gets EOF.
        drop(cmd);
        let mut master_file = std::fs::File::from(master);
        let mut buf = Vec::new();
        let _ = master_file.read_to_end(&mut buf);
        let status = child.wait().expect("wait oz");
        assert!(status.success(), "oz {:?} failed on pty", extra_args);
        String::from_utf8_lossy(&buf).into_owned()
    };

    // 1. `--no-pager` MUST suppress the pager even on a real TTY!
    let out_long = run_on_pty(&["--no-pager", "log", "--oneline", "-n", "1"]);
    assert!(out_long.contains("feat(beta): add beta extra"));
    assert!(
        !marker.exists(),
        "--no-pager must prevent pager execution on a real PTY"
    );

    // 2. `-P` MUST suppress the pager even on a real TTY!
    let out_short = run_on_pty(&["-P", "log", "--oneline", "-n", "1"]);
    assert!(out_short.contains("feat(beta): add beta extra"));
    assert!(
        !marker.exists(),
        "-P must prevent pager execution on a real PTY"
    );

    // 3. Control check: without `--no-pager`/`-P`, the same PTY DOES invoke `$OZ_PAGER`!
    let out_paged = run_on_pty(&["log", "--oneline", "-n", "1"]);
    assert!(out_paged.contains("feat(beta): add beta extra"));
    assert!(
        marker.exists(),
        "control check: PTY without --no-pager should invoke $OZ_PAGER"
    );
}
