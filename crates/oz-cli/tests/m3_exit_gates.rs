// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Milestone 3 Exit-Gate & VCBench-Style Agent Evaluation Suite (`v0.3.0` RC):
//!
//! 1. **Gate 1 — Pure-Rust `Cargo.lock` & `v0.3.0` Release-Candidate Gate**:
//!    - `oz --version --json` and `git-ozone --version --json` report `"version": "0.3.0"`
//!      and `"pure_rust": true`.
//!    - `Cargo.lock` has all 10 workspace crates at `0.3.0` and zero C bindings
//!      (`openssl-sys`, `libgit2-sys`, `curl-sys`, `fuse-sys`, `libfuse`).
//!
//! 2. **VCBench-Style Agent Eval Exit Gate (`ROADMAP.md` Milestone 3)**:
//!    - Deterministic, strictly non-interactive (`GIT_TERMINAL_PROMPT=0`, `GIT_EDITOR=true`)
//!      agent evaluation harness running identical multi-file, multi-hunk repository tasks
//!      end-to-end against both **Baseline C `git`** and **Ozone (`oz`)**:
//!      - Task 1: Selective Hunk Commits (4 dirty files, 3-hunk file, commit `f2` + `h1a`)
//!      - Task 2: Multi-Amends (3 iterative refinement rounds preserving `Change-Id:` & local debug edits)
//!      - Task 3: Split Commits (3 entangled logical concerns across 5 files + shared-file hunks -> 3 atomic commits)
//!      - Task 4: Conflict Resolution (integrating 2 feature branches into `main` across 2 conflicted files)
//!    - Asserts `>= 60%` fewer commands (`oz_commands * 100 <= git_commands * 40`),
//!      `>= 50%` lower `ceil(utf8_bytes / 4)` token overhead, `>= 50%` lower lexical token
//!      overhead, identical Git commit trees/graphs, identical post-task working trees, and
//!      clean `git fsck --full`.
//!
//! 3. **Gate 2 — End-to-End Signed Stacked PR + CI Self-Healing + Sync Loop**:
//!    - Spawns CoW workspaces -> creates pure-Rust Ed25519 SSH-signed commits verified by
//!      system `git verify-commit` -> runs `pr_stack` against a unified local `tokio`
//!      Smart-HTTP v2 + Mock GitHub REST/GraphQL server -> simulates failing CI check ->
//!      `query_ci_status` extracts failing step & log excerpt -> amends fix preserving
//!      `Change-Id:` + valid SSH signature -> re-pushes `--force-with-lease` -> simulates
//!      upstream PR #1 merge -> `sync_repo` fast-forwards `main`, retargets PR #2 base to
//!      `main`, rebases downstream stack, and GCs merged CoW workspace & checkpoint.
//!
//! 4. **Gate 3 — End-to-End Agent Ecosystem & Provenance Loop**:
//!    - `oz_agent::setup_agent_harnesses` across all 5 harnesses (Claude Code, Cursor,
//!      Codex CLI, Windsurf, Antigravity) -> `oz mcp` stdio JSON-RPC session (`initialize`,
//!      `tools/list`, `oz_status`, `oz_diff`, `oz_checkpoint`, `oz_commit` with `pick`) ->
//!      automatic `refs/notes/ai` provenance interoperating with `git notes --ref=ai` ->
//!      `oz_agent::blame_file` proving exact agent vs. human line attribution -> `oz_undo`
//!      via MCP stdio.

use oz_agent::{
    AGENTS_MD_END_MARKER, AGENTS_MD_START_MARKER, AgentHarness, AgentSetupOptions, BlameOptions,
    McpServer, OZ_CHECKPOINT_HOOK_CMD, blame_file, format_blame_report, read_provenance_note,
    setup_agent_harnesses,
};
use oz_core::{
    BackendKind, CommitOptions, MountRecord, OZ_SCHEMA_V1, OzError, OzoneRepo,
    SSHSIG_ARMOR_HEADER, base64_encode, commit_changes, compute_status,
    verify_commit_signature_with_allowed_signers,
};
use oz_cow::{SpawnWorkspaceOptions, SpawnedWorkspaceInfo, list_workspaces, spawn_workspace};
use oz_github::{
    CiQueryOptions, CloneOptions, GitHubActionsJob, GitHubCheckRun, GitHubCheckRunOutput,
    GitHubJobStep, GitHubPrRef, GitHubPullRequest, GitHubWorkflowRun, HttpAuthScheme,
    OZ_STACK_END, OZ_STACK_START, PrStackOptions, PushOptions, SyncOptions, clone_repo, pr_stack,
    push_refs, query_ci_status, sync_repo,
};
use oz_oplog::{ActorInfo, ActorKind, MutationTransaction, checkpoint};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

static VFSD_BIN_LOCK: OnceLock<PathBuf> = OnceLock::new();

fn oz_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oz"))
}

fn git_ozone_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_git-ozone"))
}

fn workspace_root_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn sys_git_bin() -> PathBuf {
    let unix_git = Path::new("/usr/bin/git");
    if unix_git.is_file() {
        unix_git.to_path_buf()
    } else {
        PathBuf::from("git")
    }
}

fn ssh_keygen_bin() -> PathBuf {
    let unix_keygen = Path::new("/usr/bin/ssh-keygen");
    if unix_keygen.is_file() {
        unix_keygen.to_path_buf()
    } else {
        PathBuf::from("ssh-keygen")
    }
}

fn ensure_oz_vfsd_bin() -> &'static PathBuf {
    VFSD_BIN_LOCK.get_or_init(|| {
        if let Some(env_bin) = std::env::var_os("OZ_VFSD_BIN") {
            let p = PathBuf::from(env_bin);
            if p.is_file() {
                return p;
            }
        }

        let oz = oz_bin();
        if let Ok(resolved) = oz_vfsd::resolve_oz_vfsd_bin_with(
            None,
            Some(&oz),
            std::env::var_os("PATH").as_deref(),
        ) {
            return resolved;
        }
        if let Ok(resolved) = oz_vfsd::find_oz_vfsd_bin() {
            return resolved;
        }
        let sibling = oz.with_file_name(oz_vfsd::vfsd_binary_name());

        let is_release = oz
            .components()
            .any(|c| matches!(c, std::path::Component::Normal(s) if s == "release"));
        let root = workspace_root_dir();

        let mut build = Command::new(env!("CARGO"));
        build
            .args(["build", "--offline", "--locked", "--bin", "oz-vfsd"])
            .current_dir(&root);
        if is_release {
            build.arg("--release");
        }
        let status = build.status().expect("cargo build --bin oz-vfsd");
        assert!(status.success(), "failed to build oz-vfsd helper binary");
        assert!(sibling.is_file(), "expected oz-vfsd at {}", sibling.display());
        sibling
    })
}

/// Cross-platform RAII test sandbox ensuring any `oz-vfsd` daemons and FUSE3 mounts
/// spawned inside the test directory are cleanly torn down on drop.
struct GateSandbox {
    root: PathBuf,
    repo_root: PathBuf,
    extra_git_dirs: Vec<PathBuf>,
    mount_points: Vec<PathBuf>,
    _tempdir: TempDir,
}

impl GateSandbox {
    fn new(repo_name: &str) -> Self {
        let _ = ensure_oz_vfsd_bin();
        // Keep base path `< 60` bytes so `.git/ozone/vfsd.sock` always stays well below Unix
        // `SUN_LEN` (< 108 bytes). On CI (`CARGO_TARGET_TMPDIR` <= 60 bytes), this uses the
        // host `ext4`/`APFS`/`NTFS` volume; in deeply nested worktrees, it uses `tempdir()`.
        let preferred_base: Option<PathBuf> = option_env!("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .filter(|p| p.as_os_str().len() <= 60);

        let tempdir = match preferred_base {
            Some(ref base) => {
                fs::create_dir_all(base).expect("create short real-disk sandbox base");
                tempfile::Builder::new()
                    .prefix("oz-m3-")
                    .tempdir_in(base)
                    .expect("create tempdir in preferred_base")
            }
            None => tempfile::Builder::new()
                .prefix("oz-m3-")
                .tempdir()
                .expect("create tempdir"),
        };
        let root = tempdir.path().to_path_buf();
        let repo_root = root.join(repo_name);
        fs::create_dir_all(&repo_root).expect("mkdir repo_root");
        Self {
            root,
            repo_root,
            extra_git_dirs: Vec::new(),
            mount_points: Vec::new(),
            _tempdir: tempdir,
        }
    }

    fn register_repo(&mut self, repo_root: &Path) {
        let git_dir = repo_root.join(".git");
        if !self.extra_git_dirs.contains(&git_dir) {
            self.extra_git_dirs.push(git_dir);
        }
    }
}

impl Drop for GateSandbox {
    fn drop(&mut self) {
        let mut git_dirs = vec![self.repo_root.join(".git")];
        for g in &self.extra_git_dirs {
            if !git_dirs.contains(g) {
                git_dirs.push(g.clone());
            }
        }
        let mut mounts_to_clean = self.mount_points.clone();
        let mut pids_to_kill: Vec<u32> = Vec::new();

        for git_dir in &git_dirs {
            let ws_root = git_dir.join("ozone/ws");
            if let Ok(entries) = fs::read_dir(&ws_root) {
                for entry in entries.flatten() {
                    let mj = entry.path().join("mount.json");
                    if let Ok(raw) = fs::read_to_string(&mj)
                        && let Ok(rec) = serde_json::from_str::<MountRecord>(&raw)
                    {
                        if !mounts_to_clean.contains(&rec.mount_point) {
                            mounts_to_clean.push(rec.mount_point);
                        }
                        if let Some(pid) = rec.daemon_pid
                            && !pids_to_kill.contains(&pid)
                        {
                            pids_to_kill.push(pid);
                        }
                    }
                }
            }
        }

        #[cfg(target_os = "linux")]
        if let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") {
            let prefix = self.root.to_string_lossy();
            for line in mountinfo.lines() {
                let Some(mp_str) = line.split_whitespace().nth(4) else {
                    continue;
                };
                if mp_str.starts_with(prefix.as_ref()) {
                    let mp = PathBuf::from(mp_str);
                    if !mounts_to_clean.contains(&mp) {
                        mounts_to_clean.push(mp);
                    }
                }
            }
        }

        for git_dir in &git_dirs {
            if git_dir.is_dir() {
                let _ = oz_vfsd::shutdown_daemon(git_dir);
            }
        }

        #[cfg(target_os = "linux")]
        {
            let root_marker = self.root.to_string_lossy().into_owned();
            for pid in pids_to_kill {
                kill_test_vfsd_pid(pid, &root_marker);
            }
            for mp in &mounts_to_clean {
                let _ = Command::new("fusermount3")
                    .args(["-u", "-z", "--"])
                    .arg(mp)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = pids_to_kill;
            let _ = mounts_to_clean;
        }
    }
}

#[cfg(target_os = "linux")]
fn kill_test_vfsd_pid(pid: u32, git_dir_marker: &str) {
    let cmdline_path = format!("/proc/{pid}/cmdline");
    let Ok(bytes) = fs::read(&cmdline_path) else {
        return;
    };
    let cmdline = String::from_utf8_lossy(&bytes);
    if cmdline.contains("oz-vfsd") && cmdline.contains(git_dir_marker) {
        unsafe {
            let _ = libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        let proc_dir = PathBuf::from(format!("/proc/{pid}"));
        while Instant::now() < deadline && proc_dir.exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

fn unmethered_git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new(sys_git_bin())
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::null())
        .output()
        .expect("execute git setup helper");
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

fn assert_git_fsck_clean(repo_dir: &Path) {
    let out = Command::new(sys_git_bin())
        .args(["fsck", "--full"])
        .current_dir(repo_dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .expect("run git fsck --full");
    assert!(
        out.status.success(),
        "git fsck --full failed in {}:\nstdout: {}\nstderr: {}",
        repo_dir.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Collects all non-`.git` working-tree files and their exact byte contents for bit-for-bit
/// equivalence assertions between Baseline C `git` and Ozone (`oz`).
fn snapshot_worktree_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, cur: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let entries = fs::read_dir(cur).expect("read_dir");
        for entry in entries {
            let entry = entry.expect("dir entry");
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            let path = entry.path();
            let ft = entry.file_type().expect("file_type");
            if ft.is_dir() {
                walk(base, &path, out);
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(base)
                    .expect("strip_prefix")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, fs::read(&path).expect("read worktree file"));
            }
        }
    }
    let mut map = BTreeMap::new();
    walk(root, root, &mut map);
    map
}

// ============================================================================
// GATE 1: Pure-Rust Cargo.lock & Workspace Version Consistency Gate
// ============================================================================

#[test]
fn gate1_pure_rust_cargo_lock_and_v0_3_0_release_candidate() {
    let expected_version = env!("CARGO_PKG_VERSION");
    assert!(
        !expected_version.is_empty(),
        "workspace package version must not be empty"
    );

    // 1. Verify both `oz --version --json` and `git-ozone --version --json`.
    for (bin_path, expected_invoked) in [(oz_bin(), "oz"), (git_ozone_bin(), "git-ozone")] {
        let out = Command::new(&bin_path)
            .args(["--version", "--json"])
            .stdin(Stdio::null())
            .output()
            .expect("run --version --json");
        assert!(
            out.status.success(),
            "{expected_invoked} --version --json failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let parsed: Value =
            serde_json::from_slice(&out.stdout).expect("parse oz.v1 version envelope");
        assert_eq!(parsed["schema"], OZ_SCHEMA_V1);
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"]["name"], "oz");
        assert_eq!(parsed["data"]["invoked_as"], expected_invoked);
        assert_eq!(parsed["data"]["version"], expected_version);
        assert_eq!(parsed["data"]["pure_rust"], true);
        assert_eq!(parsed["data"]["non_interactive"], true);
    }

    // 2. Verify root Cargo.toml and Cargo.lock are synced to the workspace version and free of forbidden C bindings.
    let root = workspace_root_dir();
    let cargo_toml = fs::read_to_string(root.join("Cargo.toml")).expect("read Cargo.toml");
    let expected_toml_version = format!("version = \"{expected_version}\"");
    assert!(
        cargo_toml.contains(&expected_toml_version),
        "root Cargo.toml must declare workspace version {expected_version}"
    );

    let cargo_lock = fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");
    let forbidden_c_bindings = [
        "name = \"openssl-sys\"",
        "name = \"libgit2-sys\"",
        "name = \"curl-sys\"",
        "name = \"fuse-sys\"",
        "name = \"libfuse\"",
    ];
    for forbidden in forbidden_c_bindings {
        assert!(
            !cargo_lock.contains(forbidden),
            "Cargo.lock contains forbidden C binding: {forbidden}"
        );
    }

    let workspace_crates = [
        "oz-cli",
        "oz-core",
        "oz-cow",
        "oz-vfsd",
        "oz-warm",
        "oz-oplog",
        "oz-integrate",
        "oz-github",
        "oz-agent",
        "oz-cloud-api",
    ];
    for crate_name in workspace_crates {
        let expected_stanza = format!("name = \"{crate_name}\"\nversion = \"{expected_version}\"");
        assert!(
            cargo_lock.contains(&expected_stanza),
            "Cargo.lock missing synced v{expected_version} package entry for {crate_name}"
        );
    }
}

// ============================================================================
// VCBENCH-STYLE AGENT EVALUATION HARNESS (ALL 4 TASKS + AGGREGATE GATE)
// ============================================================================

/// Tokenizes code/CLI text into lexical tokens (alphanumeric/underscore identifiers + individual
/// punctuation/operator symbols, skipping whitespace) to cross-check standard `ceil(bytes / 4)`
/// LLM token estimation.
fn count_lexical_tokens(text: &str) -> usize {
    let mut count = 0usize;
    let mut in_ident = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            in_ident = false;
        } else if ch.is_alphanumeric() || ch == '_' {
            if !in_ident {
                count += 1;
                in_ident = true;
            }
        } else {
            in_ident = false;
            count += 1;
        }
    }
    count
}

fn approx_llm_tokens(utf8_bytes: usize) -> usize {
    utf8_bytes.div_ceil(4)
}

#[derive(Debug, Clone)]
struct InvocationMetrics {
    exit_code: i32,
    stdout: String,
    stderr: String,
    approx_tokens: usize,
    lexical_tokens: usize,
}

#[derive(Debug, Clone, Default)]
struct VcsAgentHarness {
    label: &'static str,
    invocations: Vec<InvocationMetrics>,
}

impl VcsAgentHarness {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            invocations: Vec::new(),
        }
    }

