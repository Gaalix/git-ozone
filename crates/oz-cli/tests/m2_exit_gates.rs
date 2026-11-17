// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end Milestone 2 exit-gate verification suite covering:
//! 1. 10-process concurrent `oz commit` stress test on real Linux `ext4` FUSE3 (and OS native/fallback)
//! 2. Windows NTFS ProjFS 10-workspace space/speed gate + cross-OS `oz doctor` & `oz setup --enable-projfs`
//! 3. Guardrails (`--scope`/`--deny`, `out_of_scope` status warnings, exit-2 blocking) & live `oz radar --json`
//! 4. Atomic in-memory `oz integrate`, `_integrate/` sandbox, `oz resolve`, `--verify`, & `oz stack rebase`
//! 5. `oz git` compatibility shim (`merge`/`rebase` via `oz-integrate`, `add`/`status`/`diff`/`worktree`/`clone`) & `reset --hard` `< 10 ms` undo

use oz_core::{MountRecord, OZ_SCHEMA_V1, OzoneLayout};
use oz_oplog::{OpLogEntry, list_entries, read_head_op_id};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier, OnceLock};
use std::time::Instant;
use tempfile::TempDir;

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
        let sibling = oz.with_file_name(oz_vfsd::vfsd_binary_name());
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

/// Cross-platform RAII sandbox rooted on real `ext4`/`NTFS`/`APFS` (`CARGO_TARGET_TMPDIR`)
/// with short tempdir prefixes (`oz-m2-`) so Unix `SUN_LEN` (< 108 bytes) and AppArmor pass.
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
        let target_tmp = option_env!("CARGO_TARGET_TMPDIR").map(PathBuf::from);
        let tempdir = match target_tmp {
            Some(ref base) => {
                fs::create_dir_all(base).expect("create CARGO_TARGET_TMPDIR");
                tempfile::Builder::new()
                    .prefix("oz-m2-")
                    .tempdir_in(base)
                    .expect("create tempdir in CARGO_TARGET_TMPDIR")
            }
            None => tempfile::Builder::new()
                .prefix("oz-m2-")
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

    fn register_repo(&mut self, repo_root: &Path) {
        let git_dir = repo_root.join(".git");
        if !self.extra_git_dirs.contains(&git_dir) {
            self.extra_git_dirs.push(git_dir);
        }
    }

    fn register_mount(&mut self, path: PathBuf) {
        if !self.mount_points.contains(&path) {
            self.mount_points.push(path);
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
            if let Ok(proc_entries) = fs::read_dir("/proc") {
                for entry in proc_entries.flatten() {
                    let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    let Ok(pid) = name.parse::<u32>() else {
                        continue;
                    };
                    kill_test_vfsd_pid(pid, &root_marker);
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
        wait_pid_dead(pid, std::time::Duration::from_secs(2));
    }
}

#[cfg(target_os = "linux")]
fn wait_pid_dead(pid: u32, timeout: std::time::Duration) {
    let deadline = Instant::now() + timeout;
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    while Instant::now() < deadline {
        if !proc_dir.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
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
        .env("OZ_VFSD_BIN", ensure_oz_vfsd_bin().as_os_str())
        .stdin(Stdio::null());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("execute binary");
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let json = serde_json::from_str::<Value>(&stdout).ok();
    CmdRun {
        code,
        stdout,
        stderr,
        json,
    }
}

fn run_oz(cwd: &Path, args: &[&str]) -> CmdRun {
    run_bin(&oz_bin(), cwd, args, &[])
}

fn sys_git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
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

fn init_git_repo(repo: &Path) -> String {
    sys_git(repo, &["init", "-b", "main"]);
    sys_git(repo, &["config", "user.name", "Ozone M2 Gate"]);
    sys_git(repo, &["config", "user.email", "m2@ozone.dev"]);
    sys_git(repo, &["add", "."]);
    sys_git(repo, &["commit", "-m", "initial commit"]);
    sys_git(repo, &["rev-parse", "HEAD"])
}

fn write_repo_file(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).expect("mkdir parent");
    }
    fs::write(&p, content).expect("write file");
}

fn verify_op_ids_reachable_from_head(git_dir: &Path, expected_op_ids: &[String]) {
    let layout = OzoneLayout::from_git_common_dir(git_dir);
    let head_op = read_head_op_id(&layout)
        .expect("read oplog/HEAD")
        .expect("oplog/HEAD must be populated");
    let ops_dir = layout.oplog_dir().join("ops");

    // Walk the actual parent DAG from .git/ozone/oplog/HEAD.
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    visited.insert(head_op.clone());
    queue.push_back(head_op);

    while let Some(op_id) = queue.pop_front() {
        let raw = fs::read_to_string(ops_dir.join(format!("{op_id}.json")))
            .unwrap_or_else(|err| panic!("missing op entry {op_id}: {err}"));
        let entry: OpLogEntry = serde_json::from_str(&raw).expect("parse OpLogEntry");
        for parent in entry.parents {
            if visited.insert(parent.clone()) {
                queue.push_back(parent);
            }
        }
    }

    for op_id in expected_op_ids {
        assert!(
            visited.contains(op_id),
            "op_id '{op_id}' is not reachable from .git/ozone/oplog/HEAD DAG (visited={})",
            visited.len()
        );
    }

    let listed: HashSet<String> = list_entries(&layout)
        .expect("list_entries")
        .into_iter()
        .map(|e| e.op_id)
        .collect();
    for op_id in expected_op_ids {
        assert!(listed.contains(op_id), "op_id '{op_id}' missing from list_entries");
    }
}

#[test]
fn gate_m2_10_process_concurrent_oz_commit_stress() {
    let mut sandbox = GateSandbox::new("g1");
    let repo = sandbox.repo_root.clone();
    write_repo_file(&repo, "README.md", "# M2 Concurrent Stress Gate\n");
    write_repo_file(&repo, "src/base.rs", "pub fn base() -> u32 { 1 }\n");
    let _base_oid = init_git_repo(&repo);

    let mut ws_paths = Vec::with_capacity(10);
    let mut expected_op_ids = Vec::new();
    let mut observed_backends = HashSet::new();

    for i in 1..=10 {
        let ws_name = format!("agent-{i}");
        let branch = format!("feat/agent-{i}");
        let ws_dir = sandbox.ws_path(&ws_name);
        sandbox.register_mount(ws_dir.clone());

        let out = run_oz(
            &repo,
            &["ws", "spawn", &ws_name, "--branch", &branch, "--no-warm", "--json"],
        );
        assert_eq!(out.code, 0, "spawn {ws_name} failed: {}", out.stderr);
        let j = out.json.expect("spawn json");
        assert_eq!(j["schema"], OZ_SCHEMA_V1);
        assert_eq!(j["ok"], true);
        let op_id = j["op_id"].as_str().expect("spawn op_id").to_string();
        expected_op_ids.push(op_id);
        let backend = j["data"]["backend"].as_str().expect("backend").to_string();
        observed_backends.insert(backend);

        #[cfg(target_os = "linux")]
        {
            assert_eq!(j["data"]["backend"], "linux_fuse3");
            assert_eq!(j["data"]["mounted"], true);
        }

        ws_paths.push(ws_dir);
    }

    // Two barrier-synchronized concurrent rounds: 10 OS `oz commit --json` writers +
    // 2 concurrent `oz radar --json` / `oz status --json` readers = 12 threads per round.
    let mut round2_commit_oids = vec![String::new(); 10];
    for round in 1..=2 {
        for (idx, ws_dir) in ws_paths.iter().enumerate() {
            let i = idx + 1;
            write_repo_file(
                ws_dir,
                &format!("src/agent_{i}_r{round}.rs"),
                &format!("pub fn agent_{i}_r{round}() -> usize {{ {round} }}\n"),
            );
        }

        let barrier = Arc::new(Barrier::new(12));
        let mut commit_handles = Vec::with_capacity(10);
        for (idx, ws_dir) in ws_paths.iter().enumerate() {
            let i = idx + 1;
            let dir = ws_dir.clone();
            let b = Arc::clone(&barrier);
            commit_handles.push(std::thread::spawn(move || {
                let msg = format!("feat(agent-{i}): round {round}");
                b.wait();
                let res = run_oz(&dir, &["commit", "-m", &msg, "--json"]);
                (i, res)
            }));
        }

        let radar_repo = repo.clone();
        let radar_barrier = Arc::clone(&barrier);
        let radar_handle = std::thread::spawn(move || {
            radar_barrier.wait();
            run_oz(&radar_repo, &["radar", "--json"])
        });

        let status_ws = ws_paths[0].clone();
        let status_barrier = Arc::clone(&barrier);
        let status_handle = std::thread::spawn(move || {
            status_barrier.wait();
            run_oz(&status_ws, &["status", "--json"])
        });

        for h in commit_handles {
            let (i, res) = h.join().expect("join commit worker");
            assert_eq!(
                res.code, 0,
                "concurrent commit agent-{i} round {round} failed (stdout={}, stderr={})",
                res.stdout, res.stderr
            );
            let j = res.json.expect("commit json");
            assert_eq!(j["schema"], OZ_SCHEMA_V1);
            assert_eq!(j["ok"], true);
            let op_id = j["op_id"].as_str().expect("commit op_id").to_string();
            let commit_oid = j["data"]["commit_oid"]
                .as_str()
                .expect("commit_oid")
                .to_string();
            assert!(
                j["data"]["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("Change-Id: I")),
                "missing Change-Id trailer in agent-{i}"
            );
            expected_op_ids.push(op_id);
            if round == 2 {
                round2_commit_oids[i - 1] = commit_oid;
            }
        }

        let radar_res = radar_handle.join().expect("join radar reader");
        assert_eq!(radar_res.code, 0, "concurrent radar failed: {}", radar_res.stderr);
        assert_eq!(radar_res.json.expect("radar json")["ok"], true);

        let status_res = status_handle.join().expect("join status reader");
        assert_eq!(status_res.code, 0, "concurrent status failed: {}", status_res.stderr);
        assert_eq!(status_res.json.expect("status json")["ok"], true);
    }

    // Verify all 10 branch refs & commits intact + all 30 op_ids reachable in oplog/HEAD DAG.
    for (idx, expected_oid) in round2_commit_oids.iter().enumerate() {
        let i = idx + 1;
        let branch = format!("feat/agent-{i}");
        let actual_oid = sys_git(&repo, &["rev-parse", &format!("refs/heads/{branch}")]);
        assert_eq!(&actual_oid, expected_oid, "branch {branch} ref mismatch");

        let shown = sys_git(&repo, &["show", &format!("{branch}:src/agent_{i}_r2.rs")]);
        assert!(shown.contains(&format!("agent_{i}_r2")), "missing committed file for {branch}");
    }

    verify_op_ids_reachable_from_head(&repo.join(".git"), &expected_op_ids);
    println!(
        "[GATE M2-1] 20 concurrent commits across 10 workspaces ({observed_backends:?}) + concurrent readers: 0% lock errors, {} oplog ops reachable",
        expected_op_ids.len()
    );
}

#[test]
fn gate_m2_cross_os_doctor_and_setup_projfs_behavior() {
    let sandbox = GateSandbox::new("g2d");
    let repo = sandbox.repo_root.clone();
    write_repo_file(&repo, "README.md", "# M2 Cross-OS Doctor & Setup\n");
    let head_oid = init_git_repo(&repo);

    let doc = run_bin(&git_ozone_bin(), &repo, &["doctor", "--json"], &[]);
    assert_eq!(doc.code, 0, "doctor failed: {}", doc.stderr);
    let doc_json = doc.json.expect("doctor json");
    assert_eq!(doc_json["schema"], OZ_SCHEMA_V1);
    assert_eq!(doc_json["ok"], true);
    assert_eq!(doc_json["head_oid"], head_oid);
    assert_eq!(doc_json["data"]["oz_vfsd"]["found"], true);
    assert_eq!(doc_json["data"]["repo"]["in_repo"], true);

    #[cfg(target_os = "linux")]
    {
        assert!(doc_json["data"]["projfs_ready"].is_null());
        assert_eq!(doc_json["data"]["recommended_backend"], "linux_fuse3");
    }
    #[cfg(target_os = "windows")]
    {
        assert_eq!(doc_json["data"]["projfs_ready"], true);
        assert_eq!(doc_json["data"]["recommended_backend"], "windows_proj_fs");
    }

    // M1 setup compatibility: without `--enable-projfs`, `projfs_enabled` must be omitted.
    let plain_setup = run_oz(&repo, &["setup", "--json"]);
    assert_eq!(plain_setup.code, 0, "setup failed: {}", plain_setup.stderr);
    let plain_json = plain_setup.json.expect("plain setup json");
    assert_eq!(plain_json["ok"], true);
    assert!(
        plain_json["data"].get("projfs_enabled").is_none(),
        "projfs_enabled must stay omitted unless --enable-projfs is passed"
    );

    let projfs_setup = run_oz(&repo, &["setup", "--enable-projfs", "--json"]);
    #[cfg(target_os = "windows")]
    {
        assert_eq!(projfs_setup.code, 0, "windows setup --enable-projfs failed: {}", projfs_setup.stderr);
        let sj = projfs_setup.json.expect("windows setup json");
        assert_eq!(sj["ok"], true);
        assert_eq!(sj["data"]["projfs_enabled"], true);
        assert_eq!(sj["data"]["recommended_backend"], "windows_proj_fs");
    }
    #[cfg(not(target_os = "windows"))]
    {
        assert_eq!(
            projfs_setup.code, 2,
            "non-windows setup --enable-projfs must exit 2: {}",
            projfs_setup.stdout
        );
        let sj = projfs_setup.json.expect("non-windows setup json");
        assert_eq!(sj["ok"], false);
        assert_eq!(sj["error"]["code"], "invalid_usage");
        assert_eq!(sj["error"]["exit_code"], 2);
    }
}

#[cfg(target_os = "windows")]
#[test]
fn gate_m2_windows_ntfs_projfs_10_workspaces_space_and_speed() {
    use std::collections::HashSet as WinHashSet;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    #[allow(dead_code)]
    struct FileStandardInfo {
        allocation_size: i64,
        end_of_file: i64,
        number_of_links: u32,
        delete_pending: u8,
        directory: u8,
    }

    #[repr(C)]
    #[allow(dead_code)]
    #[derive(Default)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time: [u32; 2],
        last_access_time: [u32; 2],
        last_write_time: [u32; 2],
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    unsafe extern "system" {
        fn GetFileInformationByHandle(
            h_file: *mut core::ffi::c_void,
            lp_file_information: *mut ByHandleFileInformation,
        ) -> i32;
        fn GetFileInformationByHandleEx(
            h_file: *mut core::ffi::c_void,
            file_information_class: i32,
            lp_file_information: *mut core::ffi::c_void,
            dw_buffer_size: u32,
        ) -> i32;
    }

    const FILE_STANDARD_INFO_CLASS: i32 = 1;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    fn ntfs_file_id_and_alloc(path: &Path) -> Option<((u32, u64), u64)> {
        use std::os::windows::fs::MetadataExt;
        let meta = fs::symlink_metadata(path).ok()?;
        if (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            // ProjFS virtual placeholders are NTFS reparse points with 0 allocated data.
            return None;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .ok()?;
        let handle = file.as_raw_handle();
        let mut info = ByHandleFileInformation::default();
        if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
            return None;
        }
        let mut std_info = FileStandardInfo {
            allocation_size: 0,
            end_of_file: 0,
            number_of_links: 0,
            delete_pending: 0,
            directory: 0,
        };
        let alloc = if unsafe {
            GetFileInformationByHandleEx(
                handle,
                FILE_STANDARD_INFO_CLASS,
                (&mut std_info as *mut FileStandardInfo).cast(),
                std::mem::size_of::<FileStandardInfo>() as u32,
            )
        } != 0
        {
            std_info.allocation_size.max(0) as u64
        } else {
            meta.len()
        };
        let fid = ((info.file_index_high as u64) << 32) | (info.file_index_low as u64);
        Some(((info.volume_serial_number, fid), alloc))
    }

    fn collect_ntfs_ids(root: &Path) -> WinHashSet<(u32, u64)> {
        let mut out = WinHashSet::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                    stack.push(p);
                } else if let Some((id, _)) = ntfs_file_id_and_alloc(&p) {
                    out.insert(id);
                }
            }
        }
        out
    }

    fn ntfs_dedup_bytes(roots: &[PathBuf], exclude_ids: &WinHashSet<(u32, u64)>) -> u64 {
        let mut seen = WinHashSet::new();
        let mut total = 0u64;
        let mut stack = roots.to_vec();
        while let Some(cur) = stack.pop() {
            if cur.is_file() {
                if let Some((id, alloc)) = ntfs_file_id_and_alloc(&cur)
                    && !exclude_ids.contains(&id)
                    && seen.insert(id)
                {
                    total = total.saturating_add(alloc);
                }
                continue;
            }
            let Ok(rd) = fs::read_dir(&cur) else {
                continue;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                    stack.push(p);
                } else if let Some((id, alloc)) = ntfs_file_id_and_alloc(&p)
                    && !exclude_ids.contains(&id)
                    && seen.insert(id)
                {
                    total = total.saturating_add(alloc);
                }
            }
        }
        total
    }

    let mut sandbox = GateSandbox::new("g2w");
    let repo = sandbox.repo_root.clone();
    write_repo_file(&repo, ".gitignore", "/target\n/node_modules\n/.venv\n");
    write_repo_file(&repo, "README.md", "# Windows ProjFS Gate\n");
    write_repo_file(&repo, "src/lib.rs", "pub fn win() -> u32 { 7 }\n");
    let base_commit = init_git_repo(&repo);

    // Create >= 50 MB warm cache baseline (20 MiB + 20 MiB + 12 MiB = 52 MiB).
    for (rel, size, byte) in [
        ("target/debug/deps/libhuge.rlib", 20 * 1024 * 1024, 0xA1u8),
        ("node_modules/mega-pkg/bundle.bin", 20 * 1024 * 1024, 0xB2u8),
        (".venv/Lib/site-packages/weights.bin", 12 * 1024 * 1024, 0xC3u8),
    ] {
        let p = repo.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir warm");
        fs::write(&p, vec![byte; size]).expect("write warm blob");
    }

    let ozone_dir = repo.join(".git/ozone");
    fs::create_dir_all(&ozone_dir).expect("mkdir ozone");
    let baseline_meta =
        oz_warm::capture_baseline(&repo, &ozone_dir, &base_commit, &oz_core::OzoneConfig::default())
            .expect("capture warm baseline");
    assert!(
        baseline_meta.total_bytes >= 50 * 1024 * 1024,
        "warm baseline must be >= 50 MB (got {})",
        baseline_meta.total_bytes
    );

    let repo_ids = collect_ntfs_ids(&repo);
    let mut latencies_ms = Vec::with_capacity(10);
    let mut mount_roots = Vec::with_capacity(10);

    for i in 1..=10 {
        let ws_name = format!("agent-{i}");
        let branch = format!("feat/agent-{i}");
        let ws_path = sandbox.ws_path(&ws_name);
        sandbox.register_mount(ws_path.clone());
        mount_roots.push(ws_path);

        let t0 = Instant::now();
        let out = run_oz(&repo, &["ws", "spawn", &ws_name, "--branch", &branch, "--json"]);
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(out.code, 0, "ProjFS spawn {ws_name} failed: {}", out.stderr);
        let j = out.json.expect("spawn json");
        assert_eq!(j["ok"], true);
        assert_eq!(j["data"]["backend"], "windows_proj_fs");
        assert_eq!(j["data"]["mounted"], true);
        latencies_ms.push(elapsed_ms);
    }

    let max_ms = latencies_ms.iter().copied().fold(0.0_f64, f64::max);
    let mean_ms = latencies_ms.iter().sum::<f64>() / latencies_ms.len() as f64;
    assert!(
        max_ms < 500.0 && mean_ms < 500.0,
        "ProjFS spawn exceeded 500ms budget: mean={mean_ms:.2}ms max={max_ms:.2}ms all={latencies_ms:?}"
    );

    let mut backing_and_mounts = vec![
        ozone_dir.join("ws"),
        ozone_dir.join("baselines"),
        ozone_dir.join("state"),
        repo.join(".git/worktrees"),
    ];
    backing_and_mounts.extend(mount_roots.iter().cloned());
    let extra_bytes = ntfs_dedup_bytes(&backing_and_mounts, &repo_ids);
    println!(
        "[GATE M2-2 WIN PROJFS] 10x spawn mean={mean_ms:.2}ms max={max_ms:.2}ms extra_ntfs_bytes={extra_bytes}"
    );
    assert!(
        extra_bytes < 50 * 1024 * 1024,
        "10 ProjFS workspaces exceeded 50 MB extra disk: {extra_bytes} bytes"
    );

    // Verify virtual read + CoW write + oz commit on ProjFS workspace 1.
    let ws1 = &mount_roots[0];
    assert_eq!(fs::read_to_string(ws1.join("src/lib.rs")).unwrap(), "pub fn win() -> u32 { 7 }\n");
    write_repo_file(ws1, "src/lib.rs", "pub fn win() -> u32 { 99 }\n");
    let commit_out = run_oz(ws1, &["commit", "-m", "feat: projfs edit", "--json"]);
    assert_eq!(commit_out.code, 0, "ProjFS commit failed: {}", commit_out.stderr);
}

