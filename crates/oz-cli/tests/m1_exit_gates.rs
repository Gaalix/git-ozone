// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end Milestone 0 + Milestone 1 exit-gate and benchmark suite using
//! compiled `oz`, `git-ozone`, and `oz-vfsd` binaries on real Linux `ext4` + FUSE3.

#![cfg(target_os = "linux")]

use oz_core::{MountRecord, OZ_SCHEMA_V1, OzoneConfig, OzoneRepo};
use oz_oplog::{ActorInfo, ActorKind, checkpoint, redo, undo};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::FileTimes;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

fn sub10ms_budget_ms() -> f64 {
    if std::env::var_os("CI").is_some() && cfg!(debug_assertions) {
        100.0
    } else {
        10.0
    }
}

static VFSD_BIN_LOCK: OnceLock<PathBuf> = OnceLock::new();

fn oz_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oz"))
}

fn git_ozone_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_git-ozone"))
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
        let sibling = oz.with_file_name("oz-vfsd");
        if sibling.is_file() {
            return sibling;
        }

        let is_release = oz
            .components()
            .any(|c| matches!(c, std::path::Component::Normal(s) if s == "release"));
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir
            .parent()
            .and_then(Path::parent)
            .expect("workspace root");

        let mut build = Command::new(env!("CARGO"));
        build
            .arg("build")
            .arg("--locked")
            .arg("--bin")
            .arg("oz-vfsd")
            .current_dir(workspace_root);
        if is_release {
            build.arg("--release");
        }
        let status = build.status().expect("cargo build --bin oz-vfsd");
        assert!(status.success(), "failed to build oz-vfsd helper binary");
        assert!(sibling.is_file(), "expected oz-vfsd at {}", sibling.display());
        sibling
    })
}

/// RAII sandbox rooted on real `ext4` (`/var/tmp` when available) that guarantees
/// all FUSE mounts are unmounted and any test `oz-vfsd` processes are terminated
/// even if a test panics.
struct GateSandbox {
    root: PathBuf,
    repo_root: PathBuf,
    mount_points: Vec<PathBuf>,
    _tempdir: TempDir,
}

impl GateSandbox {
    fn new(repo_name: &str) -> Self {
        let _ = ensure_oz_vfsd_bin();
        let target_tmp = option_env!("CARGO_TARGET_TMPDIR").map(PathBuf::from);
        let tempdir = match target_tmp {
            Some(ref base) => {
                std::fs::create_dir_all(base).expect("create CARGO_TARGET_TMPDIR");
                tempfile::Builder::new()
                    .prefix("oz-m1-gate-")
                    .tempdir_in(base)
                    .expect("create ext4 tempdir in CARGO_TARGET_TMPDIR")
            }
            None => TempDir::new().expect("create tempdir"),
        };
        let root = tempdir.path().to_path_buf();
        let repo_root = root.join(repo_name);
        std::fs::create_dir_all(&repo_root).expect("mkdir repo_root");
        Self {
            root,
            repo_root,
            mount_points: Vec::new(),
            _tempdir: tempdir,
        }
    }

    fn container_dir(&self) -> PathBuf {
        let name = self
            .repo_root
            .file_name()
            .and_then(|s| s.to_str())
            .expect("repo name");
        self.root.join(format!("{name}.oz"))
    }

    fn ws_path(&self, ws: &str) -> PathBuf {
        self.container_dir().join(ws)
    }

    fn register_mount(&mut self, path: PathBuf) {
        if !self.mount_points.contains(&path) {
            self.mount_points.push(path);
        }
    }
}

impl Drop for GateSandbox {
    fn drop(&mut self) {
        let git_dir = self.repo_root.join(".git");
        let mut mounts_to_clean = self.mount_points.clone();
        let mut pids_to_kill: Vec<u32> = Vec::new();

        // Collect all mounts and daemon PIDs persisted under .git/ozone/ws/*/mount.json.
        let ws_root = git_dir.join("ozone/ws");
        if let Ok(entries) = std::fs::read_dir(&ws_root) {
            for entry in entries.flatten() {
                let mj = entry.path().join("mount.json");
                if let Ok(raw) = std::fs::read_to_string(&mj)
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

        // Also scan /proc/self/mountinfo for any FUSE mount under self.root.
        if let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") {
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

        if git_dir.is_dir() {
            let _ = oz_vfsd::shutdown_daemon(&git_dir);
        }

        // Kill any surviving oz-vfsd process specifically bound to this sandbox's git_dir.
        let git_dir_str = git_dir.to_string_lossy().into_owned();
        for pid in pids_to_kill {
            kill_test_vfsd_pid(pid, &git_dir_str);
        }
        if let Ok(proc_entries) = std::fs::read_dir("/proc") {
            for entry in proc_entries.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Ok(pid) = name.parse::<u32>() else {
                    continue;
                };
                kill_test_vfsd_pid(pid, &git_dir_str);
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
}

fn kill_test_vfsd_pid(pid: u32, git_dir_marker: &str) {
    let cmdline_path = format!("/proc/{pid}/cmdline");
    let Ok(bytes) = std::fs::read(&cmdline_path) else {
        return;
    };
    let cmdline = String::from_utf8_lossy(&bytes);
    if cmdline.contains("oz-vfsd") && cmdline.contains(git_dir_marker) {
        unsafe {
            let _ = libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        wait_pid_dead(pid, Duration::from_secs(2));
    }
}

fn wait_pid_dead(pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    while Instant::now() < deadline {
        if !proc_dir.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct CmdRun {
    code: i32,
    stdout: String,
    stderr: String,
    json: Option<Value>,
}

fn run_bin(bin: &Path, cwd: &Path, args: &[&str], extra_env: &[(&str, &Path)]) -> CmdRun {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .env_remove("OZ_AGENT")
        .env_remove("CI")
        .env_remove("OZ_VFSD_BIN")
        .stdin(Stdio::null());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("execute binary");
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let json = if args.contains(&"--json") {
        serde_json::from_str::<Value>(&stdout).ok()
    } else {
        None
    };
    CmdRun {
        code,
        stdout,
        stderr,
        json,
    }
}

fn init_git_repo(repo_root: &Path) -> String {
    assert!(
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(repo_root)
            .output()
            .expect("git init")
            .status
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "user.name", "Ozone Gate"])
            .current_dir(repo_root)
            .output()
            .expect("git config user.name")
            .status
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "user.email", "gate@ozone.dev"])
            .current_dir(repo_root)
            .output()
            .expect("git config user.email")
            .status
            .success()
    );
    git_commit_all(repo_root, "initial commit")
}

fn git_commit_all(repo_root: &Path, msg: &str) -> String {
    assert!(
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(repo_root)
            .output()
            .expect("git add")
            .status
            .success()
    );
    let out = Command::new("git")
        .args(["commit", "--allow-empty", "-m", msg])
        .current_dir(repo_root)
        .output()
        .expect("git commit");
    assert!(
        out.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rev = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&rev.stdout).trim().to_string()
}

fn set_mtime_unix_secs(path: &Path, secs: i64) {
    let st = if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs((-secs) as u64)
    };
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for set_times");
    file.set_times(FileTimes::new().set_modified(st))
        .expect("set mtime");
}

fn write_blob_file(path: &Path, size_bytes: usize, byte: u8, mode: u32, mtime_secs: i64) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir parent");
    }
    let payload = vec![byte; size_bytes];
    std::fs::write(path, payload).expect("write blob file");
    set_mtime_unix_secs(path, mtime_secs);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

fn visit_non_dir_files<F: FnMut(&Path, &std::fs::Metadata)>(root: &Path, cb: &mut F) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            visit_non_dir_files(&p, cb);
        } else {
            cb(&p, &meta);
        }
    }
}

