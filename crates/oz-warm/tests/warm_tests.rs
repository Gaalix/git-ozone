// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for `oz-warm`.

use chrono::{Duration as ChronoDuration, Utc};
use filetime::FileTime;
use fs4::FileExt;
use oz_core::OzoneConfig;
use oz_warm::{
    BaselineMetadata, BaselineSource, apply_baseline_mtimes, capture_baseline,
    check_cargo_fingerprint_portability, compute_changed_paths_between_commits,
    detect_warm_targets, detect_warm_targets_with_cargo_target_dir, find_baseline, gc_baselines,
    is_repo_quiescent, resolve_or_capture_baseline, touch_baseline_lru,
};
use std::fs::OpenOptions;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

fn init_repo(dir: &Path) -> String {
    let ok = Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(dir)
        .output()
        .expect("git init")
        .status
        .success();
    assert!(ok);
    Command::new("git")
        .args(["config", "user.name", "Ozone Bot"])
        .current_dir(dir)
        .output()
        .expect("git config name");
    Command::new("git")
        .args(["config", "user.email", "bot@ozone.dev"])
        .current_dir(dir)
        .output()
        .expect("git config email");
    commit_all(dir, "init")
}

fn commit_all(dir: &Path, msg: &str) -> String {
    let add = Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir)
        .output()
        .expect("git add");
    assert!(add.status.success());
    let commit = Command::new("git")
        .args(["commit", "--allow-empty", "-m", msg])
        .current_dir(dir)
        .output()
        .expect("git commit");
    assert!(
        commit.status.success(),
        "{}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let rev = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .expect("rev-parse");
    String::from_utf8_lossy(&rev.stdout).trim().to_string()
}

#[cfg(unix)]
fn run_script_retry_etxtbsy(script: &Path) -> std::process::Output {
    // On Linux, a concurrent test thread calling `fork()` while another thread briefly holds a
    // writable FD during `std::fs::copy` can transiently trigger ETXTBSY (`ExecutableFileBusy`)
    // until the forked child completes `exec()` and closes its `O_CLOEXEC` descriptors.
    for attempt in 0..20 {
        match Command::new(script).output() {
            Ok(out) => return out,
            Err(err)
                if err.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt + 1 < 20 =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("run {}: {err}", script.display()),
        }
    }
    unreachable!()
}