#[test]
fn gate_m2_guardrails_and_live_radar() {
    let mut sandbox = GateSandbox::new("g3");
    let repo = sandbox.repo_root.clone();

    let hundred_lines: String = (1..=100).map(|i| format!("line {i}\n")).collect();
    write_repo_file(&repo, "crates/auth/src/lib.rs", "pub fn auth() {}\n");
    write_repo_file(&repo, "crates/billing/src/lib.rs", "pub fn bill() {}\n");
    write_repo_file(&repo, "crates/core/src/session.rs", &hundred_lines);
    write_repo_file(&repo, "crates/core/src/other.rs", &hundred_lines);
    write_repo_file(&repo, "Cargo.lock", "lock-v1\n");
    let base_oid = init_git_repo(&repo);

    let ws_auth = sandbox.ws_path("ws-auth");
    let ws_bill = sandbox.ws_path("ws-bill");
    sandbox.register_mount(ws_auth.clone());
    sandbox.register_mount(ws_bill.clone());

    assert_eq!(
        run_oz(
            &repo,
            &[
                "ws",
                "spawn",
                "ws-auth",
                "--scope",
                "crates/auth/**",
                "crates/core/**",
                "--deny",
                "Cargo.lock",
                "--no-warm",
                "--json",
            ],
        )
        .code,
        0
    );
    assert_eq!(
        run_oz(
            &repo,
            &[
                "ws",
                "spawn",
                "ws-bill",
                "--scope",
                "crates/billing/**",
                "crates/core/**",
                "--deny",
                "Cargo.lock",
                "--no-warm",
                "--json",
            ],
        )
        .code,
        0
    );

    // 1. Out-of-scope + denied edits in ws-auth -> structured `out_of_scope` warnings in `oz status --json`.
    write_repo_file(&ws_auth, "crates/billing/src/lib.rs", "pub fn bill() { /* oos */ }\n");
    write_repo_file(&ws_auth, "Cargo.lock", "lock-v2-denied\n");

    let st = run_oz(&ws_auth, &["status", "--json"]);
    assert_eq!(st.code, 0, "status failed: {}", st.stderr);
    let st_json = st.json.expect("status json");
    let warnings = st_json["warnings"].as_array().expect("warnings array");
    assert!(
        warnings.iter().any(|w| {
            w["code"] == "out_of_scope" && w["path"] == "crates/billing/src/lib.rs"
        }),
        "missing out_of_scope structured warning for crates/billing/src/lib.rs: {warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w["code"] == "out_of_scope" && w["path"] == "Cargo.lock"),
        "missing out_of_scope structured warning for Cargo.lock: {warnings:?}"
    );

    // 2. Exit-2 guardrail block in `oz commit`, overridden with `--allow-out-of-scope`.
    let blocked_commit = run_oz(&ws_auth, &["commit", "-m", "oos blocked", "--json"]);
    assert_eq!(blocked_commit.code, 2, "expected exit 2: {}", blocked_commit.stdout);
    let bc_json = blocked_commit.json.expect("blocked commit json");
    assert_eq!(bc_json["ok"], false);
    assert_eq!(bc_json["error"]["code"], "guardrail_violation");

    let override_commit = run_oz(
        &ws_auth,
        &[
            "commit",
            "-m",
            "oos committed with override",
            "--allow-out-of-scope",
            "--json",
        ],
    );
    assert_eq!(override_commit.code, 0, "override commit failed: {}", override_commit.stderr);

    // 3. Exit-2 guardrail block in `oz integrate`, overridden with `--allow-out-of-scope`, then undone.
    let blocked_int = run_oz(&repo, &["integrate", "ws-auth", "--into", "main", "--json"]);
    assert_eq!(blocked_int.code, 2, "expected exit 2 on integrate: {}", blocked_int.stdout);
    let bi_json = blocked_int.json.expect("blocked integrate json");
    assert_eq!(bi_json["ok"], false);
    assert_eq!(bi_json["error"]["code"], "guardrail_violation");
    assert_eq!(sys_git(&repo, &["rev-parse", "main"]), base_oid);

    let override_int = run_oz(
        &repo,
        &[
            "integrate",
            "ws-auth",
            "--into",
            "main",
            "--allow-out-of-scope",
            "--json",
        ],
    );
    assert_eq!(override_int.code, 0, "override integrate failed: {}", override_int.stderr);
    assert_ne!(sys_git(&repo, &["rev-parse", "main"]), base_oid);

    let undo_int = run_oz(&repo, &["undo", "--json"]);
    assert_eq!(undo_int.code, 0, "undo integrate failed: {}", undo_int.stderr);
    assert_eq!(sys_git(&repo, &["rev-parse", "main"]), base_oid);

    // 4. Live `oz radar --json`: overlapping_hunks ("42-68" & "50-75"), same_file_different_lines, and out_of_scope.
    let mut auth_session: Vec<String> = (1..=100).map(|i| format!("line {i}\n")).collect();
    for (idx, line) in auth_session.iter_mut().enumerate().take(68).skip(41) {
        *line = format!("auth line {}\n", idx + 1);
    }
    write_repo_file(&ws_auth, "crates/core/src/session.rs", &auth_session.concat());
    let mut auth_other: Vec<String> = (1..=100).map(|i| format!("line {i}\n")).collect();
    auth_other[4] = "auth other line 5\n".to_string();
    write_repo_file(&ws_auth, "crates/core/src/other.rs", &auth_other.concat());

    let mut bill_session: Vec<String> = (1..=100).map(|i| format!("line {i}\n")).collect();
    for (idx, line) in bill_session.iter_mut().enumerate().take(75).skip(49) {
        *line = format!("bill line {}\n", idx + 1);
    }
    write_repo_file(&ws_bill, "crates/core/src/session.rs", &bill_session.concat());
    let mut bill_other: Vec<String> = (1..=100).map(|i| format!("line {i}\n")).collect();
    bill_other[79] = "bill other line 80\n".to_string();
    write_repo_file(&ws_bill, "crates/core/src/other.rs", &bill_other.concat());
    // Commit ws-bill edits while keeping ws-auth edits dirty so radar blends committed + uncommitted.
    assert_eq!(
        run_oz(&ws_bill, &["commit", "-m", "bill edits", "--json"]).code,
        0
    );

    let radar = run_oz(&repo, &["radar", "--json"]);
    assert_eq!(radar.code, 0, "radar failed: {}", radar.stderr);
    let rj = radar.json.expect("radar json");
    assert_eq!(rj["schema"], OZ_SCHEMA_V1);
    assert_eq!(rj["ok"], true);
    let collisions = rj["data"]["collisions"].as_array().expect("collisions array");

    let session_col = collisions
        .iter()
        .find(|c| c["path"] == "crates/core/src/session.rs")
        .expect("missing overlapping_hunks collision on session.rs");
    assert_eq!(session_col["severity"], "overlapping_hunks");
    let spans = session_col["workspaces"].as_array().expect("spans");
    assert!(
        spans.iter().any(|w| w["ws"] == "ws-auth" && w["lines"] == "42-68"),
        "expected ws-auth 42-68 span in {spans:?}"
    );
    assert!(
        spans.iter().any(|w| w["ws"] == "ws-bill" && w["lines"] == "50-75"),
        "expected ws-bill 50-75 span in {spans:?}"
    );

    let other_col = collisions
        .iter()
        .find(|c| c["path"] == "crates/core/src/other.rs")
        .expect("missing same_file_different_lines collision on other.rs");
    assert_eq!(other_col["severity"], "same_file_different_lines");

    let lock_col = collisions
        .iter()
        .find(|c| c["path"] == "Cargo.lock")
        .expect("missing out_of_scope collision on Cargo.lock");
    assert_eq!(lock_col["severity"], "out_of_scope");

    // `--no-committed` excludes committed-only ws-bill changes while retaining uncommitted ws-auth.
    write_repo_file(&ws_auth, "Cargo.lock", "lock-v3-dirty\n");
    let radar_dirty_only = run_oz(&repo, &["radar", "--no-committed", "--json"]);
    assert_eq!(radar_dirty_only.code, 0);
    let rj_dirty = radar_dirty_only.json.expect("radar --no-committed json");
    let dirty_cols = rj_dirty["data"]["collisions"].as_array().expect("dirty collisions");
    assert!(
        !dirty_cols
            .iter()
            .any(|c| c["path"] == "crates/core/src/session.rs"),
        "ws-bill committed edit should be excluded by --no-committed: {dirty_cols:?}"
    );
    assert!(
        dirty_cols.iter().any(|c| c["path"] == "Cargo.lock"),
        "uncommitted out_of_scope edit should remain with --no-committed: {dirty_cols:?}"
    );
}