fn physical_st_blocks_bytes(paths: &[PathBuf], exclude_inodes: Option<&HashSet<(u64, u64)>>) -> u64 {
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut total = 0u64;

    for root in paths {
        if !root.exists() {
            continue;
        }
        visit_non_dir_files(root, &mut |_, meta| {
            let key = (meta.dev(), meta.ino());
            if exclude_inodes.is_some_and(|ex| ex.contains(&key)) {
                return;
            }
            if seen.insert(key) {
                total = total.saturating_add(meta.blocks().saturating_mul(512));
            }
        });
    }

    total
}

fn raw_st_blocks_bytes(paths: &[PathBuf]) -> u64 {
    let mut total = 0u64;
    for root in paths {
        if !root.exists() {
            continue;
        }
        visit_non_dir_files(root, &mut |_, meta| {
            total = total.saturating_add(meta.blocks().saturating_mul(512));
        });
    }
    total
}

fn collect_inodes(root: &Path) -> HashSet<(u64, u64)> {
    let mut out = HashSet::new();
    visit_non_dir_files(root, &mut |_, meta| {
        out.insert((meta.dev(), meta.ino()));
    });
    out
}

fn ext4_allocated_bytes(path: &Path) -> u64 {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).expect("cstr");
    let mut buf = MaybeUninit::<libc::statvfs>::uninit();
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), buf.as_mut_ptr()) };
    assert_eq!(rc, 0, "statvfs failed");
    let st = unsafe { buf.assume_init() };
    let used_blocks = st.f_blocks.saturating_sub(st.f_bfree);
    used_blocks.saturating_mul(st.f_frsize)
}

#[test]
fn gate_m0_version_doctor_and_setup_json_on_ext4() {
    let sandbox = GateSandbox::new("m0-app");
    std::fs::write(sandbox.repo_root.join("README.md"), "# Ozone M0 Gate\n").expect("write readme");
    let head_oid = init_git_repo(&sandbox.repo_root);

    let ver = run_bin(&oz_bin(), &sandbox.root, &["--version", "--json"], &[]);
    assert_eq!(ver.code, 0, "stderr: {}", ver.stderr);
    let ver_json = ver.json.expect("version json");
    assert_eq!(ver_json["schema"], OZ_SCHEMA_V1);
    assert_eq!(ver_json["ok"], true);
    assert_eq!(ver_json["data"]["invoked_as"], "oz");
    assert_eq!(ver_json["data"]["pure_rust"], true);

    let doc = run_bin(
        &git_ozone_bin(),
        &sandbox.repo_root,
        &["doctor", "--json"],
        &[],
    );
    assert_eq!(doc.code, 0, "stderr: {}", doc.stderr);
    let doc_json = doc.json.expect("doctor json");
    assert_eq!(doc_json["schema"], OZ_SCHEMA_V1);
    assert_eq!(doc_json["ok"], true);
    assert_eq!(doc_json["branch"], "main");
    assert_eq!(doc_json["head_oid"], head_oid);
    assert_eq!(doc_json["data"]["invoked_as"], "git-ozone");
    assert_eq!(doc_json["data"]["filesystem"]["type"], "ext4");
    assert_eq!(doc_json["data"]["fuse"]["ready"], true);
    assert_eq!(doc_json["data"]["recommended_backend"], "linux_fuse3");
    assert_eq!(doc_json["data"]["oz_vfsd"]["found"], true);
    assert_eq!(doc_json["data"]["repo"]["in_repo"], true);
    assert_eq!(doc_json["data"]["repo"]["colocated"], true);

    let xdg_dir = sandbox.root.join("xdg_config");
    let setup = run_bin(
        &oz_bin(),
        &sandbox.repo_root,
        &["setup", "--systemd-user", "--json"],
        &[("XDG_CONFIG_HOME", &xdg_dir)],
    );
    assert_eq!(setup.code, 0, "stderr: {}", setup.stderr);
    let setup_json = setup.json.expect("setup json");
    assert_eq!(setup_json["ok"], true);
    let unit_path = PathBuf::from(
        setup_json["data"]["systemd_unit_path"]
            .as_str()
            .expect("unit path"),
    );
    assert!(unit_path.is_file(), "systemd unit file should be created");
}