fn find_system_python3() -> Option<std::path::PathBuf> {
    if let Some(path_env) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_env) {
            if cfg!(target_os = "macos") && dir == Path::new("/usr/bin") {
                continue;
            }
            let p = dir.join("python3");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    for cand in [
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
        "/usr/bin/python3",
        "/bin/python3",
    ] {
        let p = Path::new(cand);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    None
}

fn python_major_minor_dir(python: &Path) -> String {
    let out = Command::new(python)
        .args([
            "-c",
            "import sys; print(f'python{sys.version_info.major}.{sys.version_info.minor}')",
        ])
        .output()
        .expect("query python version");
    assert!(out.status.success(), "python3 --version query failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[cfg(unix)]
#[test]
fn venv_trampoline_rewrites_and_executes_from_cloned_workspace_path() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let Some(sys_python) = find_system_python3() else {
        return;
    };

    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("py-app");
    let ozone_dir = repo_root.join(".git/ozone");
    std::fs::create_dir_all(&repo_root).expect("mkdir repo");
    std::fs::write(
        repo_root.join("pyproject.toml"),
        "[project]\nname = \"py-app\"\n",
    )
    .expect("write pyproject");
    std::fs::write(repo_root.join("app.py"), "print('app')\n").expect("write app");

    let commit_oid = init_repo(&repo_root);
    std::fs::create_dir_all(&ozone_dir).expect("mkdir ozone");

    let venv_bin = repo_root.join(".venv/bin");
    std::fs::create_dir_all(&venv_bin).expect("mkdir venv/bin");
    let py_home = sys_python.parent().unwrap_or_else(|| Path::new("/usr/bin"));
    std::fs::write(
        repo_root.join(".venv/pyvenv.cfg"),
        format!(
            "home = {}\ninclude-system-site-packages = false\n",
            py_home.display()
        ),
    )
    .expect("write pyvenv.cfg");
    symlink(&sys_python, venv_bin.join("python3")).expect("symlink python3");
    symlink("python3", venv_bin.join("python")).expect("symlink python");

    let script_py3 = venv_bin.join("my-tool");
    std::fs::write(
        &script_py3,
        "#!/nonexistent/old/repo/.venv/bin/python3\nimport os, sys\nprint('PY3_OK:' + os.path.realpath(sys.prefix))\n",
    )
    .expect("write my-tool");
    std::fs::set_permissions(&script_py3, std::fs::Permissions::from_mode(0o755))
        .expect("chmod my-tool");

    let script_py = venv_bin.join("my-tool-py");
    std::fs::write(
        &script_py,
        "#!/nonexistent/old/repo/.venv/bin/python\nimport os, sys\nprint('PY_OK:' + os.path.realpath(sys.prefix))\n",
    )
    .expect("write my-tool-py");
    std::fs::set_permissions(&script_py, std::fs::Permissions::from_mode(0o755))
        .expect("chmod my-tool-py");

    let cfg = OzoneConfig::default();
    let meta = capture_baseline(&repo_root, &ozone_dir, &commit_oid, &cfg).expect("capture");
    assert!(meta.targets.iter().any(|t| t.relative_path == ".venv"));

    let baseline_venv = ozone_dir
        .join("baselines")
        .join(&commit_oid)
        .join("root/.venv");
    let cloned_ws = dir.path().join("elsewhere/cloned-ws");
    std::fs::create_dir_all(cloned_ws.join(".venv/bin")).expect("mkdir cloned venv");
    std::fs::copy(
        baseline_venv.join("pyvenv.cfg"),
        cloned_ws.join(".venv/pyvenv.cfg"),
    )
    .expect("copy pyvenv.cfg");
    symlink(&sys_python, cloned_ws.join(".venv/bin/python3")).expect("symlink cloned python3");
    symlink("python3", cloned_ws.join(".venv/bin/python")).expect("symlink cloned python");

    for bin_name in ["my-tool", "my-tool-py"] {
        let src = baseline_venv.join("bin").join(bin_name);
        let dst = cloned_ws.join(".venv/bin").join(bin_name);
        std::fs::copy(&src, &dst).expect("copy script");
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))
            .expect("chmod cloned");
        let content = std::fs::read_to_string(&dst).expect("read trampoline");
        assert!(content.starts_with("#!/bin/sh\n'''exec' \"$(dirname \"$0\")/python"));
    }

    let out1 = run_script_retry_etxtbsy(&cloned_ws.join(".venv/bin/my-tool"));
    assert!(
        out1.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let stdout1 = String::from_utf8_lossy(&out1.stdout);
    let expected_prefix = std::fs::canonicalize(cloned_ws.join(".venv"))
        .expect("canon venv")
        .display()
        .to_string();
    assert!(
        stdout1.contains(&format!("PY3_OK:{expected_prefix}")),
        "unexpected stdout: {stdout1}"
    );

    let out2 = run_script_retry_etxtbsy(&cloned_ws.join(".venv/bin/my-tool-py"));
    assert!(
        out2.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout2.contains(&format!("PY_OK:{expected_prefix}")),
        "unexpected stdout: {stdout2}"
    );
}

#[test]
fn quiescent_cargo_lock_falls_back_to_prior_baseline() {
    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("rust-app");
    let ozone_dir = repo_root.join(".git/ozone");
    std::fs::create_dir_all(&repo_root).expect("mkdir");
    std::fs::write(
        repo_root.join("Cargo.toml"),
        "[package]\nname = \"rust-app\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::create_dir_all(repo_root.join("src")).expect("mkdir src");
    std::fs::write(repo_root.join("src/lib.rs"), "pub fn v() -> u32 { 1 }\n").expect("write lib");

    let c1 = init_repo(&repo_root);
    std::fs::create_dir_all(&ozone_dir).expect("mkdir ozone");
    std::fs::create_dir_all(repo_root.join("target/debug/deps")).expect("mkdir target");
    std::fs::write(repo_root.join("target/debug/deps/liba.rlib"), b"rlib-v1").expect("write rlib");
    let lock_path = repo_root.join("target/.cargo-lock");
    std::fs::write(&lock_path, b"").expect("write .cargo-lock");

    let cfg = OzoneConfig::default();
    let targets = detect_warm_targets(&repo_root, &cfg);
    assert!(is_repo_quiescent(&repo_root, &targets).expect("quiescent"));

    let res1 = resolve_or_capture_baseline(&repo_root, &ozone_dir, &c1, &cfg)
        .expect("resolve c1")
        .expect("some c1");
    assert_eq!(res1.source, BaselineSource::Captured);

    // Advance commit to C2 and simulate active cargo build holding target/.cargo-lock.
    std::fs::write(repo_root.join("src/lib.rs"), "pub fn v() -> u32 { 2 }\n").expect("write lib2");
    let c2 = commit_all(&repo_root, "second");

    let lock_holder = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open lock holder");
    FileExt::lock(&lock_holder).expect("acquire exclusive lock");

    assert!(!is_repo_quiescent(&repo_root, &targets).expect("check quiescent while locked"));
    assert!(capture_baseline(&repo_root, &ozone_dir, &c2, &cfg).is_err());

    let fallback = resolve_or_capture_baseline(&repo_root, &ozone_dir, &c2, &cfg)
        .expect("resolve fallback")
        .expect("should fall back to c1");
    assert_eq!(fallback.source, BaselineSource::FallbackPrior);
    assert_eq!(fallback.baseline.metadata.commit_oid, c1);

    FileExt::unlock(&lock_holder).expect("unlock");
    drop(lock_holder);

    assert!(is_repo_quiescent(&repo_root, &targets).expect("quiescent after unlock"));
    let res2 = resolve_or_capture_baseline(&repo_root, &ozone_dir, &c2, &cfg)
        .expect("capture c2")
        .expect("some c2");
    assert_eq!(res2.source, BaselineSource::Captured);
    assert_eq!(res2.baseline.metadata.commit_oid, c2);
}

#[cfg(unix)]
#[test]
fn baseline_capture_excludes_locks_and_never_hardlinks_editable_sources() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("crate-a");
    let ozone_dir = repo_root.join(".git/ozone");
    std::fs::create_dir_all(repo_root.join("src")).expect("mkdir src");
    std::fs::write(
        repo_root.join("Cargo.toml"),
        "[package]\nname = \"crate-a\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(repo_root.join("src/main.rs"), "fn main() {}\n").expect("write main");

    let oid = init_repo(&repo_root);
    std::fs::create_dir_all(&ozone_dir).expect("mkdir ozone");

    let dep_path = repo_root.join("target/debug/deps/libdep.rlib");
    let inc_dir = repo_root.join("target/debug/incremental/crate_a/s-123");
    std::fs::create_dir_all(dep_path.parent().expect("parent")).expect("mkdir deps");
    std::fs::create_dir_all(&inc_dir).expect("mkdir inc");
    std::fs::write(&dep_path, b"immutable-rlib").expect("write dep");
    std::fs::set_permissions(&dep_path, std::fs::Permissions::from_mode(0o444)).expect("chmod dep");
    std::fs::write(repo_root.join("target/.cargo-lock"), b"").expect("write cargo-lock");
    std::fs::write(inc_dir.join("sess.lock"), b"").expect("write inc lock");

    let cfg = OzoneConfig::default();
    capture_baseline(&repo_root, &ozone_dir, &oid, &cfg).expect("capture baseline");

    let baseline_root = ozone_dir.join("baselines").join(&oid).join("root");
    assert!(!baseline_root.join("target/.cargo-lock").exists());
    assert!(
        !baseline_root
            .join("target/debug/incremental/crate_a/s-123/sess.lock")
            .exists()
    );
    assert!(
        baseline_root
            .join("target/debug/deps/libdep.rlib")
            .is_file()
    );

    // Editable source file MUST NOT be hardlinked!
    let src_ino = std::fs::metadata(repo_root.join("src/main.rs"))
        .expect("src meta")
        .ino();
    let base_src_meta =
        std::fs::metadata(baseline_root.join("src/main.rs")).expect("base src meta");
    assert_ne!(src_ino, base_src_meta.ino());
    assert_eq!(base_src_meta.nlink(), 1);
}

#[test]
fn mtime_preservation_restores_untouched_and_keeps_modified_at_now() {
    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("mtime-repo");
    let ozone_dir = repo_root.join(".git/ozone");
    std::fs::create_dir_all(repo_root.join("src")).expect("mkdir src");
    std::fs::write(
        repo_root.join("Cargo.toml"),
        "[package]\nname = \"mtime-repo\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(repo_root.join("src/unchanged.rs"), "pub fn a() {}\n").expect("write unchanged");
    std::fs::write(
        repo_root.join("src/modified.rs"),
        "pub fn b() -> u32 { 1 }\n",
    )
    .expect("write modified");

    let c1 = init_repo(&repo_root);
    std::fs::create_dir_all(&ozone_dir).expect("mkdir ozone");

    let old_secs = 1_700_000_000i64;
    let artifact_secs = 1_700_000_500i64;
    set_mtime_secs(&repo_root.join("Cargo.toml"), old_secs);
    set_mtime_secs(&repo_root.join("src/unchanged.rs"), old_secs);
    set_mtime_secs(&repo_root.join("src/modified.rs"), old_secs);

    std::fs::create_dir_all(repo_root.join("target/debug/deps")).expect("mkdir deps");
    let artifact_path = repo_root.join("target/debug/deps/libdep.rlib");
    std::fs::write(&artifact_path, b"compiled").expect("write artifact");
    set_mtime_secs(&artifact_path, artifact_secs);

    let cfg = OzoneConfig::default();
    let baseline_meta = capture_baseline(&repo_root, &ozone_dir, &c1, &cfg).expect("capture c1");

    // Advance branch to c2 modifying src/modified.rs and adding src/added.rs.
    std::fs::write(
        repo_root.join("src/modified.rs"),
        "pub fn b() -> u32 { 2 }\n",
    )
    .expect("modify");
    std::fs::write(repo_root.join("src/added.rs"), "pub fn c() {}\n").expect("add");
    let c2 = commit_all(&repo_root, "modify and add");

    let changed = compute_changed_paths_between_commits(&repo_root, &c1, &c2).expect("diff c1..c2");
    assert!(changed.contains("src/modified.rs"));
    assert!(changed.contains("src/added.rs"));
    assert!(!changed.contains("src/unchanged.rs"));

    // Simulate freshly checked out workspace where all files initially have mtime = NOW.
    let ws_root = dir.path().join("ws-1");
    std::fs::create_dir_all(ws_root.join("src")).expect("mkdir ws src");
    std::fs::write(
        ws_root.join("Cargo.toml"),
        "[package]\nname = \"mtime-repo\"\n",
    )
    .expect("w");
    std::fs::write(ws_root.join("src/unchanged.rs"), "pub fn a() {}\n").expect("w");
    std::fs::write(ws_root.join("src/modified.rs"), "pub fn b() -> u32 { 2 }\n").expect("w");
    std::fs::write(ws_root.join("src/added.rs"), "pub fn c() {}\n").expect("w");

    let now_before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_secs() as i64;
    let stats = apply_baseline_mtimes(&ws_root, &baseline_meta, &changed).expect("apply mtimes");
    assert_eq!(stats.kept_modified_files, 1);
    assert!(stats.restored_files >= 2);

    assert_eq!(get_mtime_secs(&ws_root.join("src/unchanged.rs")), old_secs);
    assert_eq!(get_mtime_secs(&ws_root.join("Cargo.toml")), old_secs);
    assert!(get_mtime_secs(&ws_root.join("src/unchanged.rs")) <= artifact_secs);
    assert!(get_mtime_secs(&ws_root.join("src/modified.rs")) >= now_before - 2);
    assert!(get_mtime_secs(&ws_root.join("src/added.rs")) >= now_before - 2);
}

#[test]
fn lru_gc_evicts_least_recently_used_baselines_within_budget() {
    let dir = TempDir::new().expect("tempdir");
    let ozone_dir = dir.path().join(".git/ozone");
    let baselines_dir = ozone_dir.join("baselines");
    std::fs::create_dir_all(&baselines_dir).expect("mkdir baselines");

    let now = Utc::now();
    for (idx, id) in ["c1", "c2", "c3"].iter().enumerate() {
        let bdir = baselines_dir.join(id);
        let root = bdir.join("root");
        std::fs::create_dir_all(&root).expect("mkdir root");
        std::fs::write(root.join("blob.bin"), vec![b'x'; 4096]).expect("write blob");

        let meta = BaselineMetadata {
            commit_oid: (*id).to_string(),
            created_at: now - ChronoDuration::hours((3 - idx) as i64),
            targets: Vec::new(),
            total_bytes: 4096,
            file_mtimes_secs: Default::default(),
            file_mtimes_nanos: Default::default(),
        };
        std::fs::write(
            bdir.join("metadata.json"),
            serde_json::to_string_pretty(&meta).expect("json"),
        )
        .expect("write meta");
    }

    // Touch c1 so it becomes newer than c2 (making c2 the least-recently-used).
    touch_baseline_lru(&ozone_dir, "c1").expect("touch c1");

    // 9 KiB budget out of 12 KiB total -> should evict only c2 (4 KiB), leaving 8 KiB (c1 & c3).
    let max_gb = 9000.0 / (1024.0 * 1024.0 * 1024.0);
    let report = gc_baselines(&ozone_dir, max_gb).expect("gc");
    assert_eq!(report.removed_baselines, vec!["c2"]);
    assert_eq!(report.remaining_count, 2);
    assert_eq!(report.remaining_bytes, 8192);
    assert!(find_baseline(&ozone_dir, "c1").expect("c1").is_some());
    assert!(find_baseline(&ozone_dir, "c2").expect("c2").is_none());
    assert!(find_baseline(&ozone_dir, "c3").expect("c3").is_some());
}

#[allow(deprecated)]
fn set_mtime_secs(path: &Path, secs: i64) {
    filetime::set_file_mtime(path, FileTime::from_unix_time(secs, 0)).expect("set mtime");
}

fn get_mtime_secs(path: &Path) -> i64 {
    std::fs::metadata(path)
        .expect("meta")
        .modified()
        .expect("modified")
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64
}

#[cfg(unix)]
#[test]
fn concurrent_baseline_capture_serializes_cleanly_and_hardlinks_non_incremental_cargo_artifacts() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::{Arc, Barrier};

    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("concurrent-rust");
    let ozone_dir = repo_root.join(".git/ozone");
    std::fs::create_dir_all(repo_root.join("src")).expect("mkdir src");
    std::fs::write(
        repo_root.join("Cargo.toml"),
        "[package]\nname = \"concurrent-rust\"\nversion = \"0.1.0\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(repo_root.join("src/lib.rs"), "pub fn v() -> u32 { 42 }\n").expect("write lib");

    let oid = init_repo(&repo_root);
    std::fs::create_dir_all(&ozone_dir).expect("mkdir ozone");

    let dep_path = repo_root.join("target/debug/deps/libdep.rlib");
    let bin_path = repo_root.join("target/debug/concurrent-rust");
    let inc_path = repo_root.join("target/debug/incremental/crate/s-1/dep-graph.bin");
    std::fs::create_dir_all(dep_path.parent().expect("parent")).expect("mkdir deps");
    std::fs::create_dir_all(inc_path.parent().expect("parent")).expect("mkdir inc");
    std::fs::write(&dep_path, vec![0x11u8; 64 * 1024]).expect("write dep");
    std::fs::write(&bin_path, vec![0x22u8; 64 * 1024]).expect("write bin");
    std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755)).expect("chmod bin");
    std::fs::write(&inc_path, vec![0x33u8; 16 * 1024]).expect("write inc");

    let threads = 6usize;
    let barrier = Arc::new(Barrier::new(threads));
    let mut results = Vec::new();
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for _ in 0..threads {
            let b = Arc::clone(&barrier);
            let rr = &repo_root;
            let od = &ozone_dir;
            let id = &oid;
            handles.push(s.spawn(move || {
                b.wait();
                capture_baseline(rr, od, id, &OzoneConfig::default())
            }));
        }
        for h in handles {
            results.push(h.join().expect("thread join").expect("capture ok"));
        }
    });

    assert_eq!(results.len(), threads);
    for meta in &results {
        assert_eq!(meta.commit_oid, oid);
    }

    // No leftover `.staging-*` directories after concurrent capture.
    let leftovers: Vec<_> = std::fs::read_dir(ozone_dir.join("baselines"))
        .expect("read baselines")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".staging-"))
        .collect();
    assert!(leftovers.is_empty(), "leftover staging dirs: {leftovers:?}");

    let base_root = ozone_dir.join("baselines").join(&oid).join("root");
    // Editable source MUST NOT be hardlinked!
    assert_ne!(
        std::fs::metadata(repo_root.join("src/lib.rs"))
            .expect("src")
            .ino(),
        std::fs::metadata(base_root.join("src/lib.rs"))
            .expect("base src")
            .ino()
    );
    // Incremental cache uses in-place mmap/SQLite writes -> MUST NOT be hardlinked!
    assert_ne!(
        std::fs::metadata(&inc_path).expect("inc").ino(),
        std::fs::metadata(base_root.join("target/debug/incremental/crate/s-1/dep-graph.bin"))
            .expect("base inc")
            .ino()
    );
    // Compiled deps and top-level binary in `target/debug/` ARE hardlinked on non-reflink ext4/tmpfs
    // (or reflinked on APFS/Btrfs/XFS, in which case content and mode still match).
    let base_bin = base_root.join("target/debug/concurrent-rust");
    assert_eq!(
        std::fs::metadata(&base_bin).expect("base bin").len(),
        64 * 1024
    );
    assert!(base_root.join("target/debug/deps/libdep.rlib").is_file());
}

