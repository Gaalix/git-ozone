// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end CLI UX tests for `oz oplog`, `oz undo --list`, and grouped `--help` output.

#![allow(clippy::too_many_lines)]

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

fn oz_bin() -> &'static str {
    env!("CARGO_BIN_EXE_oz")
}

fn run_oz(dir: &Path, args: &[&str]) -> Output {
    Command::new(oz_bin())
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("OZ_AGENT", "1")
        .output()
        .expect("run oz")
}

fn run_oz_raw(args: &[&str]) -> Output {
    Command::new(oz_bin()).args(args).output().expect("run oz")
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
    fs::create_dir_all(&repo).unwrap();
    git_ok(&repo, &["init", "-b", "main"]);
    git_ok(&repo, &["config", "user.name", "Ozone Tester"]);
    git_ok(&repo, &["config", "user.email", "tester@example.com"]);
    git_ok(&repo, &["config", "core.autocrlf", "false"]);
    fs::write(repo.join("notes.txt"), "initial\n").unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "chore: init"]);
    (tmp, repo)
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

#[test]
fn oz_oplog_and_undo_list_human_and_json_end_to_end() {
    let (_tmp, repo) = init_sample_repo();
    let head_initial = git_ok(&repo, &["rev-parse", "HEAD"]);

    // 1. Empty OpLog: both `oz oplog` and `oz undo --list` succeed and are strictly non-destructive.
    let empty_oplog = run_oz(&repo, &["oplog"]);
    assert!(
        empty_oplog.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&empty_oplog.stderr)
    );
    let empty_undo_list = run_oz(&repo, &["undo", "--list"]);
    assert!(empty_undo_list.status.success());
    assert_eq!(empty_oplog.stdout, empty_undo_list.stdout);
    assert!(
        String::from_utf8_lossy(&empty_oplog.stdout).contains("No operations recorded in OpLog")
    );

    let empty_json = run_oz(&repo, &["oplog", "--json"]);
    assert!(empty_json.status.success());
    let empty_env = parse_json_stdout(&empty_json);
    assert_eq!(empty_env["schema"], "oz.v1");
    assert_eq!(empty_env["ok"], true);
    assert_eq!(empty_env["data"]["total_entries"], 0);
    assert_eq!(empty_env["data"]["entries"].as_array().unwrap().len(), 0);
    assert!(empty_env["data"]["undo_cursor_op_id"].is_null());
    assert!(empty_env["data"]["redo_cursor_op_id"].is_null());
    assert!(
        !repo.join(".git/ozone/oplog").exists(),
        "empty oz oplog / undo --list must not create .git/ozone/oplog"
    );

    // 2. Create commit 1 -> checkpoint -> commit 2.
    fs::write(repo.join("notes.txt"), "step 1\n").unwrap();
    let c1_out = run_oz(&repo, &["commit", "-m", "feat: step one", "--json"]);
    assert!(c1_out.status.success());
    let c1_op_id = parse_json_stdout(&c1_out)["op_id"]
        .as_str()
        .unwrap()
        .to_string();

    fs::write(repo.join("notes.txt"), "step 1 wip\n").unwrap();
    let checkpoint_out = run_oz(&repo, &["checkpoint", "-m", "before step two", "--json"]);
    assert!(checkpoint_out.status.success());
    let checkpoint_op_id = parse_json_stdout(&checkpoint_out)["op_id"]
        .as_str()
        .unwrap()
        .to_string();

    fs::write(repo.join("notes.txt"), "step 2\n").unwrap();
    let c2_out = run_oz(&repo, &["commit", "-m", "feat: step two", "--json"]);
    assert!(c2_out.status.success());
    let c2_op_id = parse_json_stdout(&c2_out)["op_id"]
        .as_str()
        .unwrap()
        .to_string();
    let head_after_c2 = git_ok(&repo, &["rev-parse", "HEAD"]);

    // 3. Compare `oz oplog` and `oz undo --list` (human + JSON) and verify non-destructiveness.
    let human_oplog = run_oz(&repo, &["oplog"]);
    let human_undo_list = run_oz(&repo, &["undo", "--list"]);
    assert!(human_oplog.status.success());
    assert!(human_undo_list.status.success());
    assert_eq!(human_oplog.stdout, human_undo_list.stdout);

    let human_text = String::from_utf8_lossy(&human_oplog.stdout);
    assert!(human_text.contains(&c2_op_id));
    assert!(human_text.contains(&checkpoint_op_id));
    assert!(human_text.contains(&c1_op_id));
    assert!(human_text.contains("feat: step two"));
    assert!(human_text.contains("before step two"));
    assert!(human_text.contains("feat: step one"));
    assert!(human_text.contains("[active, next-undo, HEAD]"));

    let json_oplog = run_oz(&repo, &["oplog", "--json"]);
    let json_undo_list = run_oz(&repo, &["undo", "--list", "--json"]);
    assert!(json_oplog.status.success());
    assert!(json_undo_list.status.success());
    let env_oplog = parse_json_stdout(&json_oplog);
    let env_undo_list = parse_json_stdout(&json_undo_list);
    assert_eq!(env_oplog["schema"], "oz.v1");
    assert_eq!(env_oplog["ok"], true);
    assert_eq!(env_oplog["data"], env_undo_list["data"]);

    let entries = env_oplog["data"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0]["op_id"], c2_op_id);
    assert_eq!(entries[0]["kind"], "mutate");
    assert_eq!(entries[0]["status"], "active");
    assert_eq!(entries[0]["is_head"], true);
    assert_eq!(entries[0]["is_next_undo"], true);
    assert_eq!(entries[0]["is_next_redo"], false);

    assert_eq!(entries[1]["op_id"], checkpoint_op_id);
    assert_eq!(entries[1]["kind"], "checkpoint");
    assert_eq!(entries[1]["status"], "active");
    assert_eq!(entries[1]["summary"], "before step two");

    assert_eq!(entries[2]["op_id"], c1_op_id);
    assert_eq!(entries[2]["status"], "active");
    assert_eq!(env_oplog["data"]["undo_cursor_op_id"], c2_op_id);
    assert!(env_oplog["data"]["redo_cursor_op_id"].is_null());
    assert_eq!(env_oplog["data"]["active_count"], 3);

    // HEAD & working tree must be untouched by inspection commands.
    assert_eq!(git_ok(&repo, &["rev-parse", "HEAD"]), head_after_c2);
    assert_eq!(
        fs::read_to_string(repo.join("notes.txt")).unwrap(),
        "step 2\n"
    );

    // 4. Undo 1 step, verify undo/redo cursor badges, then pick `c1_op_id` from oplog for targeted undo.
    let undo_out = run_oz(&repo, &["undo", "--json"]);
    assert!(undo_out.status.success());
    let undo_op_id = parse_json_stdout(&undo_out)["op_id"]
        .as_str()
        .unwrap()
        .to_string();

    let after_undo_json = run_oz(&repo, &["oplog", "--json"]);
    assert!(after_undo_json.status.success());
    let after_undo_env = parse_json_stdout(&after_undo_json);
    assert_eq!(after_undo_env["data"]["head_op_id"], undo_op_id);
    assert_eq!(
        after_undo_env["data"]["undo_cursor_op_id"],
        checkpoint_op_id
    );
    assert_eq!(after_undo_env["data"]["redo_cursor_op_id"], undo_op_id);
    assert_eq!(after_undo_env["data"]["active_count"], 2);
    assert_eq!(after_undo_env["data"]["redo_depth"], 1);
    let after_entries = after_undo_env["data"]["entries"].as_array().unwrap();
    assert_eq!(after_entries[0]["op_id"], undo_op_id);
    assert_eq!(after_entries[0]["status"], "redoable");
    assert_eq!(after_entries[0]["is_next_redo"], true);
    assert_eq!(after_entries[1]["op_id"], c2_op_id);
    assert_eq!(after_entries[1]["status"], "undone");
    assert_eq!(after_entries[2]["op_id"], checkpoint_op_id);
    assert_eq!(after_entries[2]["status"], "active");
    assert_eq!(after_entries[2]["is_next_undo"], true);

    // 5. `-n` / `--limit` works identically for both `oz oplog` and `oz undo --list`.
    let limited_oplog = run_oz(&repo, &["oplog", "-n", "2", "--json"]);
    let limited_undo = run_oz(&repo, &["undo", "--list", "--limit", "2", "--json"]);
    assert!(limited_oplog.status.success());
    assert!(limited_undo.status.success());
    let lim_env = parse_json_stdout(&limited_oplog);
    assert_eq!(lim_env["data"], parse_json_stdout(&limited_undo)["data"]);
    assert_eq!(lim_env["data"]["entries"].as_array().unwrap().len(), 2);
    assert_eq!(lim_env["data"]["total_entries"], 4);
    assert_eq!(lim_env["data"]["truncated"], true);

    // 6. Use an OP_ID inspected from `oz oplog` to run targeted `oz undo <OP_ID>`.
    // Undoing `c1_op_id` restores HEAD to `head_initial` and restores the pre-commit dirty file (`step 1\n`).
    let target_undo = run_oz(&repo, &["undo", &c1_op_id, "--json"]);
    assert!(
        target_undo.status.success(),
        "targeted undo failed: {}",
        String::from_utf8_lossy(&target_undo.stderr)
    );
    assert_eq!(git_ok(&repo, &["rev-parse", "HEAD"]), head_initial);
    assert_eq!(
        fs::read_to_string(repo.join("notes.txt")).unwrap(),
        "step 1\n"
    );

    // 7. Invalid flag usage exits with code 2.
    let zero_limit = run_oz(&repo, &["oplog", "-n", "0", "--json"]);
    assert_eq!(zero_limit.status.code(), Some(2));
    let conflict_list = run_oz(&repo, &["undo", "--list", "--steps", "1"]);
    assert_eq!(conflict_list.status.code(), Some(2));
}

