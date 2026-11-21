// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Milestone 4 (`v1.0.0`) Exit-Gate Suite:
//!
//! 1. **Gate 1 — `v1.0.0` Version Consistency & `cargo-binstall` Contract**:
//!    - Root `Cargo.toml`, `Cargo.lock` (all 10 workspace crates), `oz --version --json`,
//!      `git-ozone --version --json`, and `oz-github` User-Agent derivation all match `1.0.0`.
//!    - `crates/oz-cli/Cargo.toml` defines `[package.metadata.binstall]` matching the standard
//!      GitHub Release archive contract (`oz-v{ version }-{ target }{ archive-suffix }`).
//!
//! 2. **Gate 2 — Pure-Rust `Cargo.lock` Guard**:
//!    - Zero forbidden C bindings (`openssl-sys`, `libgit2-sys`, `curl-sys`, `fuse-sys`,
//!      `libfuse`, `libssh2-sys`).
//!
//! 3. **Gate 3 — `oz doctor --json` M4 Completeness & Zero Secret Leakage**:
//!    - Verifies all M4 diagnostic sections (`filesystem`, OS platform readiness, `oz_vfsd`
//!      discovery & daemon health, `git_ozone_alias`, `ssh_signing`, `github_auth`, `repo` &
//!      `active_mounts`, structured `checks`, and `remediation_hints`).
//!    - Verifies strict zero secret leakage for GitHub tokens, remote URL credentials, and
//!      inline SSH private keys.
//!
//! 4. **Gate 4 — CLI Edge-Case Hardening, Exit Codes (`0`, `1`, `2`), Non-TTY & `oz.v1` Invariants**:
//!    - Non-repo `oz doctor --json`, `--help`, invalid flags (exit `2`), missing subcommand
//!      (exit `2`), nonexistent `-C` dir (exit `2`), non-TTY `oz commit` without `-m` (exit `2`),
//!      and real merge conflict returning exit `1` followed by clean `oz resolve --continue`.
//!
//! 5. **Gate 5 — End-to-End Core Workflow Smoke**:
//!    - `setup` -> `ws spawn` -> edit -> `status` -> `diff` -> `commit` -> `checkpoint` ->
//!      `undo`/`redo` -> `radar` -> `integrate` -> `ws rm` -> `git fsck --full`.

use oz_core::{MountRecord, OZ_SCHEMA_V1};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tempfile::TempDir;

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
        let sibling = oz.with_file_name(oz_vfsd::vfsd_binary_name());
        if sibling.is_file() {
            return sibling;
        }

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

struct CmdResult {
    code: i32,
    stdout: String,
    stderr: String,
    json: Option<Value>,
}