#[test]
fn excludes_and_detects_cargo_build_and_artifact_locks_across_profiles() {
    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("cargo-locks");
    let ozone_dir = repo_root.join(".git/ozone");
    std::fs::create_dir_all(repo_root.join("src")).expect("mkdir src");
    std::fs::write(
        repo_root.join("Cargo.toml"),
        "[package]\nname = \"cargo-locks\"\nversion = \"0.1.0\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(repo_root.join("src/lib.rs"), "pub fn ok() {}\n").expect("write lib");

    let oid = init_repo(&repo_root);
    std::fs::create_dir_all(&ozone_dir).expect("mkdir ozone");

    let triple_debug = repo_root.join("target/x86_64-unknown-linux-gnu/debug");
    std::fs::create_dir_all(triple_debug.join("deps")).expect("mkdir triple deps");
    std::fs::write(triple_debug.join("deps/libx.rlib"), b"rlib").expect("write rlib");
    let build_lock = triple_debug.join(".cargo-build-lock");
    let artifact_lock = repo_root.join("target/debug/.cargo-artifact-lock");
    std::fs::create_dir_all(artifact_lock.parent().expect("parent")).expect("mkdir debug");
    std::fs::write(&build_lock, b"").expect("write build lock");
    std::fs::write(&artifact_lock, b"").expect("write artifact lock");

    let cfg = OzoneConfig::default();
    let targets = detect_warm_targets(&repo_root, &cfg);
    assert!(is_repo_quiescent(&repo_root, &targets).expect("quiescent before lock"));

    // Holding `.cargo-build-lock` in target triple dir blocks quiescence.
    let holder = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&build_lock)
        .expect("open build_lock");
    FileExt::lock(&holder).expect("lock build_lock");
    assert!(!is_repo_quiescent(&repo_root, &targets).expect("non-quiescent on build_lock"));
    FileExt::unlock(&holder).expect("unlock build_lock");
    drop(holder);

    // Holding `.cargo-artifact-lock` also blocks quiescence.
    let holder2 = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&artifact_lock)
        .expect("open artifact_lock");
    FileExt::lock(&holder2).expect("lock artifact_lock");
    assert!(!is_repo_quiescent(&repo_root, &targets).expect("non-quiescent on artifact_lock"));
    FileExt::unlock(&holder2).expect("unlock artifact_lock");
    drop(holder2);

    capture_baseline(&repo_root, &ozone_dir, &oid, &cfg).expect("capture baseline");
    let base_root = ozone_dir.join("baselines").join(&oid).join("root");
    assert!(
        !base_root
            .join("target/x86_64-unknown-linux-gnu/debug/.cargo-build-lock")
            .exists()
    );
    assert!(!base_root.join("target/debug/.cargo-artifact-lock").exists());
    assert!(
        base_root
            .join("target/x86_64-unknown-linux-gnu/debug/deps/libx.rlib")
            .is_file()
    );
}