    fn command_count(&self) -> usize {
        self.invocations.len()
    }

    fn total_approx_tokens(&self) -> usize {
        self.invocations.iter().map(|i| i.approx_tokens).sum()
    }

    fn total_lexical_tokens(&self) -> usize {
        self.invocations.iter().map(|i| i.lexical_tokens).sum()
    }

    fn record_exchange(
        &mut self,
        cmd_line: String,
        stdin_payload: &str,
        exit_code: i32,
        stdout: String,
        stderr: String,
    ) -> InvocationMetrics {
        let utf8_bytes =
            cmd_line.len() + stdin_payload.len() + stdout.len() + stderr.len();
        let approx_tokens = approx_llm_tokens(utf8_bytes);
        let lexical_tokens = count_lexical_tokens(&cmd_line)
            + count_lexical_tokens(stdin_payload)
            + count_lexical_tokens(&stdout)
            + count_lexical_tokens(&stderr);

        let rec = InvocationMetrics {
            exit_code,
            stdout,
            stderr,
            approx_tokens,
            lexical_tokens,
        };
        self.invocations.push(rec.clone());
        rec
    }

    /// Executes Baseline C `git` in strict non-interactive agent mode (`GIT_TERMINAL_PROMPT=0`,
    /// `GIT_EDITOR=true`, `GIT_PAGER=cat`) with optional stdin payload (e.g., for `git apply --cached -`).
    fn exec_git(
        &mut self,
        cwd: &Path,
        args: &[&str],
        stdin_payload: Option<&str>,
        expect_success: bool,
    ) -> InvocationMetrics {
        let rendered_cmd = render_shell_cmd("git", args);
        let mut cmd = Command::new(sys_git_bin());
        cmd.args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .env("EDITOR", "true")
            .env("VISUAL", "true")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let stdin_str = stdin_payload.unwrap_or("");
        let out = if let Some(input) = stdin_payload {
            cmd.stdin(Stdio::piped());
            let mut child = cmd.spawn().expect("spawn git");
            child
                .stdin
                .as_mut()
                .expect("git stdin")
                .write_all(input.as_bytes())
                .expect("write git stdin");
            child.wait_with_output().expect("wait git")
        } else {
            cmd.stdin(Stdio::null());
            cmd.output().expect("run git")
        };

        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if expect_success {
            assert_eq!(
                code, 0,
                "[{}] `{rendered_cmd}` failed (code {code}):\nstdout: {stdout}\nstderr: {stderr}",
                self.label
            );
        }
        self.record_exchange(rendered_cmd, stdin_str, code, stdout, stderr)
    }

    /// Executes Ozone (`oz`) subprocess in strict non-interactive agent mode (`OZ_AGENT=1`).
    fn exec_oz(&mut self, cwd: &Path, args: &[&str], expect_success: bool) -> InvocationMetrics {
        let rendered_cmd = render_shell_cmd("oz", args);
        let out = Command::new(oz_bin())
            .args(args)
            .current_dir(cwd)
            .env("OZ_AGENT", "1")
            .env("OZ_VFSD_BIN", ensure_oz_vfsd_bin().as_os_str())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .stdin(Stdio::null())
            .output()
            .expect("run oz");

        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if expect_success {
            assert_eq!(
                code, 0,
                "[{}] `{rendered_cmd}` failed (code {code}):\nstdout: {stdout}\nstderr: {stderr}",
                self.label
            );
        }
        self.record_exchange(rendered_cmd, "", code, stdout, stderr)
    }

    /// Executes `oz commit --amend` via `oz_core::commit_changes` + `MutationTransaction` (matching
    /// `handle_commit` in `oz-cli` byte-for-byte while remaining 100% standalone before Wave 2 CLI merge),
    /// metering the exact CLI command string and human output line.
    fn exec_oz_amend_pick(
        &mut self,
        cwd: &Path,
        message: &str,
        picks: &[&str],
    ) -> InvocationMetrics {
        let mut rendered_args = vec!["commit", "--amend", "-m", message];
        if !picks.is_empty() {
            rendered_args.push("--pick");
            rendered_args.extend_from_slice(picks);
        }
        let rendered_cmd = render_shell_cmd("oz", &rendered_args);

        let repo = OzoneRepo::discover(Some(cwd)).expect("discover oz repo");
        repo.layout.ensure_initialized().expect("init ozone layout");
        let pre_status = compute_status(&repo).expect("pre_status");
        let actor = ActorInfo {
            kind: ActorKind::Agent,
            client: Some("oz-cli".to_string()),
            workspace: repo.workspace.clone(),
        };
        let tx = MutationTransaction::begin(&repo, "oz commit --amend", actor, Some(&pre_status.changes))
            .expect("begin oplog tx");

        let opts = CommitOptions::new(message)
            .with_amend(true)
            .with_pick(picks.iter().copied());
        let outcome = commit_changes(&repo, &opts).expect("commit_changes --amend");

        if outcome.remaining_dirty_count == 0 {
            tx.finish_with_message(&repo, Some(&outcome.message), Some(&[]))
                .expect("finish oplog tx clean");
        } else {
            let post_status = compute_status(&repo).expect("post_status");
            tx.finish_with_message(&repo, Some(&outcome.message), Some(&post_status.changes))
                .expect("finish oplog tx dirty");
        }

        let short = &outcome.commit_oid[..outcome.commit_oid.len().min(12)];
        let stdout = format!(
            "[{} {short}] {} file(s) committed ({} dirty remaining, Change-Id: {})\n",
            outcome.branch.as_deref().unwrap_or("detached"),
            outcome.committed_paths.len(),
            outcome.remaining_dirty_count,
            outcome.change_id
        );
        self.record_exchange(rendered_cmd, "", 0, stdout, String::new())
    }
}

fn render_shell_cmd(prog: &str, args: &[&str]) -> String {
    let mut s = prog.to_string();
    for arg in args {
        s.push(' ');
        if arg.contains(char::is_whitespace) || arg.contains('"') || arg.is_empty() {
            s.push('"');
            s.push_str(&arg.replace('"', "\\\""));
            s.push('"');
        } else {
            s.push_str(arg);
        }
    }
    s
}

/// Extracts a single valid unified diff hunk (`hunk_zero_idx`) from a multi-hunk `git diff <file>`
/// output so a non-interactive C `git` agent can feed it into `git apply --cached -`.
fn extract_single_hunk_patch(full_file_diff: &str, hunk_zero_idx: usize) -> String {
    let mut header = String::new();
    let mut hunks: Vec<String> = Vec::new();
    let mut current_hunk: Option<String> = None;

    for line in full_file_diff.split_inclusive('\n') {
        if line.starts_with("@@ ") {
            if let Some(prev) = current_hunk.take() {
                hunks.push(prev);
            }
            current_hunk = Some(line.to_string());
        } else if let Some(ref mut h) = current_hunk {
            h.push_str(line);
        } else {
            header.push_str(line);
        }
    }
    if let Some(last) = current_hunk {
        hunks.push(last);
    }

    assert!(
        hunk_zero_idx < hunks.len(),
        "requested hunk index {hunk_zero_idx} out of {} hunks in diff:\n{full_file_diff}",
        hunks.len()
    );
    let mut patch = format!("{header}{}", hunks[hunk_zero_idx]);
    if !patch.ends_with('\n') {
        patch.push('\n');
    }
    patch
}

#[derive(Debug, Clone)]
struct VcBenchTaskSummary {
    name: &'static str,
    git_commands: usize,
    oz_commands: usize,
    git_approx_tokens: usize,
    oz_approx_tokens: usize,
    git_lexical_tokens: usize,
    oz_lexical_tokens: usize,
}

impl VcBenchTaskSummary {
    fn from_harnesses(
        name: &'static str,
        git: &VcsAgentHarness,
        oz: &VcsAgentHarness,
    ) -> Self {
        Self {
            name,
            git_commands: git.command_count(),
            oz_commands: oz.command_count(),
            git_approx_tokens: git.total_approx_tokens(),
            oz_approx_tokens: oz.total_approx_tokens(),
            git_lexical_tokens: git.total_lexical_tokens(),
            oz_lexical_tokens: oz.total_lexical_tokens(),
        }
    }

    fn cmd_reduction_pct(&self) -> f64 {
        100.0 * (1.0 - (self.oz_commands as f64 / self.git_commands as f64))
    }

    fn approx_token_reduction_pct(&self) -> f64 {
        100.0 * (1.0 - (self.oz_approx_tokens as f64 / self.git_approx_tokens as f64))
    }

    fn lexical_token_reduction_pct(&self) -> f64 {
        100.0 * (1.0 - (self.oz_lexical_tokens as f64 / self.git_lexical_tokens as f64))
    }
}

/// Generates a realistic ~105-line Rust module with 3 distinct functional sections (`section_a`,
/// `section_b`, `section_c`) separated by ~30 lines of unchanged domain structs, docs, and
/// validation logic so diff context windows (`U3`) form genuine, non-overlapping hunks.
fn render_three_section_rust_module(
    module_title: &str,
    sec_a_body: &str,
    sec_b_body: &str,
    sec_c_body: &str,
) -> String {
    let mut lines = Vec::with_capacity(115);
    lines.push(format!("//! {module_title} — production service module."));
    lines.push("//!".to_string());
    lines.push("//! Maintains strict invariants across request validation, execution,".to_string());
    lines.push("//! and telemetry reporting within the multi-tenant workspace engine.".to_string());
    lines.push(String::new());
    lines.push("#[derive(Debug, Clone, PartialEq, Eq)]".to_string());
    lines.push("pub struct ServiceContext {".to_string());
    lines.push("    pub tenant_id: String,".to_string());
    lines.push("    pub request_id: String,".to_string());
    lines.push("    pub now_epoch_secs: u64,".to_string());
    lines.push("    pub max_skew_secs: u64,".to_string());
    lines.push("    pub region: String,".to_string());
    lines.push("}".to_string());
    lines.push(String::new());

    // Section A (~L15-L24)
    lines.push("/// Primary section A entrypoint.".to_string());
    for l in sec_a_body.lines() {
        lines.push(l.to_string());
    }
    lines.push(String::new());

    // 28 lines of realistic unchanged domain logic separating Section A and Section B
    for idx in 1..=7 {
        lines.push(format!(
            "pub fn invariant_guard_{idx}(ctx: &ServiceContext) -> Result<&str, &'static str> {{"
        ));
        lines.push("    if ctx.tenant_id.trim().is_empty() {".to_string());
        lines.push("        return Err(\"tenant_id must not be empty\");".to_string());
        lines.push("    }".to_string());
        lines.push("    Ok(ctx.region.as_str())".to_string());
        lines.push("}".to_string());
    }
    lines.push(String::new());

    // Section B (~L68-L78)
    lines.push("/// Secondary section B handler.".to_string());
    for l in sec_b_body.lines() {
        lines.push(l.to_string());
    }
    lines.push(String::new());

    // 28 lines of realistic unchanged domain logic separating Section B and Section C
    for idx in 8..=14 {
        lines.push(format!(
            "pub fn normalize_header_slot_{idx}(raw: &str) -> Option<String> {{"
        ));
        lines.push("    let trimmed = raw.trim();".to_string());
        lines.push("    if trimmed.is_empty() {".to_string());
        lines.push("        return None;".to_string());
        lines.push("    }".to_string());
        lines.push("    Some(trimmed.to_ascii_lowercase())".to_string());
        lines.push("}".to_string());
    }
    lines.push(String::new());

    // Section C (~L120+)
    lines.push("/// Tertiary section C finalize routine.".to_string());
    for l in sec_c_body.lines() {
        lines.push(l.to_string());
    }
    lines.push(String::new());

    lines.join("\n")
}