#[test]
fn oz_help_groups_subcommands_and_updates_stale_descriptions() {
    let help = run_oz_raw(&["--help"]);
    assert!(help.status.success());
    let out = String::from_utf8_lossy(&help.stdout);

    // Grouped domain headings present and no fallback group.
    for heading in [
        "Inspection:",
        "Editing & Undo:",
        "Branches & History:",
        "Workspaces & Stacks:",
        "Remotes & GitHub (PR / Issue / CI / Release):",
        "Maintenance, Agents & Setup:",
    ] {
        assert!(
            out.contains(heading),
            "expected domain heading '{heading}' in `oz --help`:\n{out}"
        );
    }
    assert!(
        !out.contains("Other Commands:"),
        "unexpected fallback heading in `oz --help`:\n{out}"
    );

    // Updated top-level descriptions for `pr`, `issue`, `ci`, `oplog`, `undo`.
    let pr_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("pr "))
        .expect("pr line in oz --help");
    for sub in [
        "create", "edit", "list", "status", "view", "comments", "reply", "resolve", "diff", "read",
        "review", "stack",
    ] {
        assert!(
            pr_line.contains(sub),
            "expected '{sub}' in `pr` help summary: {pr_line}"
        );
    }

    let issue_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("issue "))
        .expect("issue line in oz --help");
    for sub in ["view", "list", "comment"] {
        assert!(
            issue_line.contains(sub),
            "expected '{sub}' in `issue` help summary: {issue_line}"
        );
    }

    let ci_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("ci "))
        .expect("ci line in oz --help");
    for flag in ["--watch", "--runs", "--pr", "--workflow", "--job"] {
        assert!(
            ci_line.contains(flag),
            "expected '{flag}' in `ci` help summary: {ci_line}"
        );
    }

    let oplog_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("oplog "))
        .expect("oplog line in oz --help");
    assert!(
        oplog_line.contains("oz undo"),
        "expected `oplog` help summary to cross-reference `oz undo`: {oplog_line}"
    );

    let undo_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("undo "))
        .expect("undo line in oz --help");
    assert!(
        undo_line.contains("oz oplog") && undo_line.contains("--list"),
        "expected `undo` help summary to cross-reference `oz oplog` and `--list`: {undo_line}"
    );

    // Subcommand `--help` outputs stay clean and accurate.
    for (sub_args, expected_snippets) in [
        (vec!["oplog", "--help"], vec!["--limit", "--all", "oz undo"]),
        (
            vec!["undo", "--help"],
            vec!["--list", "--limit", "--steps", "oz oplog"],
        ),
        (
            vec!["pr", "--help"],
            vec![
                "create", "edit", "list", "status", "reply", "resolve", "stack",
            ],
        ),
        (vec!["issue", "--help"], vec!["view", "list", "comment"]),
        (
            vec!["ci", "--help"],
            vec!["--watch", "--runs", "--pr", "--workflow", "--job"],
        ),
    ] {
        let sub = run_oz_raw(&sub_args);
        assert!(sub.status.success(), "help {sub_args:?} failed");
        let text = String::from_utf8_lossy(&sub.stdout);
        for snippet in expected_snippets {
            assert!(
                text.contains(snippet),
                "expected '{snippet}' in `oz {}`:\n{text}",
                sub_args.join(" ")
            );
        }
    }

    // Completions still register `oplog` cleanly.
    let comp = run_oz_raw(&["completions", "bash"]);
    assert!(comp.status.success());
    assert!(String::from_utf8_lossy(&comp.stdout).contains("oplog"));
}