#[test]
fn detects_custom_cargo_target_dir_and_subsecond_mtime_preservation() {
    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("custom-target");
    let ozone_dir = repo_root.join(".git/ozone");
    std::fs::create_dir_all(repo_root.join(".cargo")).expect("mkdir .cargo");
    std::fs::create_dir_all(repo_root.join("src")).expect("mkdir src");
    std::fs::write(
        repo_root.join("Cargo.toml"),
        "[package]\nname = \"custom-target\"\nversion = \"0.1.0\"\n",
    )
    .expect("Cargo.toml");
    std::fs::write(
        repo_root.join(".cargo/config.toml"),
        "[build]\ntarget-dir = \"build/cargo-target\"\n",
    )
    .expect("config.toml");
    let src_file = repo_root.join("src/lib.rs");
    std::fs::write(&src_file, "pub fn x() -> u32 { 1 }\n").expect("write lib");

    let c1 = init_repo(&repo_root);
    std::fs::create_dir_all(&ozone_dir).expect("mkdir ozone");

    let precise_ft = FileTime::from_unix_time(1_700_000_123, 456_789_000);
    #[allow(deprecated)]
    filetime::set_file_mtime(&src_file, precise_ft).expect("set subsecond mtime");

    let cfg = OzoneConfig::default();
    let targets = detect_warm_targets_with_cargo_target_dir(&repo_root, &cfg, None);
    assert!(
        targets
            .iter()
            .any(|t| t.relative_path == "build/cargo-target")
    );

    let env_targets = detect_warm_targets_with_cargo_target_dir(
        &repo_root,
        &cfg,
        Some(std::ffi::OsStr::new("env-target")),
    );
    assert!(env_targets.iter().any(|t| t.relative_path == "env-target"));

    let meta = capture_baseline(&repo_root, &ozone_dir, &c1, &cfg).expect("capture");
    assert_eq!(
        meta.file_mtimes_secs.get("src/lib.rs").copied(),
        Some(1_700_000_123)
    );
    assert_eq!(
        meta.file_mtimes_nanos.get("src/lib.rs").copied(),
        Some(456_789_000)
    );

    let ws_root = dir.path().join("ws-subsec");
    std::fs::create_dir_all(ws_root.join("src")).expect("mkdir ws src");
    std::fs::write(ws_root.join("src/lib.rs"), "pub fn x() -> u32 { 1 }\n").expect("write ws");
    apply_baseline_mtimes(&ws_root, &meta, &Default::default()).expect("apply mtime");

    let restored_meta = std::fs::metadata(ws_root.join("src/lib.rs")).expect("stat ws lib");
    let restored_mod = restored_meta.modified().expect("modified");
    let dur = restored_mod.duration_since(UNIX_EPOCH).expect("epoch");
    assert_eq!(dur.as_secs(), 1_700_000_123);
    assert_eq!(dur.subsec_nanos(), 456_789_000);
}