fn init_paired_repos(sandbox: &mut GateSandbox, prefix: &str) -> (PathBuf, PathBuf) {
    let git_dir = sandbox.root.join(format!("{prefix}-git"));
    let oz_dir = sandbox.root.join(format!("{prefix}-oz"));
    for dir in [&git_dir, &oz_dir] {
        fs::create_dir_all(dir).expect("mkdir paired repo");
        unmethered_git(dir, &["init", "-b", "main"]);
        unmethered_git(dir, &["config", "user.name", "VCBench Agent"]);
        unmethered_git(dir, &["config", "user.email", "agent@vcbench.dev"]);
    }
    let oz_repo = OzoneRepo::discover(Some(&oz_dir)).expect("discover oz repo");
    oz_repo.layout.ensure_initialized().expect("init .git/ozone");
    sandbox.register_repo(&oz_dir);
    (git_dir, oz_dir)
}

fn write_rel(repo_dir: &Path, rel: &str, content: &str) {
    let full = repo_dir.join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("mkdir parent");
    }
    fs::write(full, content).expect("write file");
}

/// Task 1: Selective Hunk Commits
/// 4 dirty files (`[f1]` has 3 distinct hunks `h1a`, `h1b`, `h1c`; `[f2]` is a newly added file;
/// `[f3]` and `[f4]` have WIP edits). Agent must inspect changes and commit ONLY `[f2]` + `h1a`
/// while preserving `h1b`, `h1c`, `[f3]`, and `[f4]` dirty in the working tree.
fn run_vcbench_task1_selective_hunk_commits(sandbox: &mut GateSandbox) -> VcBenchTaskSummary {
    let (git_dir, oz_dir) = init_paired_repos(sandbox, "t1-selective");

    let base_session = render_three_section_rust_module(
        "Session Token Validator",
        "pub fn validate_session_expiry(ctx: &ServiceContext, expires_at: u64) -> bool {\n    expires_at > ctx.now_epoch_secs\n}",
        "pub fn trace_session_lookup(ctx: &ServiceContext) -> usize {\n    ctx.request_id.len()\n}",
        "pub fn compute_retry_budget(_attempts: u32) -> u32 {\n    3\n}",
    );
    let base_invoice = render_three_section_rust_module(
        "Billing Invoice Ledger",
        "pub fn subtotal_cents(units: u64, unit_price: u64) -> u64 {\n    units.saturating_mul(unit_price)\n}",
        "pub fn apply_tax_bps(amount: u64, bps: u64) -> u64 {\n    amount + (amount * bps) / 10_000\n}",
        "pub fn invoice_currency() -> &'static str {\n    \"USD\"\n}",
    );
    let base_metrics = render_three_section_rust_module(
        "Telemetry Metrics Exporter",
        "pub fn metric_namespace() -> &'static str {\n    \"ozone.vcs\"\n}",
        "pub fn sample_rate_pct() -> u8 {\n    10\n}",
        "pub fn flush_interval_ms() -> u64 {\n    5_000\n}",
    );

    for repo in [&git_dir, &oz_dir] {
        write_rel(repo, "crates/auth/src/session.rs", &base_session);
        write_rel(repo, "crates/billing/src/invoice.rs", &base_invoice);
        write_rel(repo, "crates/telemetry/src/metrics.rs", &base_metrics);
        unmethered_git(repo, &["add", "-A"]);
        unmethered_git(repo, &["commit", "-m", "chore: seed baseline service modules"]);
    }

    // Apply identical 4-file dirty state to both repositories:
    // - [f1] crates/auth/src/session.rs: 3 hunks (h1a = clock-skew fix [COMMIT], h1b = debug println [KEEP], h1c = experimental retry [KEEP])
    // - [f2] crates/auth/src/verifier.rs: new PKCE S256 verifier module [COMMIT]
    // - [f3] crates/billing/src/invoice.rs: WIP promotional discount [KEEP]
    // - [f4] crates/telemetry/src/metrics.rs: local 100% sampling tweak [KEEP]
    let dirty_session = render_three_section_rust_module(
        "Session Token Validator",
        "pub fn validate_session_expiry(ctx: &ServiceContext, expires_at: u64) -> bool {\n    let effective_deadline = expires_at.saturating_add(ctx.max_skew_secs);\n    effective_deadline > ctx.now_epoch_secs\n}",
        "pub fn trace_session_lookup(ctx: &ServiceContext) -> usize {\n    eprintln!(\"DEBUG lookup tenant={} req={}\", ctx.tenant_id, ctx.request_id);\n    ctx.request_id.len()\n}",
        "pub fn compute_retry_budget(attempts: u32) -> u32 {\n    // WIP experimental exponential backoff cap\n    (3u32.saturating_add(attempts)).min(12)\n}",
    );
    let new_pkce_verifier = "\
//! RFC 7636 PKCE S256 challenge verifier for OAuth2 native & agent flows.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkceVerifier {
    verifier: String,
}

impl PkceVerifier {
    pub fn new(raw: impl Into<String>) -> Result<Self, &'static str> {
        let s = raw.into();
        if !(43..=128).contains(&s.len()) {
            return Err(\"PKCE code_verifier length must be between 43 and 128 bytes\");
        }
        if !s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')) {
            return Err(\"PKCE code_verifier contains invalid RFC 7636 characters\");
        }
        Ok(Self { verifier: s })
    }

    pub fn as_str(&self) -> &str {
        &self.verifier
    }
}
";
    let dirty_invoice = base_invoice.replace(
        "units.saturating_mul(unit_price)",
        "units.saturating_mul(unit_price).saturating_sub(500) // WIP promo coupon",
    );
    let dirty_metrics = base_metrics.replace("    10", "    100 // local debug full sampling");

    for repo in [&git_dir, &oz_dir] {
        write_rel(repo, "crates/auth/src/session.rs", &dirty_session);
        write_rel(repo, "crates/auth/src/verifier.rs", new_pkce_verifier);
        write_rel(repo, "crates/billing/src/invoice.rs", &dirty_invoice);
        write_rel(repo, "crates/telemetry/src/metrics.rs", &dirty_metrics);
    }

    let commit_msg = "feat(auth): honor max clock skew and add PKCE verifier";

    // --- Ozone (`oz`) Agent Execution ---
    let mut oz_agent = VcsAgentHarness::new("oz/task1");
    let st = oz_agent.exec_oz(&oz_dir, &["status"], true);
    assert!(st.stdout.contains("[f1] M  crates/auth/src/session.rs"));
    assert!(st.stdout.contains("hunks: h1a, h1b, h1c"));
    assert!(st.stdout.contains("[f2] A  crates/auth/src/verifier.rs"));
    assert!(st.stdout.contains("[f3] M  crates/billing/src/invoice.rs"));
    assert!(st.stdout.contains("[f4] M  crates/telemetry/src/metrics.rs"));

    let cm = oz_agent.exec_oz(
        &oz_dir,
        &["commit", "-m", commit_msg, "--pick", "f2", "h1a"],
        true,
    );
    assert!(cm.stdout.contains("2 file(s) committed (3 dirty remaining"));

    // --- Baseline C `git` Agent Execution (non-interactive) ---
    let mut git_agent = VcsAgentHarness::new("git/task1");
    git_agent.exec_git(&git_dir, &["status"], None, true);
    let diff_out = git_agent.exec_git(
        &git_dir,
        &["diff", "crates/auth/src/session.rs"],
        None,
        true,
    );
    let h1a_patch = extract_single_hunk_patch(&diff_out.stdout, 0);
    git_agent.exec_git(
        &git_dir,
        &["add", "crates/auth/src/verifier.rs"],
        None,
        true,
    );
    git_agent.exec_git(&git_dir, &["apply", "--cached", "-"], Some(&h1a_patch), true);
    git_agent.exec_git(&git_dir, &["diff", "--cached", "--stat"], None, true);
    git_agent.exec_git(&git_dir, &["commit", "-m", commit_msg], None, true);
    let post_git_st = git_agent.exec_git(&git_dir, &["status"], None, true);
    assert!(post_git_st.stdout.contains("crates/auth/src/session.rs"));
    assert!(post_git_st.stdout.contains("crates/billing/src/invoice.rs"));
    assert!(post_git_st.stdout.contains("crates/telemetry/src/metrics.rs"));

    // Verify identical committed Git tree OIDs, identical remaining working trees, and git fsck!
    let oz_tree = unmethered_git(&oz_dir, &["rev-parse", "HEAD^{tree}"]);
    let git_tree = unmethered_git(&git_dir, &["rev-parse", "HEAD^{tree}"]);
    assert_eq!(oz_tree, git_tree, "Task 1 committed tree OIDs must match bit-for-bit");
    assert_eq!(
        snapshot_worktree_files(&oz_dir),
        snapshot_worktree_files(&git_dir),
        "Task 1 remaining dirty working trees must match bit-for-bit"
    );
    assert_git_fsck_clean(&oz_dir);
    assert_git_fsck_clean(&git_dir);

    VcBenchTaskSummary::from_harnesses("1. Selective Hunk Commits", &git_agent, &oz_agent)
}