#[test]
fn gate_m2_integrate_sandbox_resolve_verify_and_stack_rebase() {
    let mut sandbox = GateSandbox::new("g4");
    let repo = sandbox.repo_root.clone();
    write_repo_file(&repo, "shared.txt", "base-line\n");
    write_repo_file(&repo, "app.txt", "base-app\n");
    let main_oid_0 = init_git_repo(&repo);

    // 1. Clean multi-ws integrate + --verify pass + < 10 ms oz undo.
    let ws1 = sandbox.ws_path("ws-1");
    let ws2 = sandbox.ws_path("ws-2");
    sandbox.register_mount(ws1.clone());
    sandbox.register_mount(ws2.clone());
    assert_eq!(run_oz(&repo, &["ws", "spawn", "ws-1", "--no-warm", "--json"]).code, 0);
    assert_eq!(run_oz(&repo, &["ws", "spawn", "ws-2", "--no-warm", "--json"]).code, 0);

    write_repo_file(&ws1, "f1.txt", "ws1-ok\n");
    assert_eq!(run_oz(&ws1, &["commit", "-m", "ws1 f1", "--json"]).code, 0);
    write_repo_file(&ws2, "f2.txt", "ws2-ok\n");
    assert_eq!(run_oz(&ws2, &["commit", "-m", "ws2 f2", "--json"]).code, 0);

    let verify_both = if cfg!(windows) {
        "findstr /C:\"ws1-ok\" f1.txt >NUL && findstr /C:\"ws2-ok\" f2.txt >NUL"
    } else {
        "grep -q ws1-ok f1.txt && grep -q ws2-ok f2.txt"
    };
    let int_ok = run_oz(
        &repo,
        &[
            "integrate",
            "ws-1",
            "ws-2",
            "--into",
            "main",
            "--verify",
            verify_both,
            "--json",
        ],
    );
    assert_eq!(int_ok.code, 0, "clean integrate failed: stdout={} stderr={}", int_ok.stdout, int_ok.stderr);
    let ij = int_ok.json.expect("integrate json");
    assert_eq!(ij["schema"], OZ_SCHEMA_V1);
    assert_eq!(ij["ok"], true);
    assert_eq!(ij["data"]["merged_sources"], serde_json::json!(["ws-1", "ws-2"]));
    assert_eq!(ij["data"]["main_worktree_synced"], true);
    assert_eq!(fs::read_to_string(repo.join("f1.txt")).unwrap(), "ws1-ok\n");
    assert_eq!(fs::read_to_string(repo.join("f2.txt")).unwrap(), "ws2-ok\n");
    assert!(!sandbox.ws_path("_integrate").exists());
    assert_eq!(sys_git(&repo, &["status", "--porcelain"]), "");

    let undo_int = run_oz(&repo, &["undo", "--json"]);
    assert_eq!(undo_int.code, 0, "undo integrate failed: {}", undo_int.stderr);
    let uij = undo_int.json.expect("undo integrate json");
    assert_eq!(uij["ok"], true);
    let undo_ms = uij["data"]["engine_elapsed_ms"].as_f64().expect("engine_elapsed_ms");
    assert!(undo_ms < 10.0, "integrate undo took {undo_ms:.2}ms (>= 10ms)");
    assert_eq!(sys_git(&repo, &["rev-parse", "main"]), main_oid_0);
    assert!(!repo.join("f1.txt").exists());
    assert!(!repo.join("f2.txt").exists());

    // 2. Failing --verify leaves `main` 100% untouched (exit 1).
    let int_fail = run_oz(
        &repo,
        &["integrate", "ws-1", "--into", "main", "--verify", "exit 1", "--json"],
    );
    assert_eq!(int_fail.code, 1, "expected exit 1 on verify fail: {}", int_fail.stdout);
    let ifj = int_fail.json.expect("verify fail json");
    assert_eq!(ifj["ok"], false);
    assert_eq!(ifj["error"]["code"], "check_failed");
    assert_eq!(ifj["error"]["exit_code"], 1);
    assert_eq!(sys_git(&repo, &["rev-parse", "main"]), main_oid_0);
    assert!(!sandbox.ws_path("_integrate").exists());
    assert!(!repo.join("f1.txt").exists());

    // 3. Conflicting integrate -> `_integrate/` sandbox with c1 -> `oz resolve c1 --take ours` -> `oz integrate --continue --verify "..."`.
    let ws_c1 = sandbox.ws_path("ws-c1");
    let ws_c2 = sandbox.ws_path("ws-c2");
    sandbox.register_mount(ws_c1.clone());
    sandbox.register_mount(ws_c2.clone());
    assert_eq!(run_oz(&repo, &["ws", "spawn", "ws-c1", "--no-warm", "--json"]).code, 0);
    assert_eq!(run_oz(&repo, &["ws", "spawn", "ws-c2", "--no-warm", "--json"]).code, 0);

    write_repo_file(&ws_c1, "shared.txt", "ours-from-c1\n");
    assert_eq!(run_oz(&ws_c1, &["commit", "-m", "c1 edit", "--json"]).code, 0);
    let c1_branch_oid = sys_git(&repo, &["rev-parse", "refs/heads/ws-c1"]);

    write_repo_file(&ws_c2, "shared.txt", "theirs-from-c2\n");
    assert_eq!(run_oz(&ws_c2, &["commit", "-m", "c2 edit", "--json"]).code, 0);
    let c2_branch_oid = sys_git(&repo, &["rev-parse", "refs/heads/ws-c2"]);

    let conflict_out = run_oz(&repo, &["integrate", "ws-c1", "ws-c2", "--into", "main", "--json"]);
    assert_eq!(conflict_out.code, 1, "expected conflict exit 1: {}", conflict_out.stdout);
    let cj = conflict_out.json.expect("conflict json");
    assert_eq!(cj["ok"], false);
    assert_eq!(cj["error"]["code"], "conflict");
    assert_eq!(cj["error"]["exit_code"], 1);
    let c1 = &cj["conflicts"][0];
    assert_eq!(c1["id"], "c1");
    assert_eq!(c1["path"], "shared.txt");
    assert_eq!(c1["base"], "base-line\n");
    assert_eq!(c1["ours"], "ours-from-c1\n");
    assert_eq!(c1["theirs"], "theirs-from-c2\n");

    let sandbox_integrate = sandbox.ws_path("_integrate");
    assert!(sandbox_integrate.is_dir(), "_integrate sandbox must be materialized");
    let sandbox_shared = fs::read_to_string(sandbox_integrate.join("shared.txt")).unwrap();
    assert!(sandbox_shared.contains("<<<<<<<"), "expected conflict markers in _integrate/shared.txt");

    // `main` and agent worktrees remain 100% untouched.
    assert_eq!(sys_git(&repo, &["rev-parse", "main"]), main_oid_0);
    assert_eq!(fs::read_to_string(repo.join("shared.txt")).unwrap(), "base-line\n");
    assert_eq!(sys_git(&repo, &["rev-parse", "refs/heads/ws-c1"]), c1_branch_oid);
    assert_eq!(sys_git(&repo, &["rev-parse", "refs/heads/ws-c2"]), c2_branch_oid);
    assert_eq!(fs::read_to_string(ws_c1.join("shared.txt")).unwrap(), "ours-from-c1\n");
    assert_eq!(fs::read_to_string(ws_c2.join("shared.txt")).unwrap(), "theirs-from-c2\n");

    let resolved = run_oz(&repo, &["resolve", "c1", "--take", "ours", "--json"]);
    assert_eq!(resolved.code, 0, "resolve c1 failed: {}", resolved.stderr);
    let rj = resolved.json.expect("resolve json");
    assert_eq!(rj["ok"], true);
    assert_eq!(rj["data"]["resolved_ids"], serde_json::json!(["c1"]));
    assert_eq!(rj["data"]["remaining_unresolved"], serde_json::json!([]));

    let verify_ours = if cfg!(windows) {
        "findstr /C:\"ours-from-c1\" shared.txt >NUL"
    } else {
        "grep -q ours-from-c1 shared.txt"
    };
    let continued = run_oz(
        &repo,
        &[
            "integrate",
            "--continue",
            "--verify",
            verify_ours,
            "--json",
        ],
    );
    assert_eq!(continued.code, 0, "integrate --continue failed: stdout={} stderr={}", continued.stdout, continued.stderr);
    let cont_j = continued.json.expect("continue json");
    assert_eq!(cont_j["ok"], true);
    assert_ne!(cont_j["data"]["commit_oid"], main_oid_0);
    assert_eq!(fs::read_to_string(repo.join("shared.txt")).unwrap(), "ours-from-c1\n");
    assert!(!sandbox_integrate.exists());

    // Also verify `oz integrate --abort --json`.
    let abort_pre_main = sys_git(&repo, &["rev-parse", "main"]);
    let ws_a1 = sandbox.ws_path("ws-a1");
    let ws_a2 = sandbox.ws_path("ws-a2");
    sandbox.register_mount(ws_a1.clone());
    sandbox.register_mount(ws_a2.clone());
    assert_eq!(run_oz(&repo, &["ws", "spawn", "ws-a1", "--no-warm", "--json"]).code, 0);
    assert_eq!(run_oz(&repo, &["ws", "spawn", "ws-a2", "--no-warm", "--json"]).code, 0);
    write_repo_file(&ws_a1, "app.txt", "abort-a1\n");
    assert_eq!(run_oz(&ws_a1, &["commit", "-m", "a1", "--json"]).code, 0);
    write_repo_file(&ws_a2, "app.txt", "abort-a2\n");
    assert_eq!(run_oz(&ws_a2, &["commit", "-m", "a2", "--json"]).code, 0);
    assert_eq!(
        run_oz(&repo, &["integrate", "ws-a1", "ws-a2", "--into", "main", "--json"]).code,
        1
    );
    assert!(sandbox_integrate.is_dir());
    let aborted = run_oz(&repo, &["integrate", "--abort", "--json"]);
    assert_eq!(aborted.code, 0, "integrate --abort failed: {}", aborted.stderr);
    assert!(!sandbox_integrate.exists());
    assert_eq!(sys_git(&repo, &["rev-parse", "main"]), abort_pre_main);

    // 4. `oz stack rebase` preserving `Change-Id:` trailers + `oz stack status --json` + `< 10 ms` `oz undo`.
    let ws_s1 = sandbox.ws_path("stack-1");
    let ws_s2 = sandbox.ws_path("stack-2");
    sandbox.register_mount(ws_s1.clone());
    sandbox.register_mount(ws_s2.clone());
    assert_eq!(
        run_oz(
            &repo,
            &["ws", "spawn", "stack-1", "--branch", "stack-1", "--from", "main", "--no-warm", "--json"],
        )
        .code,
        0
    );
    write_repo_file(&ws_s1, "stack1.txt", "s1\n");
    let cs1 = run_oz(&ws_s1, &["commit", "-m", "feat: stack 1", "--json"]);
    assert_eq!(cs1.code, 0);
    let cid1 = cs1.json.expect("cs1 json")["data"]["change_id"]
        .as_str()
        .expect("cid1")
        .to_string();

    assert_eq!(
        run_oz(
            &repo,
            &["ws", "spawn", "stack-2", "--branch", "stack-2", "--from", "stack-1", "--no-warm", "--json"],
        )
        .code,
        0
    );
    write_repo_file(&ws_s2, "stack2.txt", "s2\n");
    let cs2 = run_oz(&ws_s2, &["commit", "-m", "feat: stack 2", "--json"]);
    assert_eq!(cs2.code, 0);
    let cid2 = cs2.json.expect("cs2 json")["data"]["change_id"]
        .as_str()
        .expect("cid2")
        .to_string();

    // Advance `main` with a non-conflicting commit.
    write_repo_file(&repo, "main_advance.txt", "main moved\n");
    let cm = run_oz(&repo, &["commit", "-m", "chore: advance main", "--json"]);
    assert_eq!(cm.code, 0);
    let new_main_oid = sys_git(&repo, &["rev-parse", "main"]);
    let orig_s1 = sys_git(&repo, &["rev-parse", "stack-1"]);
    let orig_s2 = sys_git(&repo, &["rev-parse", "stack-2"]);

    let st_stack = run_oz(&repo, &["stack", "status", "--json"]);
    assert_eq!(st_stack.code, 0, "stack status failed: {}", st_stack.stderr);
    let ssj = st_stack.json.expect("stack status json");
    assert_eq!(ssj["ok"], true);
    let s1_status = ssj["data"]["branches"]
        .as_array()
        .expect("branches")
        .iter()
        .find(|b| b["branch"] == "stack-1")
        .expect("stack-1 in stack status");
    assert_eq!(s1_status["needs_rebase"], true);

    let rebased = run_oz(
        &repo,
        &["stack", "rebase", "stack-1", "stack-2", "--onto", "main", "--json"],
    );
    assert_eq!(rebased.code, 0, "stack rebase failed: stdout={} stderr={}", rebased.stdout, rebased.stderr);
    let rbj = rebased.json.expect("stack rebase json");
    assert_eq!(rbj["ok"], true);

    let rebased_cids: Vec<String> = rbj["data"]["rebased_commits"]
        .as_array()
        .expect("rebased_commits")
        .iter()
        .map(|c| c["change_id"].as_str().expect("cid").to_string())
        .collect();
    assert_eq!(rebased_cids, vec![cid1.clone(), cid2.clone()]);
    assert!(sys_git(&repo, &["log", "-1", "--pretty=%B", "stack-1"]).contains(&format!("Change-Id: {cid1}")));
    assert!(sys_git(&repo, &["log", "-1", "--pretty=%B", "stack-2"]).contains(&format!("Change-Id: {cid2}")));

    let new_s1_oid = sys_git(&repo, &["rev-parse", "stack-1"]);
    assert_eq!(sys_git(&repo, &["merge-base", "main", "stack-1"]), new_main_oid);
    assert_eq!(sys_git(&repo, &["merge-base", "stack-1", "stack-2"]), new_s1_oid);

    let undo_rb = run_oz(&repo, &["undo", "--json"]);
    assert_eq!(undo_rb.code, 0, "undo stack rebase failed: {}", undo_rb.stderr);
    let urj = undo_rb.json.expect("undo stack rebase json");
    let undo_rb_ms = urj["data"]["engine_elapsed_ms"].as_f64().expect("ms");
    assert!(undo_rb_ms < 10.0, "stack rebase undo took {undo_rb_ms:.2}ms (>= 10ms)");
    assert_eq!(sys_git(&repo, &["rev-parse", "stack-1"]), orig_s1);
    assert_eq!(sys_git(&repo, &["rev-parse", "stack-2"]), orig_s2);
}