#[test]
fn cargo_fingerprint_portability_check_detects_absolute_paths_and_remap_flags() {
    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("fp-check");
    let fp_dir = repo_root.join("target/debug/.fingerprint/mycrate-abc");
    std::fs::create_dir_all(&fp_dir).expect("mkdir fp");

    std::fs::write(
        fp_dir.join("lib-mycrate.json"),
        serde_json::json!({
            "rustflags": ["--remap-path-prefix", "src=/src"],
            "local": [{"CheckDepInfo": {"dep_info": "debug/.fingerprint/mycrate-abc/dep-lib-mycrate", "checksum": false}}]
        })
        .to_string(),
    )
    .expect("write clean fp");
    let ok_report =
        check_cargo_fingerprint_portability(&repo_root, Path::new("target")).expect("scan");
    assert!(ok_report.portable);
    assert_eq!(ok_report.fingerprints_scanned, 1);

    std::fs::write(
        fp_dir.join("run-build-script.json"),
        serde_json::json!({
            "rustflags": [format!("--remap-path-prefix={}=/ws", repo_root.display())],
            "local": [{"RerunIfChanged": {"output": "out", "paths": [repo_root.join("src/lib.rs")]}}]
        })
        .to_string(),
    )
    .expect("write non-portable fp");
    let bad_report =
        check_cargo_fingerprint_portability(&repo_root, Path::new("target")).expect("scan2");
    assert!(!bad_report.portable);
    assert_eq!(bad_report.non_portable_remap_rustflags.len(), 1);
    assert_eq!(bad_report.non_portable_rerun_paths.len(), 1);
}

