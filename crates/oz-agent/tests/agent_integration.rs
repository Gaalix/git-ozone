// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end integration tests for `oz-agent` (`oz agent setup`, `refs/notes/ai` C-git
//! bidirectional interoperability, `oz blame`, and multi-workspace `oz mcp` + `oz radar`).

use oz_agent::{
    AGENTS_MD_END_MARKER, AGENTS_MD_START_MARKER, AgentHarness, AgentSetupOptions, BlameOptions,
    McpServer, OZ_CHECKPOINT_HOOK_CMD, ProvenanceOptions, blame_file, read_provenance_note,
    record_commit_provenance, setup_agent_harnesses,
};
use oz_core::{CommitOptions, OzoneRepo, commit_changes};
use serde_json::{Value, json};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn sys_git_bin() -> &'static str {
    if Path::new("/usr/bin/git").is_file() {
        "/usr/bin/git"
    } else {
        "git"
    }
}

fn init_git_repo(dir: &Path) -> OzoneRepo {
    fs::create_dir_all(dir).expect("mkdir repo");
    let status = Command::new(sys_git_bin())
        .args(["init", "-b", "main"])
        .current_dir(dir)
        .status()
        .expect("git init");
    assert!(status.success());

    for (k, v) in [
        ("user.name", "Integration Human"),
        ("user.email", "human@example.com"),
    ] {
        let s = Command::new(sys_git_bin())
            .args(["config", k, v])
            .current_dir(dir)
            .status()
            .expect("git config");
        assert!(s.success());
    }

    fs::create_dir_all(dir.join(".git/ozone")).expect("mkdir .git/ozone");
    fs::write(
        dir.join(".git/ozone/config.toml"),
        "backend = \"cas_hardlink\"\n",
    )
    .expect("write ozone config");

    OzoneRepo::discover(Some(dir)).expect("discover repo")
}

#[test]
fn bidirectional_c_git_notes_interop_and_revision_blame() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    let repo = init_git_repo(&repo_dir);

    // Commit 1: written and noted via `/usr/bin/git notes --ref=ai add`!
    fs::write(
        repo_dir.join("engine.rs"),
        "pub fn step_one() -> u32 {\n    10\n}\n",
    )
    .expect("write c1");
    let c1 = commit_changes(&repo, &CommitOptions::new("feat: step_one")).expect("commit c1");

    let c_git_note_json = r#"{"schema_version":1,"session_id":"c-git-sess","client":"codex-cli","model":"gpt-5-codex","timestamp":"2026-11-18T10:00:00Z","line_ranges":[{"path":"engine.rs","start_line":1,"end_line":3,"author_kind":"agent"}]}"#;
    let add_note = Command::new(sys_git_bin())
        .args([
            "notes",
            "--ref=ai",
            "add",
            "-m",
            c_git_note_json,
            &c1.commit_oid,
        ])
        .current_dir(&repo_dir)
        .status()
        .expect("git notes add");
    assert!(add_note.success());

    // `oz-agent` reads the note created by `git notes`!
    let read_from_c_git = read_provenance_note(&repo, &c1.commit_oid)
        .expect("read c1 note")
        .expect("note present");
    assert_eq!(read_from_c_git.session_id, "c-git-sess");
    assert_eq!(read_from_c_git.client, "codex-cli");

    // Commit 2: human commit (no note).
    let repo = OzoneRepo::discover(Some(&repo_dir)).expect("rediscover");
    fs::write(
        repo_dir.join("engine.rs"),
        "// Human module doc\npub fn step_one() -> u32 {\n    10\n}\n",
    )
    .expect("write c2");
    let c2 = commit_changes(&repo, &CommitOptions::new("docs: add human doc")).expect("commit c2");

    // Commit 3: oz-agent appends a note onto `refs/notes/ai` advancing from C git's notes commit!
    let repo = OzoneRepo::discover(Some(&repo_dir)).expect("rediscover");
    fs::write(
        repo_dir.join("engine.rs"),
        "// Human module doc\npub fn step_one() -> u32 {\n    20\n}\n",
    )
    .expect("write c3");
    let c3 = commit_changes(&repo, &CommitOptions::new("feat: tune step_one")).expect("commit c3");
    record_commit_provenance(
        &repo,
        &c3.commit_oid,
        &ProvenanceOptions::explicit("antigravity", "agy-900"),
    )
    .expect("record c3 provenance");

    // Both C1's C-git note and C3's oz-agent note must be readable via `git notes --ref=ai show`!
    for oid in [&c1.commit_oid, &c3.commit_oid] {
        let show = Command::new(sys_git_bin())
            .args(["notes", "--ref=ai", "show", oid])
            .current_dir(&repo_dir)
            .output()
            .expect("git notes show");
        assert!(show.status.success());
    }

    // Blame at HEAD:
    // Line 1: C2 (`// Human module doc`) -> human
    // Line 2: C1 (`pub fn step_one() -> u32 {`) -> agent (`codex-cli`)
    // Line 3: C3 (`    20`) -> agent (`antigravity`)
    // Line 4: C1 (`}`) -> agent (`codex-cli`)
    let repo = OzoneRepo::discover(Some(&repo_dir)).expect("rediscover");
    let head_blame = blame_file(&repo, "engine.rs", &BlameOptions::default()).expect("blame HEAD");
    assert_eq!(head_blame.total_lines, 4);
    assert_eq!(head_blame.human_lines, 1);
    assert_eq!(head_blame.agent_lines, 3);
    assert_eq!(head_blame.agent_percentage, 75.0);
    assert_eq!(head_blame.lines[0].commit_oid, c2.commit_oid);
    assert_eq!(head_blame.lines[0].author_kind, "human");
    assert_eq!(head_blame.lines[1].client.as_deref(), Some("codex-cli"));
    assert_eq!(head_blame.lines[2].client.as_deref(), Some("antigravity"));
    assert_eq!(head_blame.lines[3].client.as_deref(), Some("codex-cli"));

    // Historical blame at C2 revision still works accurately!
    let c2_blame = blame_file(
        &repo,
        "engine.rs",
        &BlameOptions::for_revision(&c2.commit_oid),
    )
    .expect("blame c2");
    assert_eq!(c2_blame.total_lines, 4);
    assert_eq!(c2_blame.lines[2].content, "    10");
    assert_eq!(c2_blame.lines[2].client.as_deref(), Some("codex-cli"));
}