#[test]
fn gate_m1_ext4_fuse3_space_and_speed_10_workspaces_and_warm_cache() {
    let mut sandbox = GateSandbox::new("mono-warm");
    let repo = sandbox.repo_root.clone();

    std::fs::create_dir_all(repo.join("src")).expect("mkdir src");
    std::fs::write(repo.join(".gitignore"), "/target\n/node_modules\n/.venv\n")
        .expect("write .gitignore");
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"mono-warm\"\nversion = \"0.1.0\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(repo.join("package.json"), "{\"name\":\"mono-warm\"}\n")
        .expect("write package.json");
    std::fs::write(repo.join("pyproject.toml"), "[project]\nname = \"mono-warm\"\n")
        .expect("write pyproject.toml");
    std::fs::write(repo.join("README.md"), "# Mono Warm\n").expect("write readme");
    std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").expect("write main.rs");
    std::fs::write(repo.join("src/untouched.rs"), "pub fn untouched() -> u32 { 1 }\n")
        .expect("write untouched");
    std::fs::write(repo.join("src/branch_mod.rs"), "pub fn mod_val() -> u32 { 1 }\n")
        .expect("write branch_mod");

    let base_commit = init_git_repo(&repo);

    // Create feature branch with modified src/branch_mod.rs, then return main to base_commit.
    assert!(
        Command::new("git")
            .args(["checkout", "-b", "feat/branch-base"])
            .current_dir(&repo)
            .output()
            .expect("checkout -b")
            .status
            .success()
    );
    std::fs::write(
        repo.join("src/branch_mod.rs"),
        "pub fn mod_val() -> u32 { 42 }\n",
    )
    .expect("edit branch_mod");
    let _branch_commit = git_commit_all(&repo, "feat: modify branch_mod.rs");
    assert!(
        Command::new("git")
            .args(["checkout", "main"])
            .current_dir(&repo)
            .output()
            .expect("checkout main")
            .status
            .success()
    );

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_secs() as i64;
    let source_mtime_secs = now_secs - 180;
    let cache_mtime_secs = now_secs - 60;

    // Backdate baseline source files so source_mtime <= cached artifact mtime.
    for rel in ["README.md", "src/main.rs", "src/untouched.rs", "src/branch_mod.rs"] {
        set_mtime_unix_secs(&repo.join(rel), source_mtime_secs);
    }

    // Create >= 50 MB of warm caches: target (20 MiB) + node_modules (20 MiB) + .venv (12 MiB) = 52 MiB.
    write_blob_file(
        &repo.join("target/debug/deps/libhuge_crate.rlib"),
        20 * 1024 * 1024,
        0xA1,
        0o444,
        cache_mtime_secs,
    );
    write_blob_file(
        &repo.join("target/debug/.fingerprint/dep.stamp"),
        1024,
        b'1',
        0o644,
        cache_mtime_secs,
    );
    write_blob_file(
        &repo.join("node_modules/mega-pkg/bundle.bin"),
        20 * 1024 * 1024,
        0xB2,
        0o444,
        cache_mtime_secs,
    );
    write_blob_file(
        &repo.join("node_modules/mega-pkg/index.js"),
        128,
        b'x',
        0o444,
        cache_mtime_secs,
    );
    write_blob_file(
        &repo.join(".venv/lib/python3.12/site-packages/weights.bin"),
        12 * 1024 * 1024,
        0xC3,
        0o444,
        cache_mtime_secs,
    );
    std::fs::write(repo.join(".venv/pyvenv.cfg"), "home = /usr/bin\n").expect("write pyvenv.cfg");
    std::fs::create_dir_all(repo.join(".venv/bin")).expect("mkdir .venv/bin");
    std::os::unix::fs::symlink("/usr/bin/python3", repo.join(".venv/bin/python3"))
        .expect("symlink python3");

    let script_path = repo.join(".venv/bin/oz-py-tool");
    let hardcoded_shebang_script = format!(
        "#!{}/.venv/bin/python3\nimport os, sys\nprint(f'TRAMPOLINE_OK:{{os.path.abspath(__file__)}}:{{sys.executable}}')\n",
        repo.display()
    );
    std::fs::write(&script_path, hardcoded_shebang_script).expect("write python tool");
    set_mtime_unix_secs(&script_path, cache_mtime_secs);
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod 755");

    // Capture warm baseline at main (c1) prior to spawning 10 workspaces.
    let ozone_dir = repo.join(".git/ozone");
    std::fs::create_dir_all(&ozone_dir).expect("mkdir ozone");
    let baseline_meta = oz_warm::capture_baseline(&repo, &ozone_dir, &base_commit, &OzoneConfig::default())
        .expect("capture warm baseline");
    assert!(
        baseline_meta.total_bytes >= 50 * 1024 * 1024,
        "warm cache baseline must be >= 50 MB (got {} bytes)",
        baseline_meta.total_bytes
    );

    let repo_inodes = collect_inodes(&repo);
    let ext4_used_before = ext4_allocated_bytes(&repo);
    let spawn_window_start_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_secs() as i64
        - 1;

    let mut latencies_ms = Vec::with_capacity(10);
    for i in 1..=10 {
        let ws_name = format!("agent-{i}");
        let branch_name = format!("feat/agent-{i}");
        sandbox.register_mount(sandbox.ws_path(&ws_name));

        let t0 = Instant::now();
        let out = run_bin(
            &oz_bin(),
            &repo,
            &[
                "ws",
                "spawn",
                &ws_name,
                "--branch",
                &branch_name,
                "--from",
                "feat/branch-base",
                "--json",
            ],
            &[],
        );
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(out.code, 0, "spawn {ws_name} failed: stdout={} stderr={}", out.stdout, out.stderr);
        let json = out.json.expect("spawn json");
        assert_eq!(json["schema"], OZ_SCHEMA_V1);
        assert_eq!(json["ok"], true);
        assert!(json["op_id"].as_str().is_some_and(|s| s.starts_with("op_")));
        assert_eq!(json["data"]["backend"], "linux_fuse3");
        assert_eq!(json["data"]["mounted"], true);
        latencies_ms.push(elapsed_ms);
    }

    let mean_ms = latencies_ms.iter().sum::<f64>() / latencies_ms.len() as f64;
    let max_ms = latencies_ms
        .iter()
        .copied()
        .fold(0.0_f64, f64::max);

    let backing_paths = vec![
        ozone_dir.join("ws"),
        repo.join(".git/worktrees"),
        ozone_dir.join("state"),
    ];
    let ws_raw_bytes = raw_st_blocks_bytes(&backing_paths);
    let ws_dedup_bytes = physical_st_blocks_bytes(&backing_paths, Some(&repo_inodes));
    let all_extra_dedup_bytes = physical_st_blocks_bytes(
        &[
            ozone_dir.join("ws"),
            ozone_dir.join("baselines"),
            ozone_dir.join("state"),
            repo.join(".git/worktrees"),
        ],
        Some(&repo_inodes),
    );
    let ext4_used_after = ext4_allocated_bytes(&repo);
    let ext4_delta_bytes = ext4_used_after.saturating_sub(ext4_used_before);

    println!(
        "[GATE M1 SPACE & SPEED] 10x ext4 FUSE3 spawn latencies_ms={latencies_ms:.2?} mean={mean_ms:.2}ms max={max_ms:.2}ms"
    );
    println!(
        "[GATE M1 SPACE & SPEED] extra_disk_bytes: ws_raw={ws_raw_bytes} ws_dedup={ws_dedup_bytes} all_ozone_dedup={all_extra_dedup_bytes} ext4_statvfs_delta={ext4_delta_bytes}"
    );

    assert!(
        max_ms < 500.0,
        "max spawn latency {max_ms:.2}ms exceeded 500ms budget (all={latencies_ms:?})"
    );
    assert!(
        mean_ms < 500.0,
        "mean spawn latency {mean_ms:.2}ms exceeded 500ms budget"
    );
    assert!(
        ws_raw_bytes < 50 * 1024 * 1024 && all_extra_dedup_bytes < 50 * 1024 * 1024,
        "additional physical disk exceeded 50 MB: ws_raw={ws_raw_bytes}, all_dedup={all_extra_dedup_bytes}"
    );

    // Check untouched mtime preserved <= cached artifact mtime, and branch-modified mtime is NOW.
    let ws1 = sandbox.ws_path("agent-1");
    let untouched_mtime = std::fs::metadata(ws1.join("src/untouched.rs"))
        .expect("stat untouched.rs")
        .mtime();
    let artifact_mtime = std::fs::metadata(ws1.join("target/debug/deps/libhuge_crate.rlib"))
        .expect("stat cached rlib")
        .mtime();
    let branch_mod_mtime = std::fs::metadata(ws1.join("src/branch_mod.rs"))
        .expect("stat branch_mod.rs")
        .mtime();

    assert_eq!(
        untouched_mtime, source_mtime_secs,
        "untouched baseline file mtime should match baseline"
    );
    assert!(
        untouched_mtime <= artifact_mtime,
        "untouched source mtime ({untouched_mtime}) must be <= cached artifact mtime ({artifact_mtime})"
    );
    assert!(
        branch_mod_mtime >= spawn_window_start_secs && branch_mod_mtime > artifact_mtime,
        "branch-modified file mtime ({branch_mod_mtime}) must reflect NOW (>= {spawn_window_start_secs} and > {artifact_mtime})"
    );
    assert_eq!(
        std::fs::read_to_string(ws1.join("src/branch_mod.rs")).expect("read branch_mod"),
        "pub fn mod_val() -> u32 { 42 }\n"
    );

    // Execute rewritten Python trampoline directly inside spawned FUSE workspace.
    let ws1_tool = ws1.join(".venv/bin/oz-py-tool");
    let tool_contents = std::fs::read_to_string(&ws1_tool).expect("read trampoline");
    assert!(
        tool_contents.starts_with("#!/bin/sh\n'''exec' \"$(dirname \"$0\")/python3\""),
        "expected rewritten trampoline shebang, got: {tool_contents}"
    );
    let py_out = Command::new(&ws1_tool)
        .current_dir(&ws1)
        .output()
        .expect("exec trampoline script");
    assert!(
        py_out.status.success(),
        "trampoline execution failed: {}",
        String::from_utf8_lossy(&py_out.stderr)
    );
    let py_stdout = String::from_utf8_lossy(&py_out.stdout);
    assert!(
        py_stdout.starts_with("TRAMPOLINE_OK:") && py_stdout.contains("agent-1"),
        "unexpected trampoline output: {py_stdout}"
    );

    // Verify `oz ws list --json` reports all 10 mounts healthy.
    let list_out = run_bin(&oz_bin(), &repo, &["ws", "list", "--json"], &[]);
    assert_eq!(list_out.code, 0, "ws list failed: {}", list_out.stderr);
    let list_json = list_out.json.expect("ws list json");
    let workspaces = list_json["data"]["workspaces"]
        .as_array()
        .expect("workspaces array");
    assert_eq!(workspaces.len(), 10);
    assert!(workspaces.iter().all(|w| w["healthy"] == true && w["mount_health"] == "healthy"));

    // Cleanly remove one workspace via CLI and verify `oz ws gc --max-gb`.
    let rm_out = run_bin(&oz_bin(), &repo, &["ws", "rm", "agent-10", "--json"], &[]);
    assert_eq!(rm_out.code, 0, "ws rm failed: {}", rm_out.stderr);
    assert!(!sandbox.ws_path("agent-10").exists());

    let gc_out = run_bin(
        &oz_bin(),
        &repo,
        &["ws", "gc", "--max-gb", "10", "--json"],
        &[],
    );
    assert_eq!(gc_out.code, 0, "ws gc failed: {}", gc_out.stdout);
    let gc_json = gc_out.json.expect("ws gc json");
    assert_eq!(gc_json["ok"], true);
    assert!(gc_json["op_id"].as_str().is_some_and(|s| s.starts_with("op_")));
}