#[cfg(unix)]
#[test]
fn python_venv_normalizes_pyvenv_cfg_activate_long_trampolines_and_editable_pth() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let Some(sys_python) = find_system_python3() else {
        return;
    };

    let dir = TempDir::new().expect("tempdir");
    let repo_root = dir.path().join("py-full");
    let ozone_dir = repo_root.join(".git/ozone");
    std::fs::create_dir_all(repo_root.join("pkg")).expect("mkdir pkg");
    std::fs::write(
        repo_root.join("pyproject.toml"),
        "[project]\nname = \"py-full\"\n",
    )
    .expect("pyproject");
    std::fs::write(repo_root.join("pkg/__init__.py"), "MARKER = 'from_repo'\n").expect("init");
    let oid = init_repo(&repo_root);
    std::fs::create_dir_all(&ozone_dir).expect("ozone");

    let venv_root = repo_root.join(".venv");
    let venv_bin = venv_root.join("bin");
    let py_ver = python_major_minor_dir(&sys_python);
    let sp_dir = venv_root.join("lib").join(py_ver).join("site-packages");
    std::fs::create_dir_all(&venv_bin).expect("mkdir bin");
    std::fs::create_dir_all(&sp_dir).expect("mkdir sp");

    let py_home = sys_python.parent().unwrap_or_else(|| Path::new("/usr/bin"));
    std::fs::write(
        venv_root.join("pyvenv.cfg"),
        format!(
            "home = {}\ninclude-system-site-packages = false\ncommand = /usr/bin/python3 -m venv {}\n",
            py_home.display(),
            venv_root.display()
        ),
    )
    .expect("pyvenv.cfg");
    symlink(&sys_python, venv_bin.join("python3")).expect("symlink python3");
    symlink("python3", venv_bin.join("python")).expect("symlink python");
    symlink(venv_bin.join("python3"), venv_bin.join("py_alias")).expect("symlink abs internal");

    std::fs::write(
        venv_bin.join("activate"),
        format!(
            "deactivate nondestructive\nexport VIRTUAL_ENV={}\nPATH=\"$VIRTUAL_ENV/bin:$PATH\"\n",
            venv_root.display()
        ),
    )
    .expect("activate");

    let long_tool = venv_bin.join("long-tool");
    std::fs::write(
        &long_tool,
        format!(
            "#!/bin/sh\n'''exec' {}/python3 \"$0\" \"$@\"\n' '''\nimport os, sys, pkg\nprint('LONG:' + os.path.realpath(sys.prefix) + ':' + pkg.MARKER)\n",
            venv_bin.display()
        ),
    )
    .expect("long-tool");
    std::fs::set_permissions(&long_tool, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    std::fs::write(
        sp_dir.join("editable.pth"),
        format!("{}\n", repo_root.display()),
    )
    .expect("pth");

    let cfg = OzoneConfig::default();
    capture_baseline(&repo_root, &ozone_dir, &oid, &cfg).expect("capture");
    let base_root = ozone_dir.join("baselines").join(&oid).join("root");

    // Copy baseline root into a relocated workspace and override pkg/__init__.py to prove isolation!
    let ws_root = dir.path().join("relocated/ws-py");
    std::fs::create_dir_all(ws_root.join("pkg")).expect("mkdir ws pkg");
    std::fs::write(ws_root.join("pkg/__init__.py"), "MARKER = 'from_ws'\n").expect("ws init");
    for entry in walkdir::WalkDir::new(base_root.join(".venv")).follow_links(false) {
        let entry = entry.expect("walk");
        let rel = entry.path().strip_prefix(&base_root).expect("strip");
        let dst = ws_root.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dst).expect("mkdir");
        } else if entry.path_is_symlink() {
            let tgt = std::fs::read_link(entry.path()).expect("readlink");
            symlink(tgt, &dst).expect("symlink");
        } else {
            std::fs::copy(entry.path(), &dst).expect("copy");
            std::fs::set_permissions(&dst, entry.metadata().expect("meta").permissions())
                .expect("perms");
        }
    }

    let cfg_text = std::fs::read_to_string(ws_root.join(".venv/pyvenv.cfg")).expect("read cfg");
    assert!(!cfg_text.contains(repo_root.to_string_lossy().as_ref()));
    assert!(cfg_text.contains("command = /usr/bin/python3 -m venv .venv"));

    let alias_target =
        std::fs::read_link(ws_root.join(".venv/bin/py_alias")).expect("readlink alias");
    assert!(
        !alias_target.is_absolute(),
        "expected relative symlink, got {alias_target:?}"
    );

    // Source `.venv/bin/activate` in bash and check VIRTUAL_ENV resolves to `ws_root/.venv`.
    let act_out = Command::new("bash")
        .args([
            "-c",
            "source .venv/bin/activate && printf '%s' \"$VIRTUAL_ENV\"",
        ])
        .current_dir(&ws_root)
        .output()
        .expect("bash activate");
    assert!(
        act_out.status.success(),
        "{}",
        String::from_utf8_lossy(&act_out.stderr)
    );
    let expected_venv = std::fs::canonicalize(ws_root.join(".venv")).expect("canon");
    assert_eq!(
        String::from_utf8_lossy(&act_out.stdout),
        expected_venv.to_string_lossy().as_ref()
    );

    // Execute rewritten long-path trampoline script and verify sys.prefix and editable pkg come from ws_root!
    let tool_out = run_script_retry_etxtbsy(&ws_root.join(".venv/bin/long-tool"));
    assert!(
        tool_out.status.success(),
        "{}",
        String::from_utf8_lossy(&tool_out.stderr)
    );
    let stdout = String::from_utf8_lossy(&tool_out.stdout);
    assert!(
        stdout.contains(&format!("LONG:{}:from_ws", expected_venv.display())),
        "unexpected long-tool output: {stdout}"
    );
}