#[test]
fn non_destructive_hook_and_marker_updates_across_all_harnesses() {
    let dir = TempDir::new().expect("tempdir");

    // Pre-populate existing hooks in Cursor, Windsurf, Antigravity, and an outdated block in AGENTS.md.
    fs::create_dir_all(dir.path().join(".cursor")).unwrap();
    fs::write(
        dir.path().join(".cursor/hooks.json"),
        r#"{"version":1,"hooks":{"afterFileEdit":[{"command":"prettier --write"}]}}"#,
    )
    .unwrap();

    fs::create_dir_all(dir.path().join(".windsurf")).unwrap();
    fs::write(
        dir.path().join(".windsurf/hooks.json"),
        r#"{"hooks":{"post_write_code":[{"command":"eslint --fix","show_output":true}]}}"#,
    )
    .unwrap();

    fs::create_dir_all(dir.path().join(".agent")).unwrap();
    fs::write(
        dir.path().join(".agent/hooks.json"),
        r#"{"customKey":42,"hooks":{"PostToolUse":[{"matcher":"run_command","hooks":[{"type":"command","command":"echo audit"}]}]}}"#,
    )
    .unwrap();

    let initial_agents = format!(
        "# Team Guide\n\nKeep commits small.\n\n{AGENTS_MD_START_MARKER}\nold stale ozone block\n{AGENTS_MD_END_MARKER}\n\n## Footer\nNever push secrets.\n"
    );
    fs::write(dir.path().join("AGENTS.md"), initial_agents).unwrap();

    let report =
        setup_agent_harnesses(dir.path(), &AgentSetupOptions::all_harnesses()).expect("setup all");
    assert!(
        report
            .updated
            .contains(&PathBuf::from(".cursor/hooks.json"))
    );
    assert!(
        report
            .updated
            .contains(&PathBuf::from(".windsurf/hooks.json"))
    );
    assert!(report.updated.contains(&PathBuf::from(".agent/hooks.json")));
    assert!(report.updated.contains(&PathBuf::from("AGENTS.md")));
    assert_eq!(report.harnesses.len(), 5);
    for h in AgentHarness::ALL {
        let parsed: AgentHarness = h.as_str().parse().expect("FromStr roundtrip");
        assert_eq!(parsed, h);
    }

    let cursor_hooks: Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join(".cursor/hooks.json")).unwrap())
            .unwrap();
    assert_eq!(
        cursor_hooks["hooks"]["afterFileEdit"][0]["command"],
        "prettier --write"
    );
    assert_eq!(
        cursor_hooks["hooks"]["afterFileEdit"][1]["command"],
        OZ_CHECKPOINT_HOOK_CMD
    );

    let agy_hooks: Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join(".agent/hooks.json")).unwrap())
            .unwrap();
    assert_eq!(agy_hooks["customKey"], 42);
    assert_eq!(
        agy_hooks["hooks"]["PostToolUse"].as_array().unwrap().len(),
        2
    );

    let updated_agents = fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
    assert!(updated_agents.starts_with("# Team Guide\n\nKeep commits small.\n\n"));
    assert!(updated_agents.ends_with("\n\n## Footer\nNever push secrets.\n"));
    assert!(!updated_agents.contains("old stale ozone block"));
    assert!(updated_agents.contains("oz commit"));
}