#[test]
fn gate_m1_sigkill_auto_remount_zero_lost_edits_including_cwd_inside_dead_mount() {
    let mut sandbox = GateSandbox::new("sigkill-app");
    let repo = sandbox.repo_root.clone();

    std::fs::create_dir_all(repo.join("src")).expect("mkdir src");
    std::fs::write(repo.join("README.md"), "baseline readme\n").expect("write readme");
    std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> i32 { 1 }\n").expect("write lib");
    init_git_repo(&repo);

    let ws_mount = sandbox.ws_path("ws-kill");
    sandbox.register_mount(ws_mount.clone());

    let spawn_out = run_bin(&oz_bin(), &repo, &["ws", "spawn", "ws-kill", "--json"], &[]);
    assert_eq!(spawn_out.code, 0, "spawn failed: stdout={} stderr={}", spawn_out.stdout, spawn_out.stderr);

    // Perform write new file, modify baseline file, delete baseline file inside FUSE3 workspace.
    std::fs::write(ws_mount.join("src/added.rs"), "pub fn added() -> i32 { 42 }\n")
        .expect("write added.rs");
    std::fs::write(ws_mount.join("src/lib.rs"), "pub fn base() -> i32 { 99 }\n")
        .expect("modify lib.rs");
    std::fs::remove_file(ws_mount.join("README.md")).expect("delete README.md");

    let mount_json_path = repo.join(".git/ozone/ws/ws-kill/mount.json");
    let rec1: MountRecord = serde_json::from_str(
        &std::fs::read_to_string(&mount_json_path).expect("read mount.json"),
    )
    .expect("parse mount.json");
    let pid1 = rec1.daemon_pid.expect("daemon_pid before first SIGKILL");

    // 1. SIGKILL daemon and verify ENOTCONN, then recover via `oz -C <ws_mount> status --json`.
    unsafe {
        assert_eq!(libc::kill(pid1 as libc::pid_t, libc::SIGKILL), 0);
    }
    wait_pid_dead(pid1, Duration::from_secs(3));
    let dead_meta = std::fs::symlink_metadata(&ws_mount).expect_err("dead mount must fail stat");
    assert_eq!(
        dead_meta.raw_os_error(),
        Some(libc::ENOTCONN),
        "expected ENOTCONN after SIGKILL, got {dead_meta:?}"
    );

    let recover_c = run_bin(
        &oz_bin(),
        &sandbox.root,
        &["-C", ws_mount.to_str().expect("utf8"), "status", "--json"],
        &[],
    );
    assert_eq!(
        recover_c.code, 0,
        "status -C <dead_mount> failed: {}",
        recover_c.stderr
    );
    let json1 = recover_c.json.expect("status json 1");
    assert_eq!(json1["ok"], true);
    assert_eq!(json1["data"]["used_fuse_fast_path"], true);
    assert_eq!(json1["data"]["changes"].as_array().map(Vec::len), Some(3));

    let rec2: MountRecord = serde_json::from_str(
        &std::fs::read_to_string(&mount_json_path).expect("read mount.json 2"),
    )
    .expect("parse mount.json 2");
    let pid2 = rec2.daemon_pid.expect("daemon_pid after remount 1");
    assert_ne!(pid1, pid2, "oz-vfsd should have respawned with a new PID");

    // 2. CWD literally INSIDE the dead FUSE mount (+ PWD set) without `-C`!
    let shell_script = r#"
cd "$1" || exit 91
export PWD="$1"
kill -9 "$2" || exit 92
while kill -0 "$2" 2>/dev/null; do sleep 0.01; done
"$3" status --json
"#;
    let cwd_dead_out = Command::new("sh")
        .arg("-c")
        .arg(shell_script)
        .arg("sh")
        .arg(&ws_mount)
        .arg(pid2.to_string())
        .arg(oz_bin())
        .env_remove("OZ_AGENT")
        .env_remove("CI")
        .env_remove("OZ_VFSD_BIN")
        .output()
        .expect("run sh inside dead FUSE mount");
    assert_eq!(
        cwd_dead_out.status.code(),
        Some(0),
        "oz status from INSIDE dead FUSE mount failed: stderr={}",
        String::from_utf8_lossy(&cwd_dead_out.stderr)
    );
    let json2: Value =
        serde_json::from_slice(&cwd_dead_out.stdout).expect("parse status json from dead cwd");
    assert_eq!(json2["schema"], OZ_SCHEMA_V1);
    assert_eq!(json2["ok"], true);
    assert_eq!(json2["workspace"], "ws-kill");
    let changes = json2["data"]["changes"].as_array().expect("changes array");
    assert_eq!(changes.len(), 3);
    assert!(changes.iter().any(|c| c["path"] == "README.md" && c["kind"] == "deleted"));
    assert!(changes.iter().any(|c| c["path"] == "src/added.rs" && c["kind"] == "added"));
    assert!(changes.iter().any(|c| c["path"] == "src/lib.rs" && c["kind"] == "modified"));

    // 3. Also verify subdirectory CWD inside dead FUSE mount recovers cleanly.
    let rec3: MountRecord = serde_json::from_str(
        &std::fs::read_to_string(&mount_json_path).expect("read mount.json 3"),
    )
    .expect("parse mount.json 3");
    let pid3 = rec3.daemon_pid.expect("daemon_pid after remount 2");
    assert_ne!(pid2, pid3);

    let subdir_dead_out = Command::new("sh")
        .arg("-c")
        .arg(shell_script)
        .arg("sh")
        .arg(ws_mount.join("src"))
        .arg(pid3.to_string())
        .arg(oz_bin())
        .env_remove("OZ_AGENT")
        .env_remove("CI")
        .env_remove("OZ_VFSD_BIN")
        .output()
        .expect("run sh inside dead FUSE subdir");
    assert_eq!(
        subdir_dead_out.status.code(),
        Some(0),
        "oz status from subdir inside dead mount failed: stderr={}",
        String::from_utf8_lossy(&subdir_dead_out.stderr)
    );

    // Confirm zero lost edits directly through the recovered FUSE mountpoint!
    assert!(!ws_mount.join("README.md").exists());
    assert_eq!(
        std::fs::read_to_string(ws_mount.join("src/added.rs")).expect("read added"),
        "pub fn added() -> i32 { 42 }\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws_mount.join("src/lib.rs")).expect("read lib"),
        "pub fn base() -> i32 { 99 }\n"
    );
}