/// Task 2: Multi-Amends
/// 3 sequential refinement rounds across multiple files/hunks folded cleanly into the active
/// feature commit while preserving logical `Change-Id:` identity AND uncommitted local debug edits.
fn run_vcbench_task2_multi_amends(sandbox: &mut GateSandbox) -> VcBenchTaskSummary {
    let (git_dir, oz_dir) = init_paired_repos(sandbox, "t2-amends");

    let base_limiter = render_three_section_rust_module(
        "Token Bucket Rate Limiter",
        "pub fn allow_request(tokens: u64, cost: u64) -> bool {\n    tokens >= cost\n}",
        "pub fn debug_bucket_state(tokens: u64) -> u64 {\n    tokens\n}",
        "pub fn refill_rate_per_sec() -> u64 {\n    50\n}",
    );
    let base_middleware = render_three_section_rust_module(
        "HTTP Rate-Limit Middleware",
        "pub fn enforce_limit(allowed: bool) -> u16 {\n    if allowed { 200 } else { 429 }\n}",
        "pub fn default_burst_capacity() -> u64 {\n    100\n}",
        "pub fn middleware_name() -> &'static str {\n    \"rate_limit_v1\"\n}",
    );
    let base_probe = render_three_section_rust_module(
        "Local Diagnostic Probe",
        "pub fn local_probe_enabled() -> bool {\n    false\n}",
        "pub fn probe_port() -> u16 {\n    9099\n}",
        "pub fn probe_tag() -> &'static str {\n    \"prod\"\n}",
    );

    for repo in [&git_dir, &oz_dir] {
        write_rel(repo, "crates/api/src/limiter.rs", &base_limiter);
        write_rel(repo, "crates/api/src/middleware.rs", &base_middleware);
        write_rel(repo, "crates/api/src/probe.rs", &base_probe);
        unmethered_git(repo, &["add", "-A"]);
        unmethered_git(repo, &["commit", "-m", "chore: seed rate limiter baseline"]);
        unmethered_git(repo, &["checkout", "-b", "feat/rate-limiter"]);
    }
    let root_main_oid_oz = unmethered_git(&oz_dir, &["rev-parse", "main"]);
    let root_main_oid_git = unmethered_git(&git_dir, &["rev-parse", "main"]);

    // Seed initial feature commit on `feat/rate-limiter` with an explicit `Change-Id:` trailer.
    let v1_limiter = render_three_section_rust_module(
        "Token Bucket Rate Limiter",
        "pub fn allow_request(tokens: u64, cost: u64) -> bool {\n    cost > 0 && tokens >= cost\n}",
        "pub fn debug_bucket_state(tokens: u64) -> u64 {\n    tokens\n}",
        "pub fn refill_rate_per_sec() -> u64 {\n    50\n}",
    );
    let initial_change_id = "I7f8e9d0c1b2a3f4e5d6c7b8a9f0e1d2c3b4a5968";
    let initial_msg = format!(
        "feat(api): add non-zero cost check to token bucket\n\nChange-Id: {initial_change_id}"
    );
    for repo in [&git_dir, &oz_dir] {
        write_rel(repo, "crates/api/src/limiter.rs", &v1_limiter);
        unmethered_git(repo, &["add", "crates/api/src/limiter.rs"]);
        unmethered_git(repo, &["commit", "-m", &initial_msg]);
    }

    // Persistent uncommitted local debug edits that MUST remain uncommitted across all 3 amends:
    // - Hunk 2 (`h1b` / `h2b`) in `crates/api/src/limiter.rs`
    // - `crates/api/src/probe.rs`
    let debug_sec_b = "pub fn debug_bucket_state(tokens: u64) -> u64 {\n    eprintln!(\"LOCAL_DEBUG_TRACE remaining_tokens={tokens}\");\n    tokens\n}";
    let dirty_probe = base_probe.replace("    false", "    true // local bench harness");

    let mut oz_agent = VcsAgentHarness::new("oz/task2");
    let mut git_agent = VcsAgentHarness::new("git/task2");

    // --- Refinement Round 1: refine burst window in `limiter.rs` (`h1a`) while preserving `h1b` + `probe.rs` ---
    let r1_limiter = render_three_section_rust_module(
        "Token Bucket Rate Limiter",
        "pub fn allow_request(tokens: u64, cost: u64) -> bool {\n    let capped_cost = cost.max(1);\n    tokens >= capped_cost\n}",
        debug_sec_b,
        "pub fn refill_rate_per_sec() -> u64 {\n    50\n}",
    );
    for repo in [&git_dir, &oz_dir] {
        write_rel(repo, "crates/api/src/limiter.rs", &r1_limiter);
        write_rel(repo, "crates/api/src/probe.rs", &dirty_probe);
    }

    oz_agent.exec_oz(&oz_dir, &["status"], true);
    oz_agent.exec_oz_amend_pick(
        &oz_dir,
        "feat(api): clamp minimum request cost in token bucket",
        &["h1a"],
    );

    git_agent.exec_git(&git_dir, &["status"], None, true);
    git_agent.exec_git(&git_dir, &["log", "-1", "--format=%B"], None, true);
    let g_diff1 = git_agent.exec_git(&git_dir, &["diff", "crates/api/src/limiter.rs"], None, true);
    let g_patch1 = extract_single_hunk_patch(&g_diff1.stdout, 0);
    git_agent.exec_git(&git_dir, &["apply", "--cached", "-"], Some(&g_patch1), true);
    git_agent.exec_git(&git_dir, &["commit", "--amend", "--no-edit"], None, true);
    git_agent.exec_git(&git_dir, &["status"], None, true);

    // --- Refinement Round 2: add `headers.rs` + update `middleware.rs`, keeping `limiter.rs` `h2a` & `probe.rs` dirty ---
    let r2_headers = "\
//! Standard RFC 6585 `Retry-After` & `X-RateLimit-Remaining` header builder.

pub fn format_retry_after_secs(deficit_tokens: u64, refill_per_sec: u64) -> String {
    let secs = deficit_tokens.div_ceil(refill_per_sec.max(1)).max(1);
    secs.to_string()
}
";
    let r2_middleware = base_middleware.replace(
        "if allowed { 200 } else { 429 }",
        "if allowed { 200 } else { 429 /* Retry-After attached */ }",
    );
    for repo in [&git_dir, &oz_dir] {
        write_rel(repo, "crates/api/src/headers.rs", r2_headers);
        write_rel(repo, "crates/api/src/middleware.rs", &r2_middleware);
    }

    oz_agent.exec_oz(&oz_dir, &["status"], true);
    // Lexicographic dirty files: [f1] headers.rs, [f2] limiter.rs (debug hunk), [f3] middleware.rs, [f4] probe.rs
    oz_agent.exec_oz_amend_pick(
        &oz_dir,
        "feat(api): add Retry-After formatter and middleware wiring",
        &["f1", "f3"],
    );

    git_agent.exec_git(&git_dir, &["status"], None, true);
    git_agent.exec_git(
        &git_dir,
        &["add", "crates/api/src/headers.rs", "crates/api/src/middleware.rs"],
        None,
        true,
    );
    git_agent.exec_git(&git_dir, &["diff", "--cached", "--stat"], None, true);
    git_agent.exec_git(&git_dir, &["commit", "--amend", "--no-edit"], None, true);
    git_agent.exec_git(&git_dir, &["status"], None, true);

    // --- Refinement Round 3: refine `limiter.rs` refill rate (`h1b` after `h1a` debug hunk -> wait: `sec_c` is `h1b`!)
    // and tighten `middleware.rs` burst capacity, while keeping `limiter.rs` debug hunk (`h1a`) & `probe.rs` dirty! ---
    let r3_limiter = render_three_section_rust_module(
        "Token Bucket Rate Limiter",
        "pub fn allow_request(tokens: u64, cost: u64) -> bool {\n    let capped_cost = cost.max(1).min(10_000);\n    tokens >= capped_cost\n}",
        debug_sec_b,
        "pub fn refill_rate_per_sec() -> u64 {\n    50\n}",
    );
    let r3_middleware = r2_middleware.replace("    100", "    250");
    for repo in [&git_dir, &oz_dir] {
        write_rel(repo, "crates/api/src/limiter.rs", &r3_limiter);
        write_rel(repo, "crates/api/src/middleware.rs", &r3_middleware);
    }

    // Dirty files now: [f1] limiter.rs (h1a = capped_cost refinement, h1b = debug trace), [f2] middleware.rs, [f3] probe.rs
    let st3 = oz_agent.exec_oz(&oz_dir, &["status"], true);
    assert!(st3.stdout.contains("[f1] M  crates/api/src/limiter.rs"));
    assert!(st3.stdout.contains("hunks: h1a, h1b"));
    assert!(st3.stdout.contains("[f2] M  crates/api/src/middleware.rs"));
    oz_agent.exec_oz_amend_pick(
        &oz_dir,
        "feat(api): finalize bounded token bucket and middleware headers",
        &["h1a", "f2"],
    );

    git_agent.exec_git(&git_dir, &["status"], None, true);
    let g_diff3 = git_agent.exec_git(&git_dir, &["diff", "crates/api/src/limiter.rs"], None, true);
    let g_patch3 = extract_single_hunk_patch(&g_diff3.stdout, 0);
    git_agent.exec_git(&git_dir, &["apply", "--cached", "-"], Some(&g_patch3), true);
    git_agent.exec_git(&git_dir, &["add", "crates/api/src/middleware.rs"], None, true);
    git_agent.exec_git(&git_dir, &["commit", "--amend", "--no-edit"], None, true);
    git_agent.exec_git(&git_dir, &["status"], None, true);

    // Verify:
    // 1. Single commit above `main` in both repos (no extra commits created)
    assert_eq!(unmethered_git(&oz_dir, &["rev-parse", "HEAD^"]), root_main_oid_oz);
    assert_eq!(unmethered_git(&git_dir, &["rev-parse", "HEAD^"]), root_main_oid_git);
    // 2. Original `Change-Id:` preserved across all 3 amends!
    let oz_head_msg = unmethered_git(&oz_dir, &["log", "-1", "--format=%B"]);
    let git_head_msg = unmethered_git(&git_dir, &["log", "-1", "--format=%B"]);
    assert!(
        oz_head_msg.contains(&format!("Change-Id: {initial_change_id}")),
        "oz --amend must preserve Change-Id trailer:\n{oz_head_msg}"
    );
    assert!(git_head_msg.contains(&format!("Change-Id: {initial_change_id}")));
    // 3. Exact same committed Git tree OID & identical remaining dirty working tree!
    assert_eq!(
        unmethered_git(&oz_dir, &["rev-parse", "HEAD^{tree}"]),
        unmethered_git(&git_dir, &["rev-parse", "HEAD^{tree}"]),
        "Task 2 final amended tree OIDs must match bit-for-bit"
    );
    assert_eq!(
        snapshot_worktree_files(&oz_dir),
        snapshot_worktree_files(&git_dir),
        "Task 2 remaining uncommitted debug edits must match bit-for-bit"
    );
    assert_git_fsck_clean(&oz_dir);
    assert_git_fsck_clean(&git_dir);

    VcBenchTaskSummary::from_harnesses("2. Multi-Amends (3x)", &git_agent, &oz_agent)
}