#[test]
fn progressive_disclosure_separates_compact_rules_from_comprehensive_skills() {
    let dir = TempDir::new().expect("tempdir");
    setup_agent_harnesses(dir.path(), &AgentSetupOptions::all_harnesses()).expect("setup all");

    let agents_md = fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
    let cursor_rule = fs::read_to_string(dir.path().join(".cursor/rules/oz.mdc")).unwrap();
    let windsurf_rule = fs::read_to_string(dir.path().join(".windsurf/rules/oz.md")).unwrap();
    let claude_skill = fs::read_to_string(dir.path().join(".claude/skills/oz/SKILL.md")).unwrap();
    let agy_skill = fs::read_to_string(dir.path().join(".agent/skills/oz/SKILL.md")).unwrap();
    let codex_skill = fs::read_to_string(dir.path().join(".codex/skills/oz/SKILL.md")).unwrap();

    // All on-demand SKILL.md files share the same comprehensive workflow template.
    assert_eq!(claude_skill, agy_skill);
    assert_eq!(claude_skill, codex_skill);

    // Always-on rules must stay significantly more compact than the on-demand SKILL.md.
    assert!(
        agents_md.len() * 2 < claude_skill.len(),
        "expected always-on AGENTS.md ({} bytes) to be < half of SKILL.md ({} bytes)",
        agents_md.len(),
        claude_skill.len()
    );
    assert!(cursor_rule.len() * 2 < claude_skill.len());
    assert!(windsurf_rule.len() * 2 < claude_skill.len());

    // Always-on rules carry key behavioral differences, concise command map, and pointer to SKILL.md.
    for (name, rule) in [
        ("AGENTS.md", &agents_md),
        (".cursor/rules/oz.mdc", &cursor_rule),
        (".windsurf/rules/oz.md", &windsurf_rule),
    ] {
        for required in [
            "oz -C <dir>",
            "untracked files automatically",
            "refs/notes/ai",
            "Rust/ERE regex",
            "oz oplog",
            "oz undo --list",
            "oz git <cmd>",
            "--agent",
            "OZ_AGENT=1",
            "SKILL.md",
            "oz <cmd> --help",
        ] {
            assert!(
                rule.contains(required),
                "expected {name} rule to contain {required:?}"
            );
        }
    }

    // On-demand SKILL.md covers all rich multi-step workflows and capabilities.
    for required in [
        "oz commit --amend",
        "--no-edit",
        "Change-Id:",
        "--sign",
        "--force",
        "--allow-out-of-scope",
        "oz git <cmd>",
        "../<repo>.oz/_integrate",
        "oz resolve c1 --take ours|theirs|base",
        "oz resolve --all --take ours --continue",
        "oz integrate --continue",
        "oz integrate --abort",
        "../<repo>.oz/<name>",
        "oz ws spawn",
        "oz ws list",
        "oz ws rm",
        "--delete-branch",
        "oz ws gc",
        "oz stack status",
        "oz stack rebase",
        "oz pr stack",
        "oz sync",
        "--push",
        "--no-rebase",
        "--no-gc",
        "--no-warm",
        "retargets downstream stacked PR",
        "oz doctor",
        "oz fsck [--repair]",
        "oz gc",
        "oz blame <path>",
        "oz oplog",
        "oz undo --list",
        "oz undo <OP_ID>",
        "oz redo",
        "--agent",
        "OZ_AGENT=1",
        "\"schema\": \"oz.v1\"",
        "\"recovery_commands\"",
        "\"conflicts\"",
    ] {
        assert!(
            claude_skill.contains(required),
            "expected SKILL.md to contain {required:?}"
        );
    }
}