#[test]
fn gate_m1_stage_free_partial_commit_guardrails_and_sub10ms_undo_redo() {
    let mut sandbox = GateSandbox::new("vcs-gate");
    let repo = sandbox.repo_root.clone();

    let orig_a: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    std::fs::write(repo.join("a_multi.txt"), &orig_a).expect("write a_multi");
    std::fs::write(repo.join("b_full.txt"), "original b\n").expect("write b_full");
    std::fs::write(repo.join("c_keep.txt"), "original c\n").expect("write c_keep");
    let base_commit = init_git_repo(&repo);

    let ws_vcs = sandbox.ws_path("ws-vcs");
    sandbox.register_mount(ws_vcs.clone());
    let sp = run_bin(&oz_bin(), &repo, &["ws", "spawn", "ws-vcs", "--json"], &[]);
    assert_eq!(sp.code, 0, "spawn ws-vcs failed: stdout={} stderr={}", sp.stdout, sp.stderr);

    // Edit a_multi.txt (two separated hunks h1a and h1b), b_full.txt (f2), and c_keep.txt (f3).
    let mut lines_a: Vec<String> = (1..=30).map(|i| format!("line {i}\n")).collect();
    lines_a[1] = "line 2 hunk A\n".to_string();
    lines_a[26] = "line 27 hunk B\n".to_string();
    std::fs::write(ws_vcs.join("a_multi.txt"), lines_a.concat()).expect("edit a_multi");
    std::fs::write(ws_vcs.join("b_full.txt"), "updated b\n").expect("edit b_full");
    std::fs::write(ws_vcs.join("c_keep.txt"), "updated c\n").expect("edit c_keep");

    // Verify [f1]/[h1a] deterministic IDs in status and diff.
    let st = run_bin(&oz_bin(), &ws_vcs, &["status", "--json"], &[]);
    assert_eq!(st.code, 0, "status failed: {}", st.stderr);
    let st_json = st.json.expect("status json");
    let changes = st_json["data"]["changes"].as_array().expect("changes");
    assert_eq!(changes.len(), 3);
    assert_eq!(changes[0]["id"], "f1");
    assert_eq!(changes[0]["path"], "a_multi.txt");
    assert_eq!(changes[0]["hunks"][0]["id"], "h1a");
    assert_eq!(changes[0]["hunks"][1]["id"], "h1b");
    assert_eq!(changes[1]["id"], "f2");
    assert_eq!(changes[1]["path"], "b_full.txt");
    assert_eq!(changes[2]["id"], "f3");
    assert_eq!(changes[2]["path"], "c_keep.txt");

    let diff_out = run_bin(&oz_bin(), &ws_vcs, &["diff", "h1a", "f2"], &[]);
    assert_eq!(diff_out.code, 0);
    assert!(diff_out.stdout.contains("diff --ozone [f1]"));
    assert!(diff_out.stdout.contains("[h1a]"));
    assert!(diff_out.stdout.contains("diff --ozone [f2]"));
    assert!(!diff_out.stdout.contains("[h1b]"));

    let diff_json_out = run_bin(&oz_bin(), &ws_vcs, &["diff", "h1a", "--json"], &[]);
    assert_eq!(diff_json_out.code, 0);
    let diff_json = diff_json_out.json.expect("diff json");
    assert_eq!(diff_json["ok"], true);
    assert!(
        diff_json["data"]["unified_diff"]
            .as_str()
            .is_some_and(|s| s.contains("[h1a]"))
    );

    let quiet_cp = run_bin(&oz_bin(), &ws_vcs, &["checkpoint", "--quiet"], &[]);
    assert_eq!(quiet_cp.code, 0);
    assert!(quiet_cp.stdout.is_empty());

    // Stage-free partial commit: --pick f2 h1a
    let commit_out = run_bin(
        &oz_bin(),
        &ws_vcs,
        &[
            "commit",
            "-m",
            "feat: partial h1a and f2",
            "--pick",
            "f2",
            "h1a",
            "--json",
        ],
        &[],
    );
    assert_eq!(commit_out.code, 0, "partial commit failed: {}", commit_out.stderr);
    let c_json = commit_out.json.expect("commit json");
    assert_eq!(c_json["ok"], true);
    let partial_op_id = c_json["op_id"].as_str().expect("op_id").to_string();
    assert!(partial_op_id.starts_with("op_"));
    let change_id = c_json["data"]["change_id"].as_str().expect("change_id");
    assert!(change_id.starts_with('I') && change_id.len() == 41);
    assert!(
        c_json["data"]["message"]
            .as_str()
            .expect("msg")
            .contains(&format!("Change-Id: {change_id}"))
    );
    assert_eq!(
        c_json["data"]["committed_paths"],
        serde_json::json!(["a_multi.txt", "b_full.txt"])
    );
    assert_eq!(
        c_json["data"]["partial_hunk_paths"],
        serde_json::json!(["a_multi.txt"])
    );

    // Check committed tree on branch ws-vcs (using git show on main repo).
    let show_a = Command::new("git")
        .args(["show", "ws-vcs:a_multi.txt"])
        .current_dir(&repo)
        .output()
        .expect("git show a");
    let show_a_str = String::from_utf8_lossy(&show_a.stdout);
    assert!(show_a_str.contains("line 2 hunk A\n"));
    assert!(!show_a_str.contains("line 27 hunk B\n"));
    assert!(show_a_str.contains("line 27\n"));

    let show_c = Command::new("git")
        .args(["show", "ws-vcs:c_keep.txt"])
        .current_dir(&repo)
        .output()
        .expect("git show c");
    assert_eq!(String::from_utf8_lossy(&show_c.stdout), "original c\n");

    // Verify h1b and f3 remain dirty on disk in ws-vcs.
    let st_after = run_bin(&oz_bin(), &ws_vcs, &["status", "--json"], &[]);
    let st_after_json = st_after.json.expect("status after partial");
    let rem_changes = st_after_json["data"]["changes"].as_array().expect("rem");
    assert_eq!(rem_changes.len(), 2);
    assert_eq!(rem_changes[0]["path"], "a_multi.txt");
    assert_eq!(rem_changes[0]["hunks"].as_array().map(Vec::len), Some(1));
    assert_eq!(rem_changes[1]["path"], "c_keep.txt");

    // CLI Checkpoint + Undo + Redo roundtrip on FUSE workspace ws-vcs (< 10 ms engine timing).
    let partial_commit_oid = c_json["data"]["commit_oid"]
        .as_str()
        .expect("partial commit oid")
        .to_string();
    let undo1 = run_bin(&oz_bin(), &ws_vcs, &["undo", "--json"], &[]);
    assert_eq!(undo1.code, 0, "undo failed: {}", undo1.stderr);
    let undo1_json = undo1.json.expect("undo1 json");
    assert_eq!(undo1_json["ok"], true);
    let undo1_ms = undo1_json["data"]["engine_elapsed_ms"].as_f64().expect("ms");
    assert_eq!(undo1_json["head_oid"].as_str(), Some(base_commit.as_str()));
    assert_eq!(
        std::fs::read_to_string(ws_vcs.join("b_full.txt")).expect("read b after undo"),
        "updated b\n"
    );

    let redo1 = run_bin(&oz_bin(), &ws_vcs, &["redo", "--json"], &[]);
    assert_eq!(redo1.code, 0, "redo failed: {}", redo1.stderr);
    let redo1_json = redo1.json.expect("redo1 json");
    assert_eq!(redo1_json["ok"], true);
    let redo1_ms = redo1_json["data"]["engine_elapsed_ms"].as_f64().expect("ms");
    assert_eq!(redo1_json["head_oid"].as_str(), Some(partial_commit_oid.as_str()));

    let cp = run_bin(
        &oz_bin(),
        &ws_vcs,
        &["checkpoint", "-m", "cp-dirty", "--json"],
        &[],
    );
    assert_eq!(cp.code, 0, "checkpoint failed: {}", cp.stderr);
    std::fs::write(ws_vcs.join("c_keep.txt"), "post checkpoint edit\n").expect("edit post cp");
    let undo_cp = run_bin(&oz_bin(), &ws_vcs, &["undo", "--json"], &[]);
    assert_eq!(undo_cp.code, 0, "undo uncheckpointed failed: {}", undo_cp.stderr);
    assert_eq!(
        std::fs::read_to_string(ws_vcs.join("c_keep.txt")).expect("read restored c_keep"),
        "updated c\n"
    );
    let redo_cp = run_bin(&oz_bin(), &ws_vcs, &["redo", "--json"], &[]);
    assert_eq!(redo_cp.code, 0, "redo uncheckpointed failed: {}", redo_cp.stderr);
    assert_eq!(
        std::fs::read_to_string(ws_vcs.join("c_keep.txt")).expect("read redone c_keep"),
        "post checkpoint edit\n"
    );

    // Verify `--exclude` commit + explicit `oz undo <op_id>`.
    let excl_commit = run_bin(
        &oz_bin(),
        &ws_vcs,
        &[
            "commit",
            "-m",
            "feat: commit c_keep excluding a_multi",
            "--exclude",
            "f1",
            "--json",
        ],
        &[],
    );
    assert_eq!(excl_commit.code, 0, "exclude commit failed: {}", excl_commit.stdout);
    let excl_json = excl_commit.json.expect("excl json");
    assert_eq!(
        excl_json["data"]["committed_paths"],
        serde_json::json!(["c_keep.txt"])
    );
    let excl_op_id = excl_json["op_id"].as_str().expect("excl op_id").to_string();
    let undo_by_id = run_bin(&oz_bin(), &ws_vcs, &["undo", &excl_op_id, "--json"], &[]);
    assert_eq!(undo_by_id.code, 0, "undo by op_id failed: {}", undo_by_id.stdout);

    // Guardrail 1: .env file -> exit code 2.
    std::fs::write(ws_vcs.join(".env"), "API_KEY=supersecret\n").expect("write .env");
    let env_fail = run_bin(
        &oz_bin(),
        &ws_vcs,
        &["commit", "-m", "bad env", "--pick", ".env", "--json"],
        &[],
    );
    assert_eq!(env_fail.code, 2, "expected exit 2 on .env: {}", env_fail.stdout);
    assert_eq!(env_fail.json.expect("env json")["error"]["code"], "guardrail_violation");
    std::fs::remove_file(ws_vcs.join(".env")).expect("rm .env");

    // Guardrail 2: secret token -> exit code 2.
    std::fs::write(
        ws_vcs.join("token.txt"),
        "token=ghp_1234567890abcdefghijklmnopqrstuvwxyz\n",
    )
    .expect("write token");
    let sec_fail = run_bin(
        &oz_bin(),
        &ws_vcs,
        &["commit", "-m", "bad secret", "--pick", "token.txt", "--json"],
        &[],
    );
    assert_eq!(sec_fail.code, 2, "expected exit 2 on secret: {}", sec_fail.stdout);
    assert_eq!(sec_fail.json.expect("sec json")["error"]["code"], "guardrail_violation");
    std::fs::remove_file(ws_vcs.join("token.txt")).expect("rm token");

    // Guardrail 3: conflict marker -> exit code 2.
    std::fs::write(
        ws_vcs.join("conflict.txt"),
        "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> feat\n",
    )
    .expect("write conflict");
    let cfl_fail = run_bin(
        &oz_bin(),
        &ws_vcs,
        &["commit", "-m", "bad conflict", "--pick", "conflict.txt", "--json"],
        &[],
    );
    assert_eq!(cfl_fail.code, 2, "expected exit 2 on conflict marker: {}", cfl_fail.stdout);
    assert_eq!(cfl_fail.json.expect("cfl json")["error"]["code"], "guardrail_violation");
    std::fs::remove_file(ws_vcs.join("conflict.txt")).expect("rm conflict");

    // Guardrail 4: > 50 MB binary -> exit code 2, overridden by `--force`.
    let huge_path = ws_vcs.join("huge.bin");
    std::fs::write(&huge_path, vec![0x5Au8; 51 * 1024 * 1024]).expect("write 51 MiB binary");
    let bin_fail = run_bin(
        &oz_bin(),
        &ws_vcs,
        &["commit", "-m", "bad huge", "--pick", "huge.bin", "--json"],
        &[],
    );
    assert_eq!(bin_fail.code, 2, "expected exit 2 on >50MB binary: {}", bin_fail.stdout);
    assert_eq!(bin_fail.json.expect("bin json")["error"]["code"], "guardrail_violation");

    let bin_force = run_bin(
        &oz_bin(),
        &ws_vcs,
        &["commit", "-m", "force huge", "--pick", "huge.bin", "--force", "--json"],
        &[],
    );
    assert_eq!(bin_force.code, 0, "expected --force override to pass: {}", bin_force.stderr);

    // Guardrail 5: workspace `--scope` / `--deny` enforcement + `--allow-out-of-scope`.
    let ws_scoped = sandbox.ws_path("ws-scoped");
    sandbox.register_mount(ws_scoped.clone());
    let sp_scoped = run_bin(
        &oz_bin(),
        &repo,
        &[
            "ws",
            "spawn",
            "ws-scoped",
            "--scope",
            "a_multi.txt",
            "b_full.txt",
            "--deny",
            "b_full.txt",
            "--json",
        ],
        &[],
    );
    assert_eq!(sp_scoped.code, 0, "spawn scoped failed: {}", sp_scoped.stderr);
    std::fs::write(ws_scoped.join("b_full.txt"), "denied edit\n").expect("edit denied");
    let scope_fail = run_bin(
        &oz_bin(),
        &ws_scoped,
        &["commit", "-m", "denied", "--json"],
        &[],
    );
    assert_eq!(scope_fail.code, 2, "expected exit 2 on deny guardrail");
    assert_eq!(
        scope_fail.json.expect("scope fail json")["error"]["code"],
        "guardrail_violation"
    );
    let scope_override = run_bin(
        &oz_bin(),
        &ws_scoped,
        &["commit", "-m", "allow out of scope", "--allow-out-of-scope", "--json"],
        &[],
    );
    assert_eq!(scope_override.code, 0, "expected --allow-out-of-scope override to pass");

    // Dedicated < 10 ms engine timing gate on a 1,500-file repository (both FUSE3 O(dirty_files) and non-FUSE).
    let mut bench_box = GateSandbox::new("oplog-1500-bench");
    let bench_repo_dir = bench_box.repo_root.clone();
    for dir_idx in 0..15 {
        let sub = bench_repo_dir.join(format!("pkg_{dir_idx:02}"));
        std::fs::create_dir_all(&sub).expect("mkdir pkg");
        for file_idx in 0..100 {
            std::fs::write(
                sub.join(format!("mod_{file_idx:03}.rs")),
                format!("pub fn f_{dir_idx}_{file_idx}() -> usize {{ {file_idx} }}\n"),
            )
            .expect("write mod file");
        }
    }
    init_git_repo(&bench_repo_dir);

    let ws_bench = bench_box.ws_path("ws-1500");
    bench_box.register_mount(ws_bench.clone());
    let sp_bench = run_bin(
        &oz_bin(),
        &bench_repo_dir,
        &["ws", "spawn", "ws-1500", "--json"],
        &[],
    );
    assert_eq!(sp_bench.code, 0, "spawn ws-1500 failed: {}", sp_bench.stdout);

    std::fs::write(ws_bench.join("pkg_00/mod_000.rs"), "pub fn edited() -> usize { 999 }\n")
        .expect("edit 1 file in 1500-file FUSE ws");
    std::fs::write(ws_bench.join("pkg_01/new_file.rs"), "pub fn added() {}\n")
        .expect("add 1 file in 1500-file FUSE ws");
    std::fs::remove_file(ws_bench.join("pkg_02/mod_000.rs")).expect("delete 1 file in 1500-file FUSE ws");

    let fuse_bench_repo = OzoneRepo::discover(Some(&ws_bench)).expect("discover ws-1500");
    let actor = ActorInfo {
        kind: ActorKind::Agent,
        client: Some("m1-exit-gate".to_string()),
        workspace: Some("ws-1500".to_string()),
    };

    let t_cp_fuse = Instant::now();
    let _cp_fuse = checkpoint(&fuse_bench_repo, "oz checkpoint", Some("dirty3"), actor.clone(), None)
        .expect("cp fuse 1500");
    let fuse_1500_cp_ms = t_cp_fuse.elapsed().as_secs_f64() * 1000.0;

    // Make post-checkpoint edit, then run real `oz undo --json` and `oz redo --json` on 1,500-file FUSE mount.
    std::fs::write(ws_bench.join("pkg_03/post_cp.rs"), "pub fn post() {}\n")
        .expect("post cp edit");
    let cli_undo_1500 = run_bin(&oz_bin(), &ws_bench, &["undo", "--json"], &[]);
    assert_eq!(cli_undo_1500.code, 0, "cli undo 1500 failed: {}", cli_undo_1500.stdout);
    let cli_undo_1500_json = cli_undo_1500.json.expect("cli undo 1500 json");
    let fuse_1500_undo_ms = cli_undo_1500_json["data"]["engine_elapsed_ms"]
        .as_f64()
        .expect("undo ms");
    assert!(!ws_bench.join("pkg_03/post_cp.rs").exists());
    assert!(ws_bench.join("pkg_01/new_file.rs").is_file());

    let cli_redo_1500 = run_bin(&oz_bin(), &ws_bench, &["redo", "--json"], &[]);
    assert_eq!(cli_redo_1500.code, 0, "cli redo 1500 failed: {}", cli_redo_1500.stdout);
    let cli_redo_1500_json = cli_redo_1500.json.expect("cli redo 1500 json");
    let fuse_1500_redo_ms = cli_redo_1500_json["data"]["engine_elapsed_ms"]
        .as_f64()
        .expect("redo ms");
    assert!(ws_bench.join("pkg_03/post_cp.rs").is_file());

    // Also benchmark direct `oz_oplog::undo` / `oz_oplog::redo` on a MutationTransaction commit in the 1,500-file repo.
    let main_bench_repo = OzoneRepo::discover(Some(&bench_repo_dir)).expect("discover bench repo");
    let main_actor = ActorInfo {
        kind: ActorKind::Agent,
        client: Some("m1-exit-gate".to_string()),
        workspace: None,
    };
    let dirty_changes = oz_core::compute_status(&main_bench_repo)
        .expect("status")
        .changes;
    let tx = oz_oplog::MutationTransaction::begin(
        &main_bench_repo,
        "oz commit -m bench",
        main_actor.clone(),
        Some(&dirty_changes),
    )
    .expect("begin tx");
    std::fs::write(bench_repo_dir.join("pkg_00/mod_000.rs"), "pub fn committed() -> usize { 1 }\n")
        .expect("edit for commit");
    git_commit_all(&bench_repo_dir, "bench commit");
    let mut post_changes = dirty_changes;
    post_changes.clear();
    let m_entry = tx.finish(&main_bench_repo, Some(&post_changes)).expect("finish tx");

    let t_undo = Instant::now();
    let _u = undo(&main_bench_repo, m_entry.op_id.as_str(), main_actor.clone()).expect("undo 1500");
    let undo_1500_ms = t_undo.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(
        std::fs::read_to_string(bench_repo_dir.join("pkg_00/mod_000.rs")).expect("read undone"),
        "pub fn f_0_0() -> usize { 0 }\n"
    );

    let t_redo = Instant::now();
    let _r = redo(&main_bench_repo, main_actor).expect("redo 1500");
    let redo_1500_ms = t_redo.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(
        std::fs::read_to_string(bench_repo_dir.join("pkg_00/mod_000.rs")).expect("read redone"),
        "pub fn committed() -> usize { 1 }\n"
    );

    println!(
        "[GATE M1 UNDO/REDO <10ms] fuse_cli=(undo={undo1_ms:.2}ms, redo={redo1_ms:.2}ms) fuse_1500=(cp={fuse_1500_cp_ms:.2}ms, undo={fuse_1500_undo_ms:.2}ms, redo={fuse_1500_redo_ms:.2}ms) repo_1500_tx=(undo={undo_1500_ms:.2}ms, redo={redo_1500_ms:.2}ms)"
    );

    let budget_ms = sub10ms_budget_ms();
    assert!(undo1_ms < budget_ms, "FUSE CLI undo engine took {undo1_ms:.2}ms (>= {budget_ms}ms)");
    assert!(redo1_ms < budget_ms, "FUSE CLI redo engine took {redo1_ms:.2}ms (>= {budget_ms}ms)");
    assert!(fuse_1500_cp_ms < budget_ms, "1500-file FUSE checkpoint took {fuse_1500_cp_ms:.2}ms (>= {budget_ms}ms)");
    assert!(fuse_1500_undo_ms < budget_ms, "1500-file FUSE CLI undo took {fuse_1500_undo_ms:.2}ms (>= {budget_ms}ms)");
    assert!(fuse_1500_redo_ms < budget_ms, "1500-file FUSE CLI redo took {fuse_1500_redo_ms:.2}ms (>= {budget_ms}ms)");
    assert!(undo_1500_ms < budget_ms, "1500-file repo undo took {undo_1500_ms:.2}ms (>= {budget_ms}ms)");
    assert!(redo_1500_ms < budget_ms, "1500-file repo redo took {redo_1500_ms:.2}ms (>= {budget_ms}ms)");
}