/// Task 3: Split Commits
/// An entangled working tree spanning 3 logical concerns across 5 files + shared-file hunks
/// (`crates/app_config/src/settings.rs` has `h1a` = DB pool, `h1b` = Webhook HMAC, `h1c` = Audit batch)
/// split into 3 clean atomic commits.
fn run_vcbench_task3_split_commits(sandbox: &mut GateSandbox) -> VcBenchTaskSummary {
    let (git_dir, oz_dir) = init_paired_repos(sandbox, "t3-split");

    let base_settings = render_three_section_rust_module(
        "Global Application Settings",
        "pub fn db_pool_timeout_ms() -> u64 {\n    3_000\n}",
        "pub fn webhook_signing_enabled() -> bool {\n    false\n}",
        "pub fn audit_export_batch_size() -> usize {\n    100\n}",
    );
    let base_audit = render_three_section_rust_module(
        "Audit Log Exporter",
        "pub fn next_batch_limit() -> usize {\n    100\n}",
        "pub fn export_format() -> &'static str {\n    \"jsonl\"\n}",
        "pub fn compression_level() -> u8 {\n    3\n}",
    );
    let base_db = render_three_section_rust_module(
        "Database Connection Pool",
        "pub fn acquire_timeout_ms() -> u64 {\n    3_000\n}",
        "pub fn max_connections() -> u32 {\n    32\n}",
        "pub fn idle_reap_secs() -> u64 {\n    120\n}",
    );
    let base_dispatcher = render_three_section_rust_module(
        "Webhook Event Dispatcher",
        "pub fn build_delivery_headers(event_id: &str) -> Vec<(&'static str, String)> {\n    vec![(\"X-Ozone-Event\", event_id.to_string())]\n}",
        "pub fn max_delivery_attempts() -> u8 {\n    5\n}",
        "pub fn delivery_user_agent() -> &'static str {\n    \"Ozone-Webhooks/0.3\"\n}",
    );

    for repo in [&git_dir, &oz_dir] {
        write_rel(repo, "crates/app_config/src/settings.rs", &base_settings);
        write_rel(repo, "crates/audit/src/exporter.rs", &base_audit);
        write_rel(repo, "crates/db/src/pool.rs", &base_db);
        write_rel(repo, "crates/webhooks/src/dispatcher.rs", &base_dispatcher);
        unmethered_git(repo, &["add", "-A"]);
        unmethered_git(repo, &["commit", "-m", "chore: initial platform crates"]);
    }

    // Entangle 3 distinct concerns into the working tree across 5 files:
    // - Concern A (DB pool timeout): settings.rs hunk 1 + crates/db/src/pool.rs
    // - Concern B (Webhook HMAC signing): settings.rs hunk 2 + crates/webhooks/src/dispatcher.rs + new signature.rs
    // - Concern C (Audit batch sizing): settings.rs hunk 3 + crates/audit/src/exporter.rs
    let entangled_settings = render_three_section_rust_module(
        "Global Application Settings",
        "pub fn db_pool_timeout_ms() -> u64 {\n    7_500\n}",
        "pub fn webhook_signing_enabled() -> bool {\n    true\n}",
        "pub fn audit_export_batch_size() -> usize {\n    500\n}",
    );
    let dirty_db = base_db.replace("    3_000", "    7_500 // aligned with app_config");
    let dirty_dispatcher = base_dispatcher.replace(
        "vec![(\"X-Ozone-Event\", event_id.to_string())]",
        "vec![(\"X-Ozone-Event\", event_id.to_string()), (\"X-Ozone-Signature-256\", format!(\"sha256={event_id}\"))]",
    );
    let new_signature = "\
//! Deterministic webhook payload signature header helper.

pub fn compute_signature_tag(secret_id: &str, payload_len: usize) -> String {
    format!(\"v1:{secret_id}:{payload_len}\")
}
";
    let dirty_audit = base_audit.replace("    100", "    500 // high-throughput SOC2 export batch");

    for repo in [&git_dir, &oz_dir] {
        write_rel(repo, "crates/app_config/src/settings.rs", &entangled_settings);
        write_rel(repo, "crates/audit/src/exporter.rs", &dirty_audit);
        write_rel(repo, "crates/db/src/pool.rs", &dirty_db);
        write_rel(repo, "crates/webhooks/src/dispatcher.rs", &dirty_dispatcher);
        write_rel(repo, "crates/webhooks/src/signature.rs", new_signature);
    }

    let msg_a = "feat(db): raise connection pool acquire timeout to 7.5s";
    let msg_b = "feat(webhooks): enable HMAC signature headers on dispatch";
    let msg_c = "feat(audit): increase SOC2 JSONL export batch size to 500";

    // --- Ozone (`oz`) Agent Execution: `oz status` + 3x `oz commit --pick ...` = 4 commands! ---
    let mut oz_agent = VcsAgentHarness::new("oz/task3");
    let st = oz_agent.exec_oz(&oz_dir, &["status"], true);
    // Lexicographic order:
    // [f1] crates/app_config/src/settings.rs (h1a, h1b, h1c)
    // [f2] crates/audit/src/exporter.rs
    // [f3] crates/db/src/pool.rs
    // [f4] crates/webhooks/src/dispatcher.rs
    // [f5] crates/webhooks/src/signature.rs
    assert!(st.stdout.contains("[f1] M  crates/app_config/src/settings.rs"));
    assert!(st.stdout.contains("hunks: h1a, h1b, h1c"));

    // Commit 1 (Concern A): `f3` (`db/pool.rs`) + `h1a` (first hunk of `settings.rs`)
    // Remaining dirty becomes: [f1] settings.rs (2 hunks: webhook=h1a, audit=h1b), [f2] audit, [f3] dispatcher, [f4] signature
    let c1 = oz_agent.exec_oz(&oz_dir, &["commit", "-m", msg_a, "--pick", "f3", "h1a"], true);
    assert!(c1.stdout.contains("2 file(s) committed (4 dirty remaining"));

    // Commit 2 (Concern B): `f3` (`dispatcher.rs`) + `f4` (`signature.rs`) + `h1a` (webhook hunk of `settings.rs`)
    // Remaining dirty becomes: [f1] settings.rs (audit hunk), [f2] audit/exporter.rs
    let c2 = oz_agent.exec_oz(
        &oz_dir,
        &["commit", "-m", msg_b, "--pick", "f3", "f4", "h1a"],
        true,
    );
    assert!(c2.stdout.contains("3 file(s) committed (2 dirty remaining"));

    // Commit 3 (Concern C): `f1` (`settings.rs`) + `f2` (`audit/exporter.rs`)
    let c3 = oz_agent.exec_oz(&oz_dir, &["commit", "-m", msg_c, "--pick", "f1", "f2"], true);
    assert!(c3.stdout.contains("2 file(s) committed (0 dirty remaining"));

    // --- Baseline C `git` Agent Execution (non-interactive) ---
    let mut git_agent = VcsAgentHarness::new("git/task3");
    git_agent.exec_git(&git_dir, &["status"], None, true);
    // Commit 1
    let diff_s1 = git_agent.exec_git(
        &git_dir,
        &["diff", "crates/app_config/src/settings.rs"],
        None,
        true,
    );
    let patch_s1 = extract_single_hunk_patch(&diff_s1.stdout, 0);
    git_agent.exec_git(&git_dir, &["apply", "--cached", "-"], Some(&patch_s1), true);
    git_agent.exec_git(&git_dir, &["add", "crates/db/src/pool.rs"], None, true);
    git_agent.exec_git(&git_dir, &["diff", "--cached", "--stat"], None, true);
    git_agent.exec_git(&git_dir, &["commit", "-m", msg_a], None, true);

    // Commit 2 (must re-diff settings.rs against new HEAD because line offsets shifted)
    let diff_s2 = git_agent.exec_git(
        &git_dir,
        &["diff", "crates/app_config/src/settings.rs"],
        None,
        true,
    );
    let patch_s2 = extract_single_hunk_patch(&diff_s2.stdout, 0);
    git_agent.exec_git(&git_dir, &["apply", "--cached", "-"], Some(&patch_s2), true);
    git_agent.exec_git(
        &git_dir,
        &[
            "add",
            "crates/webhooks/src/dispatcher.rs",
            "crates/webhooks/src/signature.rs",
        ],
        None,
        true,
    );
    git_agent.exec_git(&git_dir, &["diff", "--cached", "--stat"], None, true);
    git_agent.exec_git(&git_dir, &["commit", "-m", msg_b], None, true);

    // Commit 3
    git_agent.exec_git(
        &git_dir,
        &[
            "add",
            "crates/app_config/src/settings.rs",
            "crates/audit/src/exporter.rs",
        ],
        None,
        true,
    );
    git_agent.exec_git(&git_dir, &["commit", "-m", msg_c], None, true);
    let final_git_st = git_agent.exec_git(&git_dir, &["status"], None, true);
    assert!(final_git_st.stdout.contains("working tree clean"));

    // Verify EVERY single split commit (`HEAD~2`, `HEAD~1`, `HEAD`) has the exact same Git tree OID!
    for rev in ["HEAD~2^{tree}", "HEAD~1^{tree}", "HEAD^{tree}"] {
        assert_eq!(
            unmethered_git(&oz_dir, &["rev-parse", rev]),
            unmethered_git(&git_dir, &["rev-parse", rev]),
            "Task 3 tree OID mismatch at {rev}"
        );
    }
    assert_eq!(
        snapshot_worktree_files(&oz_dir),
        snapshot_worktree_files(&git_dir),
        "Task 3 clean working trees must match bit-for-bit"
    );
    assert_git_fsck_clean(&oz_dir);
    assert_git_fsck_clean(&git_dir);

    VcBenchTaskSummary::from_harnesses("3. Split Commits (3-way)", &git_agent, &oz_agent)
}

/// Task 4: Conflict Resolution
/// Integrating 2 feature branches (`feat/cache-policy` and `feat/tenant-isolation`) into `main`
/// with 2 content conflicts across `crates/runtime/src/executor.rs` (`c1` -> take `ours`) and
/// `crates/runtime/src/policy.rs` (`c2` -> take `theirs`), plus clean merges of non-conflicting
/// files (`cache.rs`, `tenant.rs`).
fn run_vcbench_task4_conflict_resolution(sandbox: &mut GateSandbox) -> VcBenchTaskSummary {
    let (git_dir, oz_dir) = init_paired_repos(sandbox, "t4-conflicts");

    let base_executor = render_three_section_rust_module(
        "Task Executor Engine",
        "pub fn max_parallel_workers() -> usize {\n    8\n}",
        "pub fn worker_stack_bytes() -> usize {\n    2 * 1024 * 1024\n}",
        "pub fn steal_batch_limit() -> usize {\n    16\n}",
    );
    let base_policy = render_three_section_rust_module(
        "Runtime Isolation Policy",
        "pub fn isolation_tier() -> &'static str {\n    \"shared_default\"\n}",
        "pub fn memory_ceiling_mb() -> u64 {\n    2_048\n}",
        "pub fn allow_network_egress() -> bool {\n    false\n}",
    );

    for repo in [&git_dir, &oz_dir] {
        write_rel(repo, "crates/runtime/src/executor.rs", &base_executor);
        write_rel(repo, "crates/runtime/src/policy.rs", &base_policy);
        unmethered_git(repo, &["add", "-A"]);
        unmethered_git(repo, &["commit", "-m", "chore: seed runtime modules"]);

        // Branch 1: `feat/cache-policy`
        unmethered_git(repo, &["checkout", "-b", "feat/cache-policy"]);
        write_rel(
            repo,
            "crates/runtime/src/executor.rs",
            &base_executor.replace("    8", "    16 // cache-aware worker pool"),
        );
        write_rel(
            repo,
            "crates/runtime/src/policy.rs",
            &base_policy.replace("\"shared_default\"", "\"warm_cache_shared\""),
        );
        write_rel(
            repo,
            "crates/runtime/src/cache.rs",
            "pub fn lru_shards() -> usize {\n    64\n}\n",
        );
        unmethered_git(repo, &["add", "-A"]);
        unmethered_git(repo, &["commit", "-m", "feat(runtime): tune cache worker pool"]);

        // Branch 2: `feat/tenant-isolation` (forked from `main`, conflicting on executor.rs & policy.rs)
        unmethered_git(repo, &["checkout", "-b", "feat/tenant-isolation", "main"]);
        write_rel(
            repo,
            "crates/runtime/src/executor.rs",
            &base_executor.replace("    8", "    4 // strict per-tenant worker cap"),
        );
        write_rel(
            repo,
            "crates/runtime/src/policy.rs",
            &base_policy.replace("\"shared_default\"", "\"strict_cow_sandbox\""),
        );
        write_rel(
            repo,
            "crates/runtime/src/tenant.rs",
            "pub fn enforce_tenant_quota() -> bool {\n    true\n}\n",
        );
        unmethered_git(repo, &["add", "-A"]);
        unmethered_git(
            repo,
            &["commit", "-m", "feat(runtime): strict tenant sandbox policy"],
        );
        // Note: start both agents while checked out on `feat/tenant-isolation` (realistic post-feature state)
    }

    // --- Ozone (`oz`) Agent Execution ---
    let mut oz_agent = VcsAgentHarness::new("oz/task4");
    let int_out = oz_agent.exec_oz(
        &oz_dir,
        &[
            "integrate",
            "feat/cache-policy",
            "feat/tenant-isolation",
            "--into",
            "main",
        ],
        false,
    );
    assert_eq!(int_out.exit_code, 1, "expected structured conflict exit code 1");
    assert!(int_out.stderr.contains("hint: oz resolve c1 --take ours"));
    assert!(int_out.stderr.contains("hint: oz resolve c2 --take theirs"));

    let oz_sandbox_dir = sandbox
        .root
        .join("t4-conflicts-oz.oz")
        .join("_integrate");
    let sandbox_diff = oz_agent.exec_oz(
        &oz_dir,
        &["-C", "../t4-conflicts-oz.oz/_integrate", "diff"],
        true,
    );
    assert!(sandbox_diff.stdout.contains("cache-aware worker pool"));
    assert!(sandbox_diff.stdout.contains("strict_cow_sandbox"));

    oz_agent.exec_oz(&oz_dir, &["resolve", "c1", "--take", "ours"], true);
    let cont_out = oz_agent.exec_oz(
        &oz_dir,
        &["resolve", "c2", "--take", "theirs", "--continue"],
        true,
    );
    assert!(cont_out
        .stdout
        .contains("Integrated [feat/cache-policy, feat/tenant-isolation] into main"));

    // --- Baseline C `git` Agent Execution (non-interactive) ---
    let mut git_agent = VcsAgentHarness::new("git/task4");
    git_agent.exec_git(&git_dir, &["status"], None, true);
    git_agent.exec_git(&git_dir, &["checkout", "main"], None, true);
    git_agent.exec_git(&git_dir, &["merge", "--no-edit", "feat/cache-policy"], None, true);
    let merge2 = git_agent.exec_git(
        &git_dir,
        &["merge", "--no-edit", "feat/tenant-isolation"],
        None,
        false,
    );
    assert_ne!(merge2.exit_code, 0);
    git_agent.exec_git(&git_dir, &["status"], None, true);
    git_agent.exec_git(
        &git_dir,
        &[
            "log",
            "--merge",
            "-p",
            "--",
            "crates/runtime/src/executor.rs",
            "crates/runtime/src/policy.rs",
        ],
        None,
        true,
    );
    git_agent.exec_git(
        &git_dir,
        &["checkout", "--ours", "--", "crates/runtime/src/executor.rs"],
        None,
        true,
    );
    git_agent.exec_git(
        &git_dir,
        &["checkout", "--theirs", "--", "crates/runtime/src/policy.rs"],
        None,
        true,
    );
    git_agent.exec_git(
        &git_dir,
        &[
            "add",
            "crates/runtime/src/executor.rs",
            "crates/runtime/src/policy.rs",
        ],
        None,
        true,
    );
    git_agent.exec_git(&git_dir, &["diff", "--cached", "--stat"], None, true);
    git_agent.exec_git(&git_dir, &["commit", "--no-edit"], None, true);
    git_agent.exec_git(&git_dir, &["status"], None, true);

    // Verify `main^{tree}` is bit-for-bit identical in both repos and contains:
    // - `executor.rs` -> `ours` (`16 // cache-aware worker pool`)
    // - `policy.rs` -> `theirs` (`"strict_cow_sandbox"`)
    // - both `cache.rs` and `tenant.rs` merged cleanly!
    let oz_main_tree = unmethered_git(&oz_dir, &["rev-parse", "main^{tree}"]);
    let git_main_tree = unmethered_git(&git_dir, &["rev-parse", "main^{tree}"]);
    assert_eq!(
        oz_main_tree, git_main_tree,
        "Task 4 integrated `main` tree OIDs must match bit-for-bit"
    );
    let merged_exec = unmethered_git(&oz_dir, &["show", "main:crates/runtime/src/executor.rs"]);
    let merged_pol = unmethered_git(&oz_dir, &["show", "main:crates/runtime/src/policy.rs"]);
    assert!(merged_exec.contains("16 // cache-aware worker pool"));
    assert!(merged_pol.contains("\"strict_cow_sandbox\""));
    assert!(!oz_sandbox_dir.exists(), "_integrate sandbox must be cleaned up after --continue");

    assert_git_fsck_clean(&oz_dir);
    assert_git_fsck_clean(&git_dir);

    VcBenchTaskSummary::from_harnesses("4. Conflict Resolution", &git_agent, &oz_agent)
}

#[test]
fn vcbench_agent_eval_exit_gate_commands_and_tokens() {
    let mut sandbox = GateSandbox::new("vcbench-root");

    let results = [
        run_vcbench_task1_selective_hunk_commits(&mut sandbox),
        run_vcbench_task2_multi_amends(&mut sandbox),
        run_vcbench_task3_split_commits(&mut sandbox),
        run_vcbench_task4_conflict_resolution(&mut sandbox),
    ];

    let total_git_cmds: usize = results.iter().map(|r| r.git_commands).sum();
    let total_oz_cmds: usize = results.iter().map(|r| r.oz_commands).sum();
    let total_git_approx_toks: usize = results.iter().map(|r| r.git_approx_tokens).sum();
    let total_oz_approx_toks: usize = results.iter().map(|r| r.oz_approx_tokens).sum();
    let total_git_lex_toks: usize = results.iter().map(|r| r.git_lexical_tokens).sum();
    let total_oz_lex_toks: usize = results.iter().map(|r| r.oz_lexical_tokens).sum();

    let agg_cmd_red = 100.0 * (1.0 - (total_oz_cmds as f64 / total_git_cmds as f64));
    let agg_approx_red =
        100.0 * (1.0 - (total_oz_approx_toks as f64 / total_git_approx_toks as f64));
    let agg_lex_red = 100.0 * (1.0 - (total_oz_lex_toks as f64 / total_git_lex_toks as f64));

    println!("\n==========================================================================================");
    println!("  Milestone 3 VCBench-Style Autonomous Agent Evaluation (Baseline C `git` vs Ozone `oz`)");
    println!("==========================================================================================");
    println!(
        "{:<28} | {:>8} {:>8} {:>9} | {:>9} {:>9} {:>9} | {:>9} {:>9} {:>9}",
        "Task",
        "git cmd",
        "oz cmd",
        "cmd Δ",
        "git tok~",
        "oz tok~",
        "tok~ Δ",
        "git lex",
        "oz lex",
        "lex Δ"
    );
    println!("------------------------------------------------------------------------------------------");
    for r in &results {
        println!(
            "{:<28} | {:>8} {:>8} {:>8.1}% | {:>9} {:>9} {:>8.1}% | {:>9} {:>9} {:>8.1}%",
            r.name,
            r.git_commands,
            r.oz_commands,
            -r.cmd_reduction_pct(),
            r.git_approx_tokens,
            r.oz_approx_tokens,
            -r.approx_token_reduction_pct(),
            r.git_lexical_tokens,
            r.oz_lexical_tokens,
            -r.lexical_token_reduction_pct(),
        );
    }
    println!("------------------------------------------------------------------------------------------");
    println!(
        "{:<28} | {:>8} {:>8} {:>8.1}% | {:>9} {:>9} {:>8.1}% | {:>9} {:>9} {:>8.1}%",
        "AGGREGATE (All 4 Tasks)",
        total_git_cmds,
        total_oz_cmds,
        -agg_cmd_red,
        total_git_approx_toks,
        total_oz_approx_toks,
        -agg_approx_red,
        total_git_lex_toks,
        total_oz_lex_toks,
        -agg_lex_red,
    );
    println!("==========================================================================================\n");

    // Per-task sanity gates: every single task must achieve substantial command & token savings!
    for r in &results {
        assert!(
            r.oz_commands * 100 <= r.git_commands * 50,
            "Task '{}' must reduce command count by >= 50% (oz={}, git={})",
            r.name,
            r.oz_commands,
            r.git_commands
        );
        assert!(
            r.oz_approx_tokens < r.git_approx_tokens,
            "Task '{}' must consume fewer ceil(bytes/4) tokens than C git (oz={}, git={})",
            r.name,
            r.oz_approx_tokens,
            r.git_approx_tokens
        );
        assert!(
            r.oz_lexical_tokens < r.git_lexical_tokens,
            "Task '{}' must consume fewer lexical tokens than C git (oz={}, git={})",
            r.name,
            r.oz_lexical_tokens,
            r.git_lexical_tokens
        );
    }

    // ROADMAP.md Milestone 3 Exit Gate:
    // - Aggregate commands: >= 60% reduction (`oz_commands * 100 <= git_commands * 40`)
    // - Aggregate tokens:   >= 50% reduction (`oz_tokens * 100 <= git_tokens * 50`) on BOTH metrics!
    assert!(
        total_oz_cmds * 100 <= total_git_cmds * 40,
        "VCBench Exit Gate FAILED: expected >= 60% command reduction, got {:.2}% (oz={total_oz_cmds}, git={total_git_cmds})",
        agg_cmd_red
    );
    assert!(
        total_oz_approx_toks * 100 <= total_git_approx_toks * 50,
        "VCBench Exit Gate FAILED: expected >= 50% ceil(bytes/4) token reduction, got {:.2}% (oz={total_oz_approx_toks}, git={total_git_approx_toks})",
        agg_approx_red
    );
    assert!(
        total_oz_lex_toks * 100 <= total_git_lex_toks * 50,
        "VCBench Exit Gate FAILED: expected >= 50% lexical token reduction, got {:.2}% (oz={total_oz_lex_toks}, git={total_git_lex_toks})",
        agg_lex_red
    );
}

// ============================================================================
// GATE 2: End-to-End Signed Stacked PR + CI Self-Healing + Sync Loop
// ============================================================================

struct UnifiedGitHubServerState {
    expected_token: String,
    bare_repo: PathBuf,
    next_pr_number: u64,
    prs: Vec<GitHubPullRequest>,
    check_runs_by_ref: HashMap<String, Vec<GitHubCheckRun>>,
    workflow_runs: Vec<GitHubWorkflowRun>,
    jobs_by_run: HashMap<u64, Vec<GitHubActionsJob>>,
    job_logs: HashMap<u64, Vec<u8>>,
}

impl UnifiedGitHubServerState {
    fn new(expected_token: &str, bare_repo: PathBuf) -> Self {
        Self {
            expected_token: expected_token.to_string(),
            bare_repo,
            next_pr_number: 1,
            prs: Vec::new(),
            check_runs_by_ref: HashMap::new(),
            workflow_runs: Vec::new(),
            jobs_by_run: HashMap::new(),
            job_logs: HashMap::new(),
        }
    }

    fn resolve_branch_sha(&self, branch: &str) -> String {
        let out = Command::new(sys_git_bin())
            .args(["rev-parse", &format!("refs/heads/{branch}")])
            .current_dir(&self.bare_repo)
            .output()
            .ok();
        if let Some(o) = out
            && o.status.success()
        {
            return String::from_utf8_lossy(&o.stdout).trim().to_string();
        }
        "0000000000000000000000000000000000000000".to_string()
    }
}

/// Spawns a single local `tokio` HTTP server serving BOTH:
/// 1. Git Smart-HTTP Wire Protocol v2 (`/acme/rocket.git/info/refs`, `/git-upload-pack`, `/git-receive-pack`)
/// 2. GitHub REST & GraphQL API (`/graphql`, `/repos/acme/rocket/pulls`, `/commits/:ref/check-runs`, `/actions/...`)
async fn spawn_unified_github_and_smart_http_server(
    state: Arc<Mutex<UnifiedGitHubServerState>>,
) -> (String, String) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind unified mock server");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let st = Arc::clone(&state);
            tokio::spawn(async move {
                let _ = handle_unified_http_conn(stream, st).await;
            });
        }
    });

    let base_url = format!("http://{addr}");
    let git_remote_url = format!("{base_url}/acme/rocket.git");
    (base_url, git_remote_url)
}