fn run_bin(bin: &Path, cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> CmdResult {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .env("OZ_VFSD_BIN", ensure_oz_vfsd_bin().as_os_str())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("execute binary");
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let json = serde_json::from_str::<Value>(&stdout).ok();
    CmdResult {
        code,
        stdout,
        stderr,
        json,
    }
}

fn run_bin_clean_auth(bin: &Path, cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> CmdResult {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .env("OZ_VFSD_BIN", ensure_oz_vfsd_bin().as_os_str())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env_remove("OZ_GITHUB_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .env_remove("GH_CONFIG_DIR")
        .stdin(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("execute binary");
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let json = serde_json::from_str::<Value>(&stdout).ok();
    CmdResult {
        code,
        stdout,
        stderr,
        json,
    }
}

fn sys_git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new(sys_git_bin())
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
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

fn init_git_repo(repo_dir: &Path) -> String {
    fs::create_dir_all(repo_dir).expect("mkdir repo_dir");
    sys_git(repo_dir, &["init", "-b", "main"]);
    sys_git(repo_dir, &["config", "user.name", "Ozone M4 Gate"]);
    sys_git(repo_dir, &["config", "user.email", "m4@ozone.test"]);
    fs::write(
        repo_dir.join("README.md"),
        "# Ozone v1.0.0 Exit Gate Repo\n",
    )
    .expect("write readme");
    fs::write(
        repo_dir.join("lib.rs"),
        "pub fn greet() -> &'static str {\n    \"hello\"\n}\n",
    )
    .expect("write lib.rs");
    sys_git(repo_dir, &["add", "."]);
    sys_git(repo_dir, &["commit", "-m", "chore: initial commit"]);
    sys_git(repo_dir, &["rev-parse", "HEAD"])
}

/// Cross-platform RAII sandbox ensuring any `oz-vfsd` daemons and FUSE3 mounts
/// created during tests are cleanly shut down and unmounted on drop.
struct M4Sandbox {
    root: PathBuf,
    repo_root: PathBuf,
    _tempdir: TempDir,
}

impl M4Sandbox {
    fn new(repo_name: &str) -> Self {
        let _ = ensure_oz_vfsd_bin();
        let preferred_base: Option<PathBuf> = option_env!("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .filter(|p| p.as_os_str().len() <= 60);

        let tempdir = match preferred_base {
            Some(ref base) => {
                fs::create_dir_all(base).expect("create short sandbox base");
                tempfile::Builder::new()
                    .prefix("oz-m4-")
                    .tempdir_in(base)
                    .expect("tempdir in base")
            }
            None => tempfile::Builder::new()
                .prefix("oz-m4-")
                .tempdir()
                .expect("tempdir"),
        };
        let root = tempdir.path().to_path_buf();
        let repo_root = root.join(repo_name);
        fs::create_dir_all(&repo_root).expect("mkdir repo_root");
        Self {
            root,
            repo_root,
            _tempdir: tempdir,
        }
    }
}

impl Drop for M4Sandbox {
    fn drop(&mut self) {
        let git_dir = self.repo_root.join(".git");
        let mut mounts_to_clean: Vec<PathBuf> = Vec::new();
        let mut pids_to_kill: Vec<u32> = Vec::new();

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

        if git_dir.is_dir() {
            let _ = oz_vfsd::shutdown_daemon(&git_dir);
        }

        #[cfg(target_os = "linux")]
        {
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
            let root_marker = self.root.to_string_lossy().into_owned();
            for pid in pids_to_kill {
                let cmdline_path = format!("/proc/{pid}/cmdline");
                if let Ok(bytes) = fs::read(&cmdline_path) {
                    let cmdline = String::from_utf8_lossy(&bytes);
                    if cmdline.contains("oz-vfsd") && cmdline.contains(&root_marker) {
                        unsafe {
                            let _ = libc::kill(pid as libc::pid_t, libc::SIGKILL);
                        }
                        let deadline = Instant::now() + Duration::from_secs(2);
                        let proc_dir = PathBuf::from(format!("/proc/{pid}"));
                        while Instant::now() < deadline && proc_dir.exists() {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                }
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

fn spawn_ws_or_cas_fallback(repo: &Path, ws_name: &str) -> CmdResult {
    let first = run_bin(
        &oz_bin(),
        repo,
        &["ws", "spawn", ws_name, "--no-warm", "--json"],
        &[],
    );
    if first.code == 0 {
        return first;
    }
    let msg = first
        .json
        .as_ref()
        .and_then(|v| v["error"]["message"].as_str())
        .unwrap_or("");
    if msg.contains("Operation not permitted") || msg.contains("SUN_LEN") {
        let local_cfg = repo.join(".git/ozone/config.toml");
        if let Some(parent) = local_cfg.parent() {
            fs::create_dir_all(parent).expect("create .git/ozone");
        }
        fs::write(&local_cfg, "backend = \"cas_hardlink\"\n").expect("write cas_hardlink fallback");
        return run_bin(
            &oz_bin(),
            repo,
            &["ws", "spawn", ws_name, "--no-warm", "--json"],
            &[],
        );
    }
    first
}

fn assert_oz_v1_envelope_invariants(v: &Value, expected_ok: bool) {
    assert_eq!(v["schema"], OZ_SCHEMA_V1, "envelope schema must be oz.v1");
    assert_eq!(v["ok"], expected_ok, "envelope ok mismatch: {v}");
    assert!(v.get("op_id").is_some(), "envelope must include op_id key");
    assert!(
        v.get("workspace").is_some(),
        "envelope must include workspace key"
    );
    assert!(v.get("branch").is_some(), "envelope must include branch key");
    assert!(
        v.get("head_oid").is_some(),
        "envelope must include head_oid key"
    );
    assert!(v.get("data").is_some(), "envelope must include data key");
    assert!(v["warnings"].is_array(), "warnings must be array");
    assert!(v["conflicts"].is_array(), "conflicts must be array");
    assert!(
        v["recovery_commands"].is_array(),
        "recovery_commands must be array"
    );
    if expected_ok {
        assert!(
            v.get("error").is_none(),
            "successful envelope must omit error"
        );
    } else {
        assert!(v["error"].is_object(), "failing envelope must include error");
        assert!(v["error"]["code"].is_string());
        assert!(v["error"]["exit_code"].is_i64());
        assert!(v["error"]["message"].is_string());
    }
}

// ============================================================================
// GATE 1: v1.0.0 Version Consistency & cargo-binstall Contract
// ============================================================================

#[test]
fn gate1_v1_0_0_version_consistency_and_binstall_contract() {
    assert_eq!(
        env!("CARGO_PKG_VERSION"),
        "1.0.0",
        "workspace package version must be 1.0.0 for Milestone 4 public launch"
    );

    let root = workspace_root_dir();

    // 1. Root Cargo.toml workspace version & repository URL.
    let root_toml = fs::read_to_string(root.join("Cargo.toml")).expect("read Cargo.toml");
    assert!(
        root_toml.contains("version = \"1.0.0\""),
        "root Cargo.toml must declare version = \"1.0.0\""
    );
    assert!(
        root_toml.contains("repository = \"https://github.com/Gaalix/git-ozone\""),
        "root Cargo.toml must declare canonical GitHub repository"
    );

    // 2. crates/oz-cli/Cargo.toml binstall metadata matching GitHub Release naming.
    let cli_toml =
        fs::read_to_string(root.join("crates/oz-cli/Cargo.toml")).expect("read oz-cli Cargo.toml");
    assert!(
        cli_toml.contains("[package.metadata.binstall]"),
        "crates/oz-cli/Cargo.toml must declare [package.metadata.binstall]"
    );
    assert!(
        cli_toml.contains(
            "pkg-url = \"{ repo }/releases/download/v{ version }/oz-v{ version }-{ target }{ archive-suffix }\""
        ),
        "binstall pkg-url must match standard GitHub Release archive naming"
    );
    assert!(
        cli_toml.contains("bin-dir = \"oz-v{ version }-{ target }/{ bin }{ binary-ext }\""),
        "binstall bin-dir must match release tarball inner directory layout"
    );
    assert!(
        cli_toml.contains("pkg-fmt = \"tgz\""),
        "binstall default pkg-fmt must be tgz"
    );
    assert!(
        cli_toml.contains("[package.metadata.binstall.overrides.x86_64-pc-windows-msvc]")
            && cli_toml.contains("pkg-fmt = \"zip\""),
        "binstall must override Windows target to pkg-fmt = \"zip\""
    );

    // 3. Cargo.lock synced to 1.0.0 across all 10 workspace crates.
    let cargo_lock = fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");
    for crate_name in [
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
    ] {
        let stanza = format!("name = \"{crate_name}\"\nversion = \"1.0.0\"");
        assert!(
            cargo_lock.contains(&stanza),
            "Cargo.lock missing synced v1.0.0 entry for {crate_name}"
        );
    }

    // 4. oz-github User-Agent constants derive from CARGO_PKG_VERSION (no hardcoded 0.3.0).
    for rel in ["crates/oz-github/src/wire.rs", "crates/oz-github/src/api.rs"] {
        let src = fs::read_to_string(root.join(rel)).expect("read oz-github source");
        assert!(
            src.contains("env!(\"CARGO_PKG_VERSION\")"),
            "{rel} must derive User-Agent from env!(\"CARGO_PKG_VERSION\")"
        );
        assert!(
            !src.contains("0.3.0"),
            "{rel} must not hardcode legacy version 0.3.0"
        );
    }

    // 5. CLI binary version flags (`oz` and `git-ozone`) emit v1.0.0.
    let tmp = TempDir::new().expect("tempdir");
    for (bin, expected_invoked) in [(oz_bin(), "oz"), (git_ozone_bin(), "git-ozone")] {
        let res = run_bin(&bin, tmp.path(), &["--version", "--json"], &[]);
        assert_eq!(res.code, 0, "stderr: {}", res.stderr);
        let v = res.json.expect("parse version json");
        assert_oz_v1_envelope_invariants(&v, true);
        assert_eq!(v["data"]["name"], "oz");
        assert_eq!(v["data"]["invoked_as"], expected_invoked);
        assert_eq!(v["data"]["version"], "1.0.0");
        assert_eq!(v["data"]["pure_rust"], true);
        assert_eq!(v["data"]["non_interactive"], true);

        let human = run_bin(&bin, tmp.path(), &["--version"], &[]);
        assert_eq!(human.code, 0);
        assert_eq!(human.stdout.trim(), format!("{expected_invoked} 1.0.0"));
    }

    let subcmd_ver = run_bin(&oz_bin(), tmp.path(), &["version", "--json"], &[]);
    assert_eq!(subcmd_ver.code, 0);
    let sv = subcmd_ver.json.expect("version subcommand json");
    assert_oz_v1_envelope_invariants(&sv, true);
    assert_eq!(sv["data"]["version"], "1.0.0");
}

// ============================================================================
// GATE 2: Pure-Rust Cargo.lock Guard (Zero Forbidden C Bindings)
// ============================================================================

#[test]
fn gate2_pure_rust_cargo_lock_zero_c_bindings() {
    let cargo_lock =
        fs::read_to_string(workspace_root_dir().join("Cargo.lock")).expect("read Cargo.lock");
    for forbidden in [
        "name = \"openssl-sys\"",
        "name = \"libgit2-sys\"",
        "name = \"curl-sys\"",
        "name = \"fuse-sys\"",
        "name = \"libfuse\"",
        "name = \"libssh2-sys\"",
    ] {
        assert!(
            !cargo_lock.contains(forbidden),
            "Cargo.lock contains forbidden C binding: {forbidden}"
        );
    }
}

// ============================================================================
// GATE 3: `oz doctor --json` M4 Completeness & Zero Secret Leakage
// ============================================================================

#[test]
fn gate3_doctor_json_m4_completeness_and_zero_secret_leakage() {
    let sandbox = M4Sandbox::new("doc-app");
    let repo = sandbox.repo_root.clone();
    let head_oid = init_git_repo(&repo);

    // Configure real Ed25519 SSH signing key via git config.
    let key_path = sandbox.root.join("m4_doctor_ed25519");
    let keygen = Command::new(ssh_keygen_bin())
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "m4-doctor@ozone.test",
            "-f",
            key_path.to_str().expect("utf8 key path"),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run ssh-keygen");
    assert!(keygen.status.success(), "ssh-keygen failed");

    sys_git(&repo, &["config", "gpg.format", "ssh"]);
    sys_git(
        &repo,
        &[
            "config",
            "user.signingkey",
            &key_path.display().to_string().replace('\\', "/"),
        ],
    );
    sys_git(&repo, &["config", "commit.gpgsign", "true"]);

    // Remote URL with embedded credentials to prove sanitization.
    let secret_remote_pass = "top_secret_remote_pass_999888";
    let remote_with_creds = format!(
        "https://botuser:{secret_remote_pass}@github.com/ozone-vcs/ozone.git"
    );
    sys_git(&repo, &["remote", "add", "origin", &remote_with_creds]);

    // Spawn a real workspace so active_mounts and oz-vfsd health are exercised live.
    let spawn = spawn_ws_or_cas_fallback(&repo, "doc-ws");
    assert_eq!(spawn.code, 0, "ws spawn failed: {}\n{}", spawn.stdout, spawn.stderr);

    let secret_token = "ghp_m4_super_secret_token_do_not_leak_123456";
    let doc = run_bin_clean_auth(
        &git_ozone_bin(),
        &repo,
        &["doctor", "--json"],
        &[("GITHUB_TOKEN", secret_token)],
    );
    assert_eq!(doc.code, 0, "doctor failed: {}", doc.stderr);

    // Zero secret leakage across complete stdout and stderr!
    let combined = format!("{}\n{}", doc.stdout, doc.stderr);
    assert!(
        !combined.contains(secret_token),
        "raw GitHub token leaked in oz doctor output!"
    );
    assert!(
        !combined.contains("m4_super_secret_token_do_not_leak"),
        "GitHub token secret body leaked in oz doctor output!"
    );
    assert!(
        !combined.contains(secret_remote_pass),
        "remote URL credential password leaked in oz doctor output!"
    );
    assert!(
        !combined.contains("BEGIN OPENSSH PRIVATE KEY"),
        "private SSH key PEM leaked in oz doctor output!"
    );

    let dj = doc.json.expect("parse doctor json");
    assert_oz_v1_envelope_invariants(&dj, true);
    assert_eq!(dj["branch"], "main");
    assert_eq!(dj["head_oid"], head_oid);

    let data = &dj["data"];
    assert_eq!(data["version"], "1.0.0");
    assert_eq!(data["invoked_as"], "git-ozone");
    assert_eq!(data["non_interactive"], true);

    // Filesystem section.
    assert!(data["filesystem"]["type"].is_string());
    assert!(data["filesystem"]["reflink_ready"].is_boolean());
    assert!(data["filesystem"]["apfs_clonefile_ready"].is_boolean());
    assert!(data["filesystem"]["dev_drive_refs"].is_boolean());

    // Platform-specific readiness fields.
    #[cfg(target_os = "linux")]
    {
        assert!(data["fuse"].is_object());
        assert!(data["fuse"]["dev_fuse_exists"].is_boolean());
        assert!(data["fuse"]["dev_fuse_rw"].is_boolean());
        assert!(data["fuse"]["ready"].is_boolean());
        assert!(data["projfs_ready"].is_null());
        assert!(data["windows"].is_null());
        assert!(data["macos"].is_null());
    }
    #[cfg(target_os = "windows")]
    {
        assert!(data["projfs_ready"].is_boolean());
        assert!(data["windows"].is_object());
        assert_eq!(data["windows"]["projfs_feature"], "Client-ProjFS");
        assert!(data["windows"]["projfs_dll_path"].is_string());
        assert!(data["windows"]["dev_drive_detected"].is_boolean());
        assert!(data["windows"]["refs_clone_ready"].is_boolean());
    }
    #[cfg(target_os = "macos")]
    {
        assert!(data["macos"].is_object());
        assert!(data["macos"]["apfs_detected"].is_boolean());
        assert!(data["macos"]["clonefile_ready"].is_boolean());
    }

    // oz-vfsd discovery & daemon health.
    assert_eq!(data["oz_vfsd"]["found"], true);
    assert!(data["oz_vfsd"]["path"].is_string());
    assert!(data["oz_vfsd"]["daemon_required"].is_boolean());
    assert!(data["oz_vfsd"]["daemon_running"].is_boolean());
    assert!(data["oz_vfsd"]["daemon_health"].is_string());

    // git-ozone alias section.
    assert_eq!(data["git_ozone_alias"]["invoked_as"], "git-ozone");
    assert_eq!(data["git_ozone_alias"]["resolved"], true);

    // SSH signing section.
    assert_eq!(data["ssh_signing"]["enabled_by_default"], true);
    assert_eq!(data["ssh_signing"]["ready"], true);
    assert_eq!(data["ssh_signing"]["key_algorithm"], "ssh-ed25519");
    assert_eq!(data["ssh_signing"]["git_gpg_format"], "ssh");
    assert!(
        data["ssh_signing"]["fingerprint"]
            .as_str()
            .is_some_and(|fp| fp.starts_with("SHA256:"))
    );

    // GitHub auth section.
    assert_eq!(data["github_auth"]["authenticated"], true);
    assert_eq!(data["github_auth"]["host"], "github.com");
    assert_eq!(data["github_auth"]["owner"], "ozone-vcs");
    assert_eq!(data["github_auth"]["repo"], "ozone");
    assert_eq!(data["github_auth"]["token_source"]["kind"], "github_token_env");
    let masked = data["github_auth"]["masked_token"].as_str().expect("masked token");
    assert!(masked.ends_with("****"));
    assert!(!masked.contains("m4_super_secret"));

    // Repo & active mounts section.
    assert_eq!(data["repo"]["in_repo"], true);
    assert_eq!(data["repo"]["colocated"], true);
    assert_eq!(data["repo"]["ozone_initialized"], true);
    assert!(data["repo"]["active_mounts"].is_array());
    assert!(data["repo"]["active_mount_count"].as_u64().is_some());
    assert_eq!(data["repo"]["unhealthy_mount_count"], 0);

    // Structured M4 checks list covers all 8 required domains.
    let checks = data["checks"].as_array().expect("checks array");
    let ids: BTreeSet<&str> = checks
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    for required_id in [
        "linux_fuse",
        "windows_projfs_dev_drive",
        "macos_apfs_clonefile",
        "oz_vfsd",
        "git_ozone_alias",
        "ssh_signing",
        "github_auth",
        "repository_mounts",
    ] {
        assert!(
            ids.contains(required_id),
            "oz doctor checks missing required diagnostic '{required_id}': {ids:?}"
        );
    }
    assert!(data["remediation_hints"].is_array());

    // Verify active_mounts & stopped_with_active_mounts diagnostics even on non-FUSE sandboxes.
    let mount_dir = repo.join(".git/ozone/ws/doc-mount-check");
    fs::create_dir_all(&mount_dir).expect("mkdir ws mount_dir");
    let fake_mp = sandbox.root.join("doc-mount-point");
    fs::create_dir_all(&fake_mp).expect("mkdir fake_mp");
    let rec = MountRecord {
        schema_version: MountRecord::CURRENT_SCHEMA_VERSION,
        workspace: "doc-mount-check".to_string(),
        branch: "feat/doc-mount".to_string(),
        backend: oz_core::BackendKind::LinuxFuse3,
        repo_root: repo.clone(),
        git_dir: repo.join(".git"),
        mount_point: fake_mp,
        upper_dir: Some(mount_dir.join("upper")),
        meta_dir: Some(mount_dir.join("meta")),
        base_tree: None,
        base_commit: Some(head_oid),
        socket_path: Some(repo.join(".git/ozone/vfsd.sock")),
        daemon_pid: None,
        mounted: true,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    fs::write(
        mount_dir.join("mount.json"),
        serde_json::to_string_pretty(&rec).expect("serialize mount record"),
    )
    .expect("write mount.json");
    let doc_mount = run_bin_clean_auth(&oz_bin(), &repo, &["doctor", "--json"], &[]);
    assert_eq!(doc_mount.code, 0, "stderr: {}", doc_mount.stderr);
    let dmj = doc_mount.json.expect("parse mount doctor json");
    assert_oz_v1_envelope_invariants(&dmj, true);
    assert!(
        dmj["data"]["repo"]["active_mount_count"]
            .as_u64()
            .is_some_and(|n| n >= 1),
        "doctor must report active registered mounts"
    );
    assert!(
        dmj["data"]["repo"]["active_mounts"]
            .as_array()
            .expect("active_mounts")
            .iter()
            .any(|m| m["workspace"] == "doc-mount-check"),
        "active_mounts must contain doc-mount-check"
    );
    let _ = fs::remove_dir_all(&mount_dir);

    // Inline private key PEM in `.ozone.toml` must NEVER leak into doctor output!
    let raw_pem = fs::read_to_string(&key_path).expect("read private key pem");
    fs::write(
        repo.join(".ozone.toml"),
        format!("[signing]\nenabled = true\nkey = '''\n{raw_pem}\n'''\n"),
    )
    .expect("write .ozone.toml with inline key");

    let doc_inline = run_bin_clean_auth(&oz_bin(), &repo, &["doctor", "--json"], &[]);
    assert_eq!(doc_inline.code, 0, "stderr: {}", doc_inline.stderr);
    assert!(
        !doc_inline.stdout.contains("BEGIN OPENSSH PRIVATE KEY"),
        "inline private key PEM leaked into oz doctor --json!"
    );
    let dj_inline = doc_inline.json.expect("parse inline doctor json");
    assert_eq!(
        dj_inline["data"]["ssh_signing"]["configured_key"],
        "<redacted-inline-private-key>"
    );

    // Human-readable `oz doctor` also succeeds cleanly without leaking secrets.
    let human = run_bin_clean_auth(
        &oz_bin(),
        &repo,
        &["doctor"],
        &[("GITHUB_TOKEN", secret_token)],
    );
    assert_eq!(human.code, 0, "human doctor failed: {}", human.stderr);
    assert!(human.stdout.contains("Ozone Doctor (oz 1.0.0)"));
    assert!(!human.stdout.contains("m4_super_secret_token_do_not_leak"));
    assert!(!human.stdout.contains("BEGIN OPENSSH PRIVATE KEY"));
}

// ============================================================================
// GATE 4: CLI Edge-Case Hardening, Exit Codes (0/1/2), Non-TTY & Envelopes
// ============================================================================

#[test]
fn gate4_cli_edge_cases_exit_codes_and_json_envelope_invariants() {
    let isolated_home = TempDir::new().expect("isolated home");
    let non_repo_dir = TempDir::new().expect("non-repo dir");

    // 1. Non-repo `oz doctor --json` succeeds (exit 0) and reports actionable hints.
    let empty_gh_dir = isolated_home.path().join("no-gh");
    let doc = run_bin_clean_auth(
        &oz_bin(),
        non_repo_dir.path(),
        &["doctor", "--json"],
        &[
            ("HOME", isolated_home.path().to_str().expect("home")),
            ("USERPROFILE", isolated_home.path().to_str().expect("home")),
            ("XDG_CONFIG_HOME", isolated_home.path().to_str().expect("xdg")),
            ("GH_CONFIG_DIR", empty_gh_dir.to_str().expect("gh")),
        ],
    );
    assert_eq!(doc.code, 0, "non-repo doctor failed: {}", doc.stderr);
    let dj = doc.json.expect("non-repo doctor json");
    assert_oz_v1_envelope_invariants(&dj, true);
    assert_eq!(dj["data"]["repo"]["in_repo"], false);
    assert_eq!(dj["data"]["repo"]["active_mount_count"], 0);
    assert!(
        !dj["data"]["remediation_hints"]
            .as_array()
            .expect("remediation_hints")
            .is_empty(),
        "unconfigured non-repo environment should surface actionable remediation hints"
    );

    // 2. `--help` returns exit 0 for `oz`, `git-ozone`, and subcommands.
    for (bin, args) in [
        (oz_bin(), vec!["--help"]),
        (git_ozone_bin(), vec!["--help"]),
        (oz_bin(), vec!["doctor", "--help"]),
        (oz_bin(), vec!["status", "--help"]),
    ] {
        let h = run_bin(&bin, non_repo_dir.path(), &args, &[]);
        assert_eq!(h.code, 0, "help {:?} failed: {}", args, h.stderr);
        assert!(!h.stdout.trim().is_empty());
    }

    // 3. Invalid flags & missing subcommand return exit 2 with valid `oz.v1` JSON envelope.
    for bad_args in [
        vec!["--not-a-real-flag", "--json"],
        vec!["--json", "--bogus"],
        vec!["--json"],
        vec!["ws", "--json"],
    ] {
        let res = run_bin(&oz_bin(), non_repo_dir.path(), &bad_args, &[]);
        assert_eq!(
            res.code, 2,
            "args {:?} must exit 2; stdout: {} stderr: {}",
            bad_args, res.stdout, res.stderr
        );
        let v = res
            .json
            .unwrap_or_else(|| panic!("args {:?} must emit valid JSON: {}", bad_args, res.stdout));
        assert_oz_v1_envelope_invariants(&v, false);
        assert_eq!(v["error"]["code"], "invalid_usage");
        assert_eq!(v["error"]["exit_code"], 2);
        assert!(!v["recovery_commands"].as_array().unwrap().is_empty());
    }

    // 4. Nonexistent `-C <DIR>` returns exit 2 with `invalid_usage` JSON envelope.
    let nonexistent = non_repo_dir.path().join("missing-subdir-xyz");
    let bad_dir = run_bin(
        &oz_bin(),
        non_repo_dir.path(),
        &["-C", nonexistent.to_str().unwrap(), "doctor", "--json"],
        &[],
    );
    assert_eq!(bad_dir.code, 2, "nonexistent -C must exit 2");
    let bdj = bad_dir.json.expect("bad -C json");
    assert_oz_v1_envelope_invariants(&bdj, false);
    assert_eq!(bdj["error"]["code"], "invalid_usage");
    assert_eq!(bdj["error"]["exit_code"], 2);

    // 5. Non-repo `oz status --json` returns exit 2 (`not_a_repository`).
    let non_repo_status = run_bin(&oz_bin(), non_repo_dir.path(), &["status", "--json"], &[]);
    assert_eq!(non_repo_status.code, 2);
    let nrs = non_repo_status.json.expect("non-repo status json");
    assert_oz_v1_envelope_invariants(&nrs, false);
    assert_eq!(nrs["error"]["code"], "not_a_repository");
    assert_eq!(nrs["error"]["exit_code"], 2);

    // 6. Non-TTY prompt suppression: `oz commit --json` without `-m` fails fast with exit 2.
    let sandbox = M4Sandbox::new("edge-repo");
    let repo = sandbox.repo_root.clone();
    init_git_repo(&repo);
    fs::write(repo.join("notes.txt"), "dirty\n").expect("write dirty file");

    let no_msg = run_bin(
        &oz_bin(),
        &repo,
        &["commit", "--agent", "--json"],
        &[("OZ_AGENT", "1")],
    );
    assert_eq!(no_msg.code, 2, "commit without -m must exit 2");
    let nmj = no_msg.json.expect("commit error json");
    assert_oz_v1_envelope_invariants(&nmj, false);
    assert_eq!(nmj["error"]["code"], "invalid_usage");
    assert_eq!(nmj["error"]["exit_code"], 2);

    // 7. Exit code 1 (`ConflictOrCheckFail`) via real merge conflict + `oz resolve --continue`.
    sys_git(&repo, &["checkout", "-b", "branch-a"]);
    fs::write(repo.join("lib.rs"), "pub fn greet() -> &'static str {\n    \"from-a\"\n}\n")
        .expect("write branch-a");
    sys_git(&repo, &["add", "lib.rs"]);
    sys_git(&repo, &["commit", "-m", "feat: branch-a greeting"]);

    sys_git(&repo, &["checkout", "main"]);
    sys_git(&repo, &["checkout", "-b", "branch-b"]);
    fs::write(repo.join("lib.rs"), "pub fn greet() -> &'static str {\n    \"from-b\"\n}\n")
        .expect("write branch-b");
    sys_git(&repo, &["add", "lib.rs"]);
    sys_git(&repo, &["commit", "-m", "feat: branch-b greeting"]);

    sys_git(&repo, &["checkout", "main"]);
    let conflict = run_bin(
        &oz_bin(),
        &repo,
        &["integrate", "branch-a", "branch-b", "--into", "main", "--json"],
        &[],
    );
    assert_eq!(
        conflict.code, 1,
        "merge conflict must exit 1; stdout: {} stderr: {}",
        conflict.stdout, conflict.stderr
    );
    let cj = conflict.json.expect("conflict json");
    assert_oz_v1_envelope_invariants(&cj, false);
    assert_eq!(cj["error"]["code"], "conflict");
    assert_eq!(cj["error"]["exit_code"], 1);
    let conflicts = cj["conflicts"].as_array().expect("conflicts array");
    assert!(!conflicts.is_empty(), "conflicts array must list c1");
    let cid = conflicts[0]["id"].as_str().expect("conflict id");

    let resolved = run_bin(
        &oz_bin(),
        &repo,
        &["resolve", cid, "--take", "ours", "--continue", "--json"],
        &[],
    );
    assert_eq!(
        resolved.code, 0,
        "resolve --continue failed: {}\n{}",
        resolved.stdout, resolved.stderr
    );
    let rj = resolved.json.expect("resolve json");
    assert_oz_v1_envelope_invariants(&rj, true);
}

// ============================================================================
// GATE 5: End-to-End Core Workflow Smoke (`v1.0.0`)
// ============================================================================

#[test]
fn gate5_end_to_end_core_workflow_smoke() {
    let sandbox = M4Sandbox::new("smoke-repo");
    let repo = sandbox.repo_root.clone();
    init_git_repo(&repo);

    // 1. Setup colocated `.git/ozone/`.
    let setup = run_bin(&oz_bin(), &repo, &["setup", "--json"], &[]);
    assert_eq!(setup.code, 0, "setup failed: {}", setup.stderr);
    let sj = setup.json.expect("setup json");
    assert_oz_v1_envelope_invariants(&sj, true);

    // 2. Spawn CoW/VFS workspace.
    let spawn = spawn_ws_or_cas_fallback(&repo, "agent-smoke");
    assert_eq!(
        spawn.code, 0,
        "ws spawn failed: {}\n{}",
        spawn.stdout, spawn.stderr
    );
    let spj = spawn.json.expect("spawn json");
    assert_oz_v1_envelope_invariants(&spj, true);
    assert!(spj["op_id"].is_string());
    let ws_path = PathBuf::from(
        spj["data"]["worktree_path"]
            .as_str()
            .expect("worktree_path"),
    );
    assert!(ws_path.is_dir(), "workspace path must exist on disk");

    // 3. Edit a file inside workspace -> `status` -> `diff` -> `commit`.
    fs::write(
        ws_path.join("lib.rs"),
        "pub fn greet() -> &'static str {\n    \"hello from v1.0.0\"\n}\n",
    )
    .expect("edit lib.rs in workspace");

    let status = run_bin(&oz_bin(), &ws_path, &["status", "--json"], &[]);
    assert_eq!(status.code, 0, "status failed: {}", status.stderr);
    let stj = status.json.expect("status json");
    assert_oz_v1_envelope_invariants(&stj, true);
    assert_eq!(stj["data"]["changes"][0]["id"], "f1");
    assert_eq!(stj["data"]["changes"][0]["path"], "lib.rs");

    let diff = run_bin(&oz_bin(), &ws_path, &["diff", "f1", "--json"], &[]);
    assert_eq!(diff.code, 0, "diff failed: {}", diff.stderr);
    let dfj = diff.json.expect("diff json");
    assert_oz_v1_envelope_invariants(&dfj, true);
    assert!(
        dfj["data"]["unified_diff"]
            .as_str()
            .is_some_and(|u| u.contains("hello from v1.0.0"))
    );

    let commit = run_bin(
        &oz_bin(),
        &ws_path,
        &["commit", "-m", "feat: v1.0.0 greeting", "--json"],
        &[],
    );
    assert_eq!(
        commit.code, 0,
        "commit failed: {}\n{}",
        commit.stdout, commit.stderr
    );
    let cmj = commit.json.expect("commit json");
    assert_oz_v1_envelope_invariants(&cmj, true);
    assert!(cmj["op_id"].is_string());
    assert!(cmj["data"]["change_id"].is_string());

    // 4. Checkpoint -> edit -> undo -> redo.
    let cp = run_bin(
        &oz_bin(),
        &ws_path,
        &["checkpoint", "-m", "before scratch", "--json"],
        &[],
    );
    assert_eq!(cp.code, 0, "checkpoint failed: {}", cp.stderr);
    assert_oz_v1_envelope_invariants(&cp.json.expect("cp json"), true);

    fs::write(ws_path.join("scratch.txt"), "temporary edit\n").expect("write scratch");
    let undo = run_bin(&oz_bin(), &ws_path, &["undo", "--json"], &[]);
    assert_eq!(undo.code, 0, "undo failed: {}\n{}", undo.stdout, undo.stderr);
    assert_oz_v1_envelope_invariants(&undo.json.expect("undo json"), true);
    assert!(!ws_path.join("scratch.txt").exists(), "undo must revert scratch.txt");

    let redo = run_bin(&oz_bin(), &ws_path, &["redo", "--json"], &[]);
    assert_eq!(redo.code, 0, "redo failed: {}\n{}", redo.stdout, redo.stderr);
    assert_oz_v1_envelope_invariants(&redo.json.expect("redo json"), true);
    assert!(ws_path.join("scratch.txt").exists(), "redo must restore scratch.txt");

    let undo2 = run_bin(&oz_bin(), &ws_path, &["undo", "--json"], &[]);
    assert_eq!(undo2.code, 0, "second undo failed: {}\n{}", undo2.stdout, undo2.stderr);
    assert_oz_v1_envelope_invariants(&undo2.json.expect("undo2 json"), true);
    assert!(!ws_path.join("scratch.txt").exists(), "second undo must clean scratch.txt");

    // 5. Radar + Integrate into `main` + Workspace removal + `git fsck --full`.
    let radar = run_bin(&oz_bin(), &repo, &["radar", "--json"], &[]);
    assert_eq!(radar.code, 0, "radar failed: {}", radar.stderr);
    assert_oz_v1_envelope_invariants(&radar.json.expect("radar json"), true);

    let integ = run_bin(
        &oz_bin(),
        &repo,
        &["integrate", "agent-smoke", "--into", "main", "--json"],
        &[],
    );
    assert_eq!(
        integ.code, 0,
        "integrate failed: {}\n{}",
        integ.stdout, integ.stderr
    );
    assert_oz_v1_envelope_invariants(&integ.json.expect("integrate json"), true);

    let rm = run_bin(
        &oz_bin(),
        &repo,
        &["ws", "rm", "agent-smoke", "--delete-branch", "--json"],
        &[],
    );
    assert_eq!(rm.code, 0, "ws rm failed: {}\n{}", rm.stdout, rm.stderr);
    assert_oz_v1_envelope_invariants(&rm.json.expect("ws rm json"), true);

    // Verify merged content on `main` and full Git integrity.
    let main_lib = sys_git(&repo, &["show", "main:lib.rs"]);
    assert!(main_lib.contains("hello from v1.0.0"));
    sys_git(&repo, &["fsck", "--full"]);
}