#[test]
fn mcp_multi_workspace_radar_collision_and_partial_commit_blame_e2e() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_dir = tmp.path().join("monorepo");
    let repo = init_git_repo(&repo_dir);

    fs::create_dir_all(repo_dir.join("src")).unwrap();
    fs::write(
        repo_dir.join("src/service.rs"),
        "pub fn alpha() -> i32 {\n    1\n}\n\n\n\n\n\n\npub fn beta() -> i32 {\n    2\n}\n",
    )
    .unwrap();
    commit_changes(&repo, &CommitOptions::new("chore: base service")).unwrap();

    let mut server = McpServer::new(Some(repo_dir.clone())).with_session_id("e2e-mcp-sess");

    let spawn_reqs = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","clientInfo":{"name":"cursor"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"oz_ws_spawn","arguments":{"name":"ws-alpha","from":"main","warm":false}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"oz_ws_spawn","arguments":{"name":"ws-beta","from":"main","warm":false}}}),
    ];
    let mut wire = String::new();
    for r in &spawn_reqs {
        wire.push_str(&serde_json::to_string(r).unwrap());
        wire.push('\n');
    }
    let mut out = Vec::new();
    server
        .serve_stdio(Cursor::new(wire.into_bytes()), &mut out)
        .unwrap();
    let lines: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let alpha_info: Value =
        serde_json::from_str(lines[1]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let beta_info: Value =
        serde_json::from_str(lines[2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();

    let alpha_path = PathBuf::from(alpha_info["worktree_path"].as_str().unwrap());
    let beta_path = PathBuf::from(beta_info["worktree_path"].as_str().unwrap());

    // Edit overlapping lines in `src/service.rs` across both spawned workspaces!
    fs::write(
        alpha_path.join("src/service.rs"),
        "pub fn alpha() -> i32 {\n    100\n}\n\n\n\n\n\n\npub fn beta() -> i32 {\n    200\n}\n",
    )
    .unwrap();
    fs::write(
        beta_path.join("src/service.rs"),
        "pub fn alpha() -> i32 {\n    999\n}\n\n\n\n\n\n\npub fn beta() -> i32 {\n    2\n}\n",
    )
    .unwrap();

    // Query `oz_radar` and commit `h1a` in `ws-alpha` via MCP!
    let next_reqs = [
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"oz_radar","arguments":{}}}),
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"oz_commit","arguments":{"workspace":"ws-alpha","message":"feat(alpha): set 100","pick":["h1a"]}}}),
    ];
    let mut wire2 = String::new();
    for r in &next_reqs {
        wire2.push_str(&serde_json::to_string(r).unwrap());
        wire2.push('\n');
    }
    let mut out2 = Vec::new();
    server
        .serve_stdio(Cursor::new(wire2.into_bytes()), &mut out2)
        .unwrap();
    let lines2: Vec<Value> = String::from_utf8(out2)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let radar_res: Value =
        serde_json::from_str(lines2[0]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(radar_res["collisions"][0]["path"], "src/service.rs");
    assert_eq!(radar_res["collisions"][0]["severity"], "overlapping_hunks");

    let commit_res: Value =
        serde_json::from_str(lines2[1]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(commit_res["ok"], true);
    assert_eq!(commit_res["remaining_dirty_count"], 1);

    // Run `oz blame` on `ws-alpha`: line 2 (`100`) is attributed to `cursor` (`e2e-mcp-sess`),
    // while `beta()` (`2` in committed HEAD of `ws-alpha`) remains `human`!
    let alpha_repo = OzoneRepo::discover(Some(&alpha_path)).unwrap();
    let blame = blame_file(&alpha_repo, "src/service.rs", &BlameOptions::default()).unwrap();
    assert_eq!(blame.agent_lines, 1);
    assert_eq!(blame.lines[1].content, "    100");
    assert_eq!(blame.lines[1].author_kind, "agent");
    assert_eq!(blame.lines[1].client.as_deref(), Some("cursor"));
    assert_eq!(blame.lines[1].session_id.as_deref(), Some("e2e-mcp-sess"));
    assert_eq!(blame.lines[10].content, "    2");
    assert_eq!(blame.lines[10].author_kind, "human");
}