#[test]
fn gate_m2_git_shim_and_reset_hard_undo() {
    let mut sandbox = GateSandbox::new("g5");
    let remote = sandbox.root.join("origin.git");
    fs::create_dir_all(&remote).expect("mkdir bare remote");
    sys_git(&remote, &["init", "--bare", "-b", "main"]);

    let seed = sandbox.repo_root.clone();
    write_repo_file(&seed, "README.md", "# Shim Gate\n");
    write_repo_file(&seed, "a.rs", "pub fn a() -> i32 { 1 }\n");
    write_repo_file(&seed, "b.rs", "pub fn b() -> i32 { 1 }\n");
    init_git_repo(&seed);
    sys_git(&seed, &["remote", "add", "origin", remote.to_str().unwrap()]);
    sys_git(&seed, &["push", "-u", "origin", "main"]);

    // 1. `oz git clone` auto-initializes `.git/ozone/`.
    let clone_out = run_oz(
        &sandbox.root,
        &["git", "clone", remote.to_str().unwrap(), "cloned", "--json"],
    );
    assert_eq!(clone_out.code, 0, "git clone failed: {}", clone_out.stderr);
    let cj = clone_out.json.expect("clone json");
    assert_eq!(cj["schema"], OZ_SCHEMA_V1);
    assert_eq!(cj["ok"], true);

    let cloned = sandbox.root.join("cloned");
    sandbox.register_repo(&cloned);
    assert!(cloned.join(".git/ozone/oplog").is_dir());
    assert!(cloned.join(".git/ozone/state").is_dir());
    sys_git(&cloned, &["config", "user.name", "Shim User"]);
    sys_git(&cloned, &["config", "user.email", "shim@ozone.dev"]);

    // 2. `oz git add` (virtual pinned set, zero `.git/index.lock`), `status`, `diff --cached`, `commit`.
    write_repo_file(&cloned, "a.rs", "pub fn a() -> i32 { 2 }\n");
    write_repo_file(&cloned, "b.rs", "pub fn b() -> i32 { 2 }\n");
    write_repo_file(&cloned, "new_a.rs", "pub fn na() {}\n");

    let add_out = run_oz(&cloned, &["git", "add", "a.rs"]);
    assert_eq!(add_out.code, 0, "git add failed: {}", add_out.stderr);
    assert!(cloned.join(".git/ozone/pinned_main.json").is_file());
    assert!(!cloned.join(".git/index.lock").exists());

    let porcelain = run_oz(&cloned, &["git", "status", "--porcelain"]);
    assert_eq!(porcelain.code, 0);
    assert!(porcelain.stdout.contains("M  a.rs"), "got:\n{}", porcelain.stdout);
    assert!(porcelain.stdout.contains(" M b.rs"), "got:\n{}", porcelain.stdout);
    assert!(porcelain.stdout.contains("?? new_a.rs"), "got:\n{}", porcelain.stdout);

    let diff_cached = run_oz(&cloned, &["git", "diff", "--cached", "--name-only"]);
    assert_eq!(diff_cached.code, 0);
    assert_eq!(diff_cached.stdout.lines().collect::<Vec<_>>(), vec!["a.rs"]);

    let commit_pinned = run_oz(&cloned, &["git", "commit", "-m", "feat: commit pinned a", "--json"]);
    assert_eq!(commit_pinned.code, 0, "git commit failed: {}", commit_pinned.stderr);
    let cpj = commit_pinned.json.expect("commit json");
    assert_eq!(cpj["data"]["committed_paths"], serde_json::json!(["a.rs"]));
    assert_eq!(cpj["data"]["remaining_dirty_count"], 2);

    // 3. `oz git switch -c` + `oz git commit`.
    let sw = run_oz(&cloned, &["git", "switch", "-c", "feat/shim", "--json"]);
    assert_eq!(sw.code, 0, "git switch -c failed: {}", sw.stderr);
    let commit_feat = run_oz(&cloned, &["git", "commit", "-m", "feat: shim branch", "--json"]);
    assert_eq!(commit_feat.code, 0, "git commit on feat/shim failed: {}", commit_feat.stderr);
    let feat_cid = commit_feat.json.expect("feat commit json")["data"]["change_id"]
        .as_str()
        .expect("feat cid")
        .to_string();

    // 4. `oz git worktree add` / `list` / `remove`.
    let wt_shim_path = sandbox.root.join("cloned.oz/wt-shim");
    sandbox.register_mount(wt_shim_path.clone());
    let wt_add = run_oz(
        &cloned,
        &["git", "worktree", "add", "-b", "feat/wt-shim", "wt-shim", "main", "--json"],
    );
    assert_eq!(wt_add.code, 0, "git worktree add failed: {}", wt_add.stderr);
    let wt_list = run_oz(&cloned, &["git", "worktree", "list", "--json"]);
    assert_eq!(wt_list.code, 0);
    assert_eq!(wt_list.json.expect("wt list")["data"].as_array().unwrap().len(), 2);
    let wt_rm = run_oz(&cloned, &["git", "worktree", "remove", "-f", "wt-shim", "--json"]);
    assert_eq!(wt_rm.code, 0, "git worktree remove failed: {}", wt_rm.stderr);

    // 5. `oz git merge` (via `oz_integrate::integrate`) + `oz undo`.
    assert_eq!(run_oz(&cloned, &["git", "switch", "main", "--json"]).code, 0);
    write_repo_file(&cloned, "main_only.txt", "main line\n");
    assert_eq!(
        run_oz(&cloned, &["git", "commit", "-m", "feat: main advance", "--json"]).code,
        0
    );
    let main_before_merge = sys_git(&cloned, &["rev-parse", "HEAD"]);

    let merge_out = run_oz(
        &cloned,
        &["git", "merge", "--no-ff", "feat/shim", "-m", "merge feat/shim", "--json"],
    );
    assert_eq!(merge_out.code, 0, "git merge failed: stdout={} stderr={}", merge_out.stdout, merge_out.stderr);
    let mj = merge_out.json.expect("merge json");
    assert_eq!(mj["ok"], true);
    assert_eq!(mj["data"]["merged_sources"], serde_json::json!(["feat/shim"]));
    assert!(cloned.join("new_a.rs").is_file());

    let undo_merge = run_oz(&cloned, &["undo", "--json"]);
    assert_eq!(undo_merge.code, 0);
    assert!(undo_merge.json.expect("undo merge")["data"]["engine_elapsed_ms"].as_f64().unwrap() < 10.0);
    assert_eq!(sys_git(&cloned, &["rev-parse", "HEAD"]), main_before_merge);
    assert!(!cloned.join("new_a.rs").exists());

    // 6. `oz git rebase` (via `oz_integrate::stack_rebase`) preserving `Change-Id:` + `oz undo`.
    assert_eq!(run_oz(&cloned, &["git", "switch", "feat/shim", "--json"]).code, 0);
    let feat_before_rebase = sys_git(&cloned, &["rev-parse", "HEAD"]);

    let rebase_out = run_oz(&cloned, &["git", "rebase", "main", "--json"]);
    assert_eq!(rebase_out.code, 0, "git rebase failed: stdout={} stderr={}", rebase_out.stdout, rebase_out.stderr);
    let rj = rebase_out.json.expect("rebase json");
    assert_eq!(rj["ok"], true);
    assert_eq!(rj["data"]["onto_branch"], "main");
    assert_eq!(
        rj["data"]["rebased_commits"][0]["change_id"].as_str(),
        Some(feat_cid.as_str())
    );
    assert!(cloned.join("main_only.txt").is_file());

    let undo_rebase = run_oz(&cloned, &["undo", "--json"]);
    assert_eq!(undo_rebase.code, 0);
    assert!(undo_rebase.json.expect("undo rebase")["data"]["engine_elapsed_ms"].as_f64().unwrap() < 10.0);
    assert_eq!(sys_git(&cloned, &["rev-parse", "HEAD"]), feat_before_rebase);
    assert!(!cloned.join("main_only.txt").exists());

    // 7. Destructive `oz git reset --hard HEAD~1` completely reversed by `oz undo --json` in < 10 ms.
    assert_eq!(run_oz(&cloned, &["git", "switch", "main", "--json"]).code, 0);
    write_repo_file(&cloned, "c2_file.txt", "born in c2\n");
    write_repo_file(&cloned, "a.rs", "pub fn a() -> i32 { 200 }\n");
    assert_eq!(
        run_oz(&cloned, &["git", "commit", "-m", "feat: commit c2", "--json"]).code,
        0
    );
    let c2_oid = sys_git(&cloned, &["rev-parse", "HEAD"]);

    // Uncommitted edits (both tracked + untracked) that `reset --hard` will wipe / leave behind.
    write_repo_file(&cloned, "a.rs", "precious dirty edit\n");
    write_repo_file(&cloned, "precious_untracked.txt", "precious untracked\n");
    write_repo_file(&cloned, "scratch/note.md", "precious nested\n");

    let reset_hard = run_oz(&cloned, &["git", "reset", "--hard", "HEAD~1"]);
    assert_eq!(reset_hard.code, 0, "git reset --hard failed: {}", reset_hard.stderr);
    assert_ne!(sys_git(&cloned, &["rev-parse", "HEAD"]), c2_oid);
    assert!(!cloned.join("c2_file.txt").exists());
    assert_eq!(
        fs::read_to_string(cloned.join("a.rs")).unwrap(),
        "pub fn a() -> i32 { 2 }\n"
    );

    let undo_reset = run_oz(&cloned, &["undo", "--json"]);
    assert_eq!(undo_reset.code, 0, "undo reset --hard failed: {}", undo_reset.stderr);
    let urj = undo_reset.json.expect("undo reset json");
    assert_eq!(urj["ok"], true);
    let reset_undo_ms = urj["data"]["engine_elapsed_ms"].as_f64().expect("ms");
    assert!(
        reset_undo_ms < 10.0,
        "reset --hard undo took {reset_undo_ms:.2}ms (>= 10ms)"
    );
    assert_eq!(sys_git(&cloned, &["rev-parse", "HEAD"]), c2_oid);
    assert_eq!(fs::read_to_string(cloned.join("c2_file.txt")).unwrap(), "born in c2\n");
    assert_eq!(fs::read_to_string(cloned.join("a.rs")).unwrap(), "precious dirty edit\n");
    assert_eq!(
        fs::read_to_string(cloned.join("precious_untracked.txt")).unwrap(),
        "precious untracked\n"
    );
    assert_eq!(
        fs::read_to_string(cloned.join("scratch/note.md")).unwrap(),
        "precious nested\n"
    );
    let porcelain_after_undo = sys_git(&cloned, &["status", "--porcelain"]);
    assert!(
        porcelain_after_undo.lines().any(|l| l.trim() == "M a.rs"),
        "a.rs should be dirty after undo: {porcelain_after_undo}"
    );
    assert!(
        !porcelain_after_undo.contains("c2_file.txt"),
        "c2_file.txt should be clean in index after undo: {porcelain_after_undo}"
    );
}