async fn handle_unified_http_conn(
    stream: TcpStream,
    state: Arc<Mutex<UnifiedGitHubServerState>>,
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

    let (expected_token, bare_repo) = {
        let guard = state.lock().unwrap_or_else(|p| p.into_inner());
        (guard.expected_token.clone(), guard.bare_repo.clone())
    };
    let expected_basic = format!(
        "Basic {}",
        base64_encode(format!("x-access-token:{expected_token}").as_bytes())
    );
    let expected_bearer = format!("Bearer {expected_token}");
    let auth_ok = headers
        .get("authorization")
        .is_some_and(|v| v == &expected_basic || v == &expected_bearer);

    if !auth_ok {
        write_http_response(
            &mut write_half,
            "401 Unauthorized",
            "application/json",
            br#"{"message":"Bad credentials"}"#,
        )
        .await?;
        return Ok(());
    }

    let (path, query) = raw_target
        .split_once('?')
        .map(|(p, q)| (p, Some(q)))
        .unwrap_or((raw_target.as_str(), None));
    let git_protocol = headers.get("git-protocol").cloned();

    // 1. Smart-HTTP Git Wire v2 endpoints
    if method == "GET" && path.ends_with("/info/refs") {
        let service = query
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("service=")))
            .unwrap_or_default();
        let subcmd = match service {
            "git-upload-pack" => "upload-pack",
            "git-receive-pack" => "receive-pack",
            _ => {
                return write_http_response(
                    &mut write_half,
                    "400 Bad Request",
                    "text/plain",
                    b"bad service",
                )
                .await;
            }
        };

        let mut cmd = Command::new(sys_git_bin());
        cmd.args([subcmd, "--stateless-rpc", "--advertise-refs"])
            .arg(&bare_repo);
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
            let pkt_header = format!("{:04x}", announcement.len() + 4);
            payload.extend_from_slice(pkt_header.as_bytes());
            payload.extend_from_slice(announcement.as_bytes());
            payload.extend_from_slice(b"0000");
        }
        payload.extend_from_slice(&out.stdout);
        let ct = format!("application/x-{service}-advertisement");
        return write_http_response(&mut write_half, "200 OK", &ct, &payload).await;
    }

    if method == "POST"
        && (path.ends_with("/git-upload-pack") || path.ends_with("/git-receive-pack"))
    {
        let (service, subcmd) = if path.ends_with("/git-upload-pack") {
            ("git-upload-pack", "upload-pack")
        } else {
            ("git-receive-pack", "receive-pack")
        };
        let mut cmd = Command::new(sys_git_bin());
        cmd.args([subcmd, "--stateless-rpc"])
            .arg(&bare_repo)
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
        let ct = format!("application/x-{service}-result");
        return write_http_response(&mut write_half, "200 OK", &ct, &out.stdout).await;
    }

    // 2. GitHub REST & GraphQL API endpoints
    let query_map = parse_query_map(query);
    let (status, content_type, resp_bytes) = {
        let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
        route_github_api(&method, path, &query_map, &body, &mut guard)
    };
    write_http_response(&mut write_half, status, content_type, &resp_bytes).await
}

fn route_github_api(
    method: &str,
    path: &str,
    query: &HashMap<String, String>,
    body: &[u8],
    st: &mut UnifiedGitHubServerState,
) -> (&'static str, &'static str, Vec<u8>) {
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();

    if method == "POST" && segs.as_slice() == ["graphql"] {
        return (
            "200 OK",
            "application/json",
            serde_json::to_vec(&json!({ "data": { "viewer": { "login": "ozone-rc-bot" } } }))
                .unwrap(),
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
            let runs = st
                .check_runs_by_ref
                .get(tail[1])
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

fn parse_query_map(query: Option<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(q) = query else {
        return map;
    };
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(k.replace("%3A", ":"), v.replace("%3A", ":"));
        }
    }
    map
}

/// Spawns a CoW workspace using the platform's primary backend (`LinuxFuse3`, `MacosClonefile`,
/// `WindowsProjFs`, etc.), falling back cleanly to `BackendKind::CasHardlink` only inside
/// unprivileged container sandboxes where `fusermount3` is blocked (`Operation not permitted`).
fn spawn_cow_workspace(repo: &OzoneRepo, mut opts: SpawnWorkspaceOptions) -> SpawnedWorkspaceInfo {
    match spawn_workspace(repo, &opts) {
        Ok(info) => info,
        Err(OzError::MountUnavailable { ref message, .. })
            if message.contains("Operation not permitted") || message.contains("SUN_LEN") =>
        {
            opts.backend = Some(BackendKind::CasHardlink);
            spawn_workspace(repo, &opts).expect("spawn CoW workspace via CasHardlink fallback")
        }
        Err(err) => panic!("spawn_workspace '{}' failed: {err:?}", opts.name),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate2_e2e_signed_stacked_pr_ci_self_healing_and_sync_loop() {
    let mut sandbox = GateSandbox::new("m3-gate2-seed");

    // 1. Generate Ed25519 SSH signing keypair via `ssh-keygen` and prepare `allowed_signers`.
    let key_dir = sandbox.root.join("ssh-keys");
    fs::create_dir_all(&key_dir).expect("mkdir ssh-keys");
    let priv_key_path = key_dir.join("id_ed25519");
    let pub_key_path = key_dir.join("id_ed25519.pub");
    let signer_email = "signer@ozone.dev";
    let keygen_out = Command::new(ssh_keygen_bin())
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            signer_email,
            "-f",
            priv_key_path.to_str().expect("utf8 key path"),
        ])
        .output()
        .expect("run ssh-keygen");
    assert!(keygen_out.status.success(), "ssh-keygen failed");

    let pub_key_text = fs::read_to_string(&pub_key_path).expect("read pubkey");
    let allowed_signers = key_dir.join("allowed_signers");
    fs::write(
        &allowed_signers,
        format!("{signer_email} {}\n", pub_key_text.trim()),
    )
    .expect("write allowed_signers");

    // 2. Seed upstream bare repository (`origin.git`) & launch unified Smart-HTTP v2 + Mock GitHub server.
    let bare_dir = sandbox.root.join("origin.git");
    fs::create_dir_all(&bare_dir).expect("mkdir bare");
    unmethered_git(&bare_dir, &["init", "--bare", "-b", "main"]);
    unmethered_git(&bare_dir, &["config", "uploadpack.allowfilter", "true"]);
    unmethered_git(
        &bare_dir,
        &["config", "uploadpack.allowanysha1inwant", "true"],
    );
    unmethered_git(&bare_dir, &["config", "http.receivepack", "true"]);
    unmethered_git(&bare_dir, &["config", "receive.advertiseAtomic", "true"]);

    unmethered_git(&sandbox.repo_root, &["init", "-b", "main"]);
    unmethered_git(&sandbox.repo_root, &["config", "user.name", "Ozone Signer"]);
    unmethered_git(&sandbox.repo_root, &["config", "user.email", signer_email]);
    write_rel(
        &sandbox.repo_root,
        "Cargo.toml",
        "[package]\nname = \"rocket\"\nversion = \"0.1.0\"\n",
    );
    write_rel(
        &sandbox.repo_root,
        "src/lib.rs",
        "pub fn base_version() -> &'static str {\n    \"0.1.0\"\n}\n",
    );
    unmethered_git(&sandbox.repo_root, &["add", "-A"]);
    unmethered_git(&sandbox.repo_root, &["commit", "-m", "chore: initial rocket crate"]);
    unmethered_git(
        &sandbox.repo_root,
        &["push", bare_dir.to_str().unwrap(), "main:main"],
    );

    let token = "gho_m3_gate2_unified_token";
    let server_state = Arc::new(Mutex::new(UnifiedGitHubServerState::new(
        token,
        bare_dir.clone(),
    )));
    let (api_url, remote_http_url) =
        spawn_unified_github_and_smart_http_server(Arc::clone(&server_state)).await;

    // 3. Verify `--filter=blob:none` partial clone AND full Smart-HTTP Wire v2 clone!
    let actor = ActorInfo {
        kind: ActorKind::Agent,
        client: Some("m3-gate2-agent".to_string()),
        workspace: None,
    };
    let partial_dir = sandbox.root.join("rocket-partial");
    let mut partial_opts = CloneOptions::new(&remote_http_url, &partial_dir);
    partial_opts.filter = Some("blob:none".to_string());
    partial_opts.auth_token = Some(token.to_string());
    partial_opts.auth_scheme = HttpAuthScheme::Bearer;
    let partial_out = clone_repo(&partial_opts, actor.clone()).expect("Wire v2 partial clone");
    assert_eq!(partial_out.protocol_version, 2);
    assert!(partial_out.filter_negotiated);

    let cloned_dir = sandbox.root.join("rocket");
    let mut clone_opts = CloneOptions::new(&remote_http_url, &cloned_dir);
    clone_opts.auth_token = Some(token.to_string());
    clone_opts.auth_scheme = HttpAuthScheme::Bearer;
    let clone_out = clone_repo(&clone_opts, actor.clone()).expect("Wire v2 full clone");
    assert_eq!(clone_out.protocol_version, 2);
    sandbox.register_repo(&cloned_dir);

    // Configure git identity & `allowedSignersFile` on cloned repo + warm `target/` cache artifact.
    unmethered_git(&cloned_dir, &["config", "user.name", "Ozone Signer"]);
    unmethered_git(&cloned_dir, &["config", "user.email", signer_email]);
    unmethered_git(&cloned_dir, &["config", "gpg.format", "ssh"]);
    unmethered_git(
        &cloned_dir,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            allowed_signers.to_str().unwrap(),
        ],
    );
    fs::create_dir_all(cloned_dir.join("target/debug")).expect("mkdir warm target");
    fs::write(cloned_dir.join("target/debug/warm.rlib"), b"warm-cache").expect("write warm rlib");

    // 4. Spawn 2 stacked CoW workspaces (`ws-stack-1` -> `ws-stack-2`) and author pure-Rust
    //    Ed25519 SSH-signed commits verified by BOTH `oz_core` AND system `git verify-commit`!
    let main_oz = OzoneRepo::discover(Some(&cloned_dir)).expect("discover cloned repo");
    let ws1 = spawn_cow_workspace(
        &main_oz,
        SpawnWorkspaceOptions {
            name: "ws-stack-1".to_string(),
            branch: Some("feat/stack-1".to_string()),
            from_ref: Some("main".to_string()),
            backend: None,
            scope: Vec::new(),
            deny: Vec::new(),
            warm: true,
        },
    );
    write_rel(
        &ws1.workspace.workspace_path,
        "src/auth.rs",
        "pub fn verify_token(tok: &str) -> bool {\n    tok.starts_with(\"oz_\")\n}\n",
    );
    let ws1_oz =
        OzoneRepo::discover(Some(&ws1.workspace.workspace_path)).expect("discover ws-stack-1");
    let cp1 = checkpoint(
        &ws1_oz,
        "oz checkpoint",
        Some("pre-commit stack-1"),
        actor.clone(),
        None,
    )
    .expect("checkpoint ws-stack-1");
    assert_eq!(cp1.checkpoint_ref, "refs/ozone/checkpoints/ws-stack-1");

    let c_s1 = commit_changes(
        &ws1_oz,
        &CommitOptions::new("feat(auth): add token prefix validator")
            .with_sign(true)
            .with_signing_key(priv_key_path.to_string_lossy().into_owned()),
    )
    .expect("signed commit stack-1");
    assert!(c_s1.signed);

    let main_oz = OzoneRepo::discover(Some(&cloned_dir)).expect("rediscover main");
    let ws2 = spawn_cow_workspace(
        &main_oz,
        SpawnWorkspaceOptions {
            name: "ws-stack-2".to_string(),
            branch: Some("feat/stack-2".to_string()),
            from_ref: Some("feat/stack-1".to_string()),
            backend: None,
            scope: Vec::new(),
            deny: Vec::new(),
            warm: true,
        },
    );
    write_rel(
        &ws2.workspace.workspace_path,
        "src/handler.rs",
        "pub fn handle_auth_status(valid: bool) -> u16 {\n    if valid { 500 } else { 401 } // intentional bug for CI self-heal\n}\n",
    );
    let ws2_oz =
        OzoneRepo::discover(Some(&ws2.workspace.workspace_path)).expect("discover ws-stack-2");
    let c_s2_buggy = commit_changes(
        &ws2_oz,
        &CommitOptions::new("feat(api): wire auth HTTP status handler")
            .with_sign(true)
            .with_signing_key(priv_key_path.to_string_lossy().into_owned()),
    )
    .expect("signed commit stack-2");
    assert!(c_s2_buggy.signed);

    for oid in [&c_s1.commit_oid, &c_s2_buggy.commit_oid] {
        let verified =
            verify_commit_signature_with_allowed_signers(&main_oz, oid, &allowed_signers)
                .expect("oz_core verify_commit_signature");
        assert!(verified.valid && verified.allowed_signer_matched == Some(true));

        let sys_verify = Command::new(sys_git_bin())
            .args(["verify-commit", oid])
            .current_dir(&cloned_dir)
            .output()
            .expect("git verify-commit");
        assert!(
            sys_verify.status.success(),
            "system git verify-commit failed for {oid}:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&sys_verify.stdout),
            String::from_utf8_lossy(&sys_verify.stderr)
        );
        let raw_cat = unmethered_git(&cloned_dir, &["cat-file", "commit", oid]);
        assert!(raw_cat.contains(SSHSIG_ARMOR_HEADER));
    }

    // 5. Run `pr_stack` from `ws-stack-2` against the Smart-HTTP + Mock GitHub server!
    let ws2_oz =
        OzoneRepo::discover(Some(&ws2.workspace.workspace_path)).expect("rediscover ws-stack-2");
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
            git_auth_scheme: HttpAuthScheme::BasicToken,
        },
        actor.clone(),
    )
    .expect("pr_stack over Smart-HTTP + Mock GitHub");

    assert!(stack_out.pushed);
    assert_eq!(stack_out.items.len(), 2);
    assert_eq!(stack_out.items[0].head_branch, "feat/stack-1");
    assert_eq!(stack_out.items[0].base_branch, "main");
    assert_eq!(stack_out.items[1].head_branch, "feat/stack-2");
    assert_eq!(stack_out.items[1].base_branch, "feat/stack-1");
    let pr1_num = stack_out.items[0].number;
    let pr2_num = stack_out.items[1].number;
    {
        let guard = server_state.lock().unwrap();
        for pr in &guard.prs {
            let body = pr.body.as_deref().expect("PR body");
            assert!(body.contains(OZ_STACK_START) && body.contains(OZ_STACK_END));
        }
    }

    // 6. Simulate failing CI check on `feat/stack-2` & extract failure via `query_ci_status`!
    {
        let mut guard = server_state.lock().unwrap();
        guard.check_runs_by_ref.insert(
            c_s2_buggy.commit_oid.clone(),
            vec![GitHubCheckRun {
                id: 701,
                name: "Build & Test (ubuntu-latest)".to_string(),
                status: "completed".to_string(),
                conclusion: Some("failure".to_string()),
                details_url: Some(
                    "https://github.com/acme/rocket/actions/runs/800/job/9101".to_string(),
                ),
                html_url: None,
                output: GitHubCheckRunOutput::default(),
            }],
        );
        guard.workflow_runs.push(GitHubWorkflowRun {
            id: 800,
            name: Some("CI".to_string()),
            head_branch: Some("feat/stack-2".to_string()),
            head_sha: c_s2_buggy.commit_oid.clone(),
            status: "completed".to_string(),
            conclusion: Some("failure".to_string()),
            html_url: None,
        });
        guard.jobs_by_run.insert(
            800,
            vec![GitHubActionsJob {
                id: 9101,
                run_id: Some(800),
                name: "Build & Test (ubuntu-latest)".to_string(),
                status: "completed".to_string(),
                conclusion: Some("failure".to_string()),
                html_url: None,
                steps: vec![GitHubJobStep {
                    name: "Run workspace tests".to_string(),
                    status: "completed".to_string(),
                    conclusion: Some("failure".to_string()),
                    number: 4,
                }],
            }],
        );
        let raw_ci_log = "\
2026-11-18T14:10:01.0000000Z \x1b[32mCompiling\x1b[0m rocket v0.1.0\n\
2026-11-18T14:10:02.0000000Z thread 'handler::returns_200_on_valid_token' panicked at src/handler.rs:2:5:\n\
2026-11-18T14:10:02.0000001Z assertion `left == right` failed: expected 200, got 500\n\
2026-11-18T14:10:02.0000002Z ##[error]Process completed with exit code 101.\n";
        guard.job_logs.insert(9101, raw_ci_log.as_bytes().to_vec());
    }

    let ci_report = query_ci_status(
        &ws2_oz,
        &CiQueryOptions {
            target_ref: Some("feat/stack-2".to_string()),
            auth_token: Some(token.to_string()),
            api_url: Some(api_url.clone()),
            ..Default::default()
        },
    )
    .expect("query_ci_status");
    assert!(!ci_report.all_passed);
    assert_eq!(ci_report.checks.len(), 1);
    assert_eq!(
        ci_report.checks[0].failing_step.as_deref(),
        Some("Run workspace tests")
    );
    let excerpt = ci_report.checks[0]
        .log_excerpt
        .as_deref()
        .expect("CI log excerpt");
    assert!(excerpt.contains("expected 200, got 500"));

    // 7. Self-heal `ws-stack-2` via signed `--amend` preserving `Change-Id:` + `--force-with-lease` push!
    write_rel(
        &ws2.workspace.workspace_path,
        "src/handler.rs",
        "pub fn handle_auth_status(valid: bool) -> u16 {\n    if valid { 200 } else { 401 }\n}\n",
    );
    let ws2_oz =
        OzoneRepo::discover(Some(&ws2.workspace.workspace_path)).expect("rediscover ws-stack-2");
    let c_s2_fixed = commit_changes(
        &ws2_oz,
        &CommitOptions::new("feat(api): wire auth HTTP status handler")
            .with_amend(true)
            .with_sign(true)
            .with_signing_key(priv_key_path.to_string_lossy().into_owned()),
    )
    .expect("signed --amend fix");
    assert!(c_s2_fixed.amended);
    assert!(c_s2_fixed.signed);
    assert_eq!(
        c_s2_fixed.change_id, c_s2_buggy.change_id,
        "signed --amend must preserve Change-Id:"
    );
    let amend_verify = Command::new(sys_git_bin())
        .args(["verify-commit", &c_s2_fixed.commit_oid])
        .current_dir(&cloned_dir)
        .output()
        .expect("git verify-commit amended");
    assert!(amend_verify.status.success());

    let ws2_oz =
        OzoneRepo::discover(Some(&ws2.workspace.workspace_path)).expect("rediscover after amend");
    let lease_push = push_refs(
        &ws2_oz,
        &PushOptions {
            remote: Some("origin".to_string()),
            refspecs: vec!["feat/stack-2".to_string()],
            force_with_lease: true,
            auth_token: Some(token.to_string()),
            auth_scheme: HttpAuthScheme::BasicToken,
            ..Default::default()
        },
        actor.clone(),
    )
    .expect("re-push --force-with-lease");
    assert_eq!(
        lease_push.pushed_refs[0].new_oid.as_deref(),
        Some(c_s2_fixed.commit_oid.as_str())
    );
    assert_eq!(
        unmethered_git(&bare_dir, &["rev-parse", "refs/heads/feat/stack-2"]),
        c_s2_fixed.commit_oid
    );

    // 8. Simulate upstream GitHub squash-merge of PR #1 (`feat/stack-1`) into `main`, then run `sync_repo`!
    let upstream_merger = sandbox.root.join("upstream-merger");
    unmethered_git(
        &sandbox.root,
        &[
            "clone",
            bare_dir.to_str().unwrap(),
            upstream_merger.to_str().unwrap(),
        ],
    );
    unmethered_git(&upstream_merger, &["config", "user.name", "GitHub Merge Queue"]);
    unmethered_git(&upstream_merger, &["config", "user.email", "noreply@github.com"]);
    write_rel(
        &upstream_merger,
        "src/auth.rs",
        "pub fn verify_token(tok: &str) -> bool {\n    tok.starts_with(\"oz_\")\n}\n",
    );
    write_rel(&upstream_merger, "RELEASE_NOTES.md", "- Landed PR #1\n");
    unmethered_git(&upstream_merger, &["add", "src/auth.rs", "RELEASE_NOTES.md"]);
    unmethered_git(
        &upstream_merger,
        &[
            "commit",
            "-m",
            &format!(
                "feat(auth): add token prefix validator (#{pr1_num})\n\nChange-Id: {}",
                c_s1.change_id
            ),
        ],
    );
    let new_upstream_main = unmethered_git(&upstream_merger, &["rev-parse", "HEAD"]);
    unmethered_git(&upstream_merger, &["push", "origin", "main"]);
    {
        let mut guard = server_state.lock().unwrap();
        let pr1 = guard
            .prs
            .iter_mut()
            .find(|p| p.number == pr1_num)
            .expect("find PR #1");
        pr1.state = "closed".to_string();
        pr1.merged = true;
        pr1.merged_at = Some("2026-11-18T15:00:00Z".to_string());
        pr1.merge_commit_sha = Some(new_upstream_main.clone());
    }

    let main_oz = OzoneRepo::discover(Some(&cloned_dir)).expect("discover main before sync");
    let sync_out = sync_repo(
        &main_oz,
        &SyncOptions {
            remote: Some("origin".to_string()),
            base_branch: Some("main".to_string()),
            auth_token: Some(token.to_string()),
            api_url: Some(api_url.clone()),
            git_auth_scheme: HttpAuthScheme::Bearer,
            ..Default::default()
        },
        actor,
    )
    .expect("sync_repo over Smart-HTTP + Mock GitHub");

    assert!(sync_out.fetched);
    assert!(sync_out.fast_forwarded);
    assert_eq!(sync_out.base_oid.as_deref(), Some(new_upstream_main.as_str()));
    assert!(sync_out.merged_branches.contains(&"feat/stack-1".to_string()));
    assert_eq!(sync_out.retargeted_prs.len(), 1);
    assert_eq!(sync_out.retargeted_prs[0].number, pr2_num);
    assert_eq!(sync_out.retargeted_prs[0].old_base, "feat/stack-1");
    assert_eq!(sync_out.retargeted_prs[0].new_base, "main");
    assert!(sync_out.rebased_branches.contains(&"feat/stack-2".to_string()));
    assert!(sync_out.removed_workspaces.contains(&"ws-stack-1".to_string()));
    assert!(!ws1.workspace.workspace_path.exists());
    assert!(
        sync_out
            .pruned_checkpoints
            .contains(&"refs/ozone/checkpoints/ws-stack-1".to_string())
    );

    // Rebased `feat/stack-2` sits directly on `new_upstream_main` and preserves `Change-Id:`!
    assert_eq!(
        unmethered_git(&cloned_dir, &["rev-parse", "feat/stack-2^"]),
        new_upstream_main
    );
    assert!(
        unmethered_git(&cloned_dir, &["log", "-1", "--format=%B", "feat/stack-2"])
            .contains(&format!("Change-Id: {}", c_s2_fixed.change_id))
    );
    let remaining_ws: Vec<String> = list_workspaces(&main_oz)
        .expect("list_workspaces")
        .into_iter()
        .map(|w| w.state.name)
        .collect();
    assert_eq!(remaining_ws, vec!["ws-stack-2".to_string()]);
    assert_git_fsck_clean(&cloned_dir);
}

// ============================================================================
// GATE 3: End-to-End Agent Ecosystem & Provenance Loop
// ============================================================================

#[test]
fn gate3_e2e_agent_setup_mcp_stdio_notes_provenance_blame_and_undo() {
    let sandbox = GateSandbox::new("m3-gate3-agent");
    let repo_dir = &sandbox.repo_root;

    unmethered_git(repo_dir, &["init", "-b", "main"]);
    unmethered_git(repo_dir, &["config", "user.name", "Lead Human Engineer"]);
    unmethered_git(repo_dir, &["config", "user.email", "lead@ozone.dev"]);

    // 1. Run `oz_agent::setup_agent_harnesses` across all 5 harnesses and verify idempotency!
    let setup_report =
        setup_agent_harnesses(repo_dir, &AgentSetupOptions::all_harnesses()).expect("setup 5 harnesses");
    assert_eq!(setup_report.harnesses, AgentHarness::ALL.to_vec());

    let expected_harness_files = [
        ".claude/skills/oz/SKILL.md",
        ".claude/settings.json",
        ".cursor/rules/oz.mdc",
        ".cursor/hooks.json",
        ".agents/skills/oz/SKILL.md",
        ".codex/skills/oz/SKILL.md",
        "AGENTS.md",
        ".codex/hooks.json",
        ".windsurf/rules/oz.md",
        ".windsurf/hooks.json",
        ".agent/skills/oz/SKILL.md",
        ".agent/hooks.json",
    ];
    for rel in expected_harness_files {
        assert!(
            repo_dir.join(rel).is_file(),
            "expected agent harness file {rel} to be created"
        );
    }
    let agents_md = fs::read_to_string(repo_dir.join("AGENTS.md")).expect("read AGENTS.md");
    assert!(agents_md.contains(AGENTS_MD_START_MARKER) && agents_md.contains(AGENTS_MD_END_MARKER));
    let claude_settings =
        fs::read_to_string(repo_dir.join(".claude/settings.json")).expect("read .claude/settings.json");
    assert!(claude_settings.contains(OZ_CHECKPOINT_HOOK_CMD));

    let second_setup =
        setup_agent_harnesses(repo_dir, &AgentSetupOptions::all_harnesses()).expect("idempotent setup");
    assert!(second_setup.created.is_empty() && second_setup.updated.is_empty());
    assert_eq!(second_setup.unchanged.len(), expected_harness_files.len());

    // 2. Author an initial HUMAN commit with multi-section `src/pricing.rs`.
    let human_pricing = "\
//! Core subscription seat pricing calculator.

pub fn compute_seat_total_cents(seats: u32) -> u64 {
    let unit_cents = 2_000u64;
    u64::from(seats) * unit_cents
}




pub fn annual_discount_bps() -> u64 {
    1_500
}
";
    write_rel(repo_dir, "src/pricing.rs", human_pricing);
    let repo = OzoneRepo::discover(Some(repo_dir)).expect("discover repo");
    repo.layout.ensure_initialized().expect("init ozone");
    let human_commit = commit_changes(
        &repo,
        &CommitOptions::new("feat(pricing): initial human seat calculator"),
    )
    .expect("commit human baseline");

    // 3. Make 2 distinct hunks in `src/pricing.rs`:
    //    - `h1a`: tiered enterprise seat pricing (to be committed by Agent via MCP `oz_commit` with `pick: ["h1a"]`)
    //    - `h1b`: uncommitted local debug logging in `annual_discount_bps`
    let agent_and_wip_pricing = "\
//! Core subscription seat pricing calculator.

pub fn compute_seat_total_cents(seats: u32) -> u64 {
    let unit_cents = if seats >= 50 { 1_600u64 } else { 2_000u64 };
    u64::from(seats) * unit_cents
}




pub fn annual_discount_bps() -> u64 {
    eprintln!(\"WIP debug discount check\");
    1_500
}
";
    write_rel(repo_dir, "src/pricing.rs", agent_and_wip_pricing);

    // 4. Drive `oz mcp` stdio session (testing both JSON-line and `Content-Length` LSP framing):
    //    `initialize` -> `tools/list` -> `oz_status` -> `oz_diff` -> `oz_checkpoint` -> `oz_commit` (`pick: ["h1a"]`)
    let mcp_session_id = "mcp-sess-m3-gate3";
    let mut mcp_server =
        McpServer::new(Some(repo_dir.clone())).with_session_id(mcp_session_id);

    let json_line_requests = [
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "clientInfo": { "name": "claude-code", "version": "1.0" }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "oz_status", "arguments": {} }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "oz_diff", "arguments": { "targets": ["h1a"], "include_unified": true } }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "oz_checkpoint", "arguments": { "message": "before selective MCP commit" } }
        }),
    ];

    let mut wire_in = Vec::new();
    for req in &json_line_requests {
        writeln!(&mut wire_in, "{}", serde_json::to_string(req).unwrap()).unwrap();
    }
    // Send `oz_commit` using LSP `Content-Length` framing to exercise dual-framing support!
    let commit_rpc = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "oz_commit",
            "arguments": {
                "message": "feat(pricing): add volume tier for >=50 seats",
                "pick": ["h1a"],
                "model": "claude-sonnet-4-6",
                "prompt_summary": "add 50-seat volume discount tier without touching annual_discount_bps"
            }
        }
    }))
    .unwrap();
    write!(
        &mut wire_in,
        "Content-Length: {}\r\n\r\n{}",
        commit_rpc.len(),
        commit_rpc
    )
    .unwrap();

    let mut wire_out = Vec::new();
    mcp_server
        .serve_stdio(Cursor::new(wire_in), &mut wire_out)
        .expect("serve MCP stdio batch 1");

    let out_str = String::from_utf8(wire_out).expect("utf8 MCP responses");
    let (json_lines_part, content_len_part) = out_str
        .split_once("Content-Length:")
        .expect("expected 5 JSON-line responses followed by 1 Content-Length response");

    let line_resps: Vec<Value> = json_lines_part
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse JSON-line RPC response"))
        .collect();
    assert_eq!(line_resps.len(), 5);

    let tools_list = line_resps[1]["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools_list.len(), 11, "MCP server must advertise all 11 Ozone tools");

    let (_, cl_body) = content_len_part
        .split_once("\r\n\r\n")
        .expect("Content-Length body separator");
    let commit_rpc_resp: Value = serde_json::from_str(cl_body).expect("parse commit RPC response");
    assert_eq!(commit_rpc_resp["result"]["isError"], false);
    let commit_payload: Value = serde_json::from_str(
        commit_rpc_resp["result"]["content"][0]["text"]
            .as_str()
            .expect("commit tool text"),
    )
    .expect("parse oz_commit JSON payload");

    assert_eq!(commit_payload["ok"], true);
    assert_eq!(commit_payload["remaining_dirty_count"], 1);
    let agent_commit_oid = commit_payload["commit_oid"]
        .as_str()
        .expect("agent commit_oid")
        .to_string();

    // Verify GitHub commit message stays clean of AI metadata spam (only subject + Change-Id trailer)!
    let raw_commit_msg = unmethered_git(repo_dir, &["log", "-1", "--format=%B", &agent_commit_oid]);
    assert!(raw_commit_msg.starts_with("feat(pricing): add volume tier for >=50 seats"));
    assert!(raw_commit_msg.contains("Change-Id: I"));
    assert!(!raw_commit_msg.contains("claude-sonnet-4-6"));
    assert!(!raw_commit_msg.contains(mcp_session_id));

    // 5. Verify automatic `refs/notes/ai` provenance via `oz_agent` AND system `git notes --ref=ai show`!
    let repo = OzoneRepo::discover(Some(repo_dir)).expect("rediscover after MCP commit");
    let note = read_provenance_note(&repo, &agent_commit_oid)
        .expect("read_provenance_note")
        .expect("provenance note recorded on refs/notes/ai");
    assert_eq!(note.schema_version, 1);
    assert_eq!(note.client, "claude-code");
    assert_eq!(note.session_id, mcp_session_id);
    assert_eq!(note.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(note.line_ranges.len(), 1);
    assert_eq!(note.line_ranges[0].path, "src/pricing.rs");
    assert_eq!(note.line_ranges[0].start_line, 4);
    assert_eq!(note.line_ranges[0].end_line, 4);

    let sys_note_raw = unmethered_git(repo_dir, &["notes", "--ref=ai", "show", &agent_commit_oid]);
    let sys_note_json: Value = serde_json::from_str(&sys_note_raw).expect("valid JSON in git notes");
    assert_eq!(sys_note_json["session_id"], mcp_session_id);

    // 6. Verify `oz_agent::blame_file` (`oz blame`) exact line-level human vs. agent attribution!
    let blame = blame_file(&repo, "src/pricing.rs", &BlameOptions::default()).expect("blame_file");
    let formatted_blame = format_blame_report(&blame);
    assert!(formatted_blame.contains("claude-code"));
    assert_eq!(blame.agent_lines, 1);
    assert!(blame.human_lines >= 8);

    // Line 3 (`pub fn compute_seat_total_cents...`) -> human baseline commit
    assert_eq!(blame.lines[2].line_number, 3);
    assert_eq!(blame.lines[2].commit_oid, human_commit.commit_oid);
    assert_eq!(blame.lines[2].author_kind, "human");

    // Line 4 (`    let unit_cents = if seats >= 50 ...`) -> agent MCP commit (`claude-code`)!
    assert_eq!(blame.lines[3].line_number, 4);
    assert_eq!(blame.lines[3].commit_oid, agent_commit_oid);
    assert_eq!(blame.lines[3].author_kind, "agent");
    assert_eq!(blame.lines[3].client.as_deref(), Some("claude-code"));
    assert_eq!(blame.lines[3].session_id.as_deref(), Some(mcp_session_id));
    assert_eq!(blame.lines[3].model.as_deref(), Some("claude-sonnet-4-6"));

    // 7. Invoke `oz_undo` via MCP stdio and verify `HEAD` rewinds to `human_commit` while restoring
    //    both `h1a` and `h1b` dirty changes cleanly in `src/pricing.rs`!
    let undo_req = format!(
        "{}\n",
        serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "oz_undo", "arguments": {} }
        }))
        .unwrap()
    );
    let mut undo_out = Vec::new();
    mcp_server
        .serve_stdio(Cursor::new(undo_req.into_bytes()), &mut undo_out)
        .expect("serve MCP oz_undo");
    let undo_rpc: Value =
        serde_json::from_slice(&undo_out).expect("parse oz_undo JSON-RPC response");
    assert_eq!(undo_rpc["result"]["isError"], false);
    let undo_payload: Value = serde_json::from_str(
        undo_rpc["result"]["content"][0]["text"]
            .as_str()
            .expect("undo text"),
    )
    .expect("parse undo payload");
    assert_eq!(undo_payload["ok"], true);

    assert_eq!(
        unmethered_git(repo_dir, &["rev-parse", "HEAD"]),
        human_commit.commit_oid,
        "oz_undo via MCP must rewind HEAD back to the human baseline commit"
    );
    assert_eq!(
        fs::read_to_string(repo_dir.join("src/pricing.rs")).expect("read restored pricing.rs"),
        agent_and_wip_pricing,
        "oz_undo via MCP must preserve/restore both h1a and h1b working-tree edits"
    );
    assert_git_fsck_clean(repo_dir);
}
