// Copyright (c) 2026 Ozone Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for `oz-warm`.

use chrono::{Duration as ChronoDuration, Utc};
use filetime::FileTime;
use fs4::FileExt;
use oz_core::OzoneConfig;
use oz_warm::{
    BaselineMetadata, BaselineSource, apply_baseline_mtimes, capture_baseline,
    compute_changed_paths_between_commits, detect_warm_targets, find_baseline, gc_baselines,
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

fn find_system_python3() -> Option<std::path::PathBuf> {
    for cand in ["/usr/bin/python3", "/bin/python3"] {
        let p = Path::new(cand);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    if let Some(path_env) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_env) {
            let p = dir.join("python3");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
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
    std::fs::write(
        repo_root.join(".venv/pyvenv.cfg"),
        "home = /usr/bin\ninclude-system-site-packages = false\n",
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

    let baseline_venv = ozone_dir.join("baselines").join(&commit_oid).join("root/.venv");
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

    let out1 = Command::new(cloned_ws.join(".venv/bin/my-tool"))
        .output()
        .expect("run my-tool");
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

    let out2 = Command::new(cloned_ws.join(".venv/bin/my-tool-py"))
        .output()
        .expect("run my-tool-py");
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
    assert!(baseline_root.join("target/debug/deps/libdep.rlib").is_file());

    // Editable source file MUST NOT be hardlinked!
    let src_ino = std::fs::metadata(repo_root.join("src/main.rs"))
        .expect("src meta")
        .ino();
    let base_src_meta = std::fs::metadata(baseline_root.join("src/main.rs")).expect("base src meta");
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
    std::fs::write(repo_root.join("src/modified.rs"), "pub fn b() -> u32 { 1 }\n")
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
    std::fs::write(repo_root.join("src/modified.rs"), "pub fn b() -> u32 { 2 }\n")
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
    std::fs::write(ws_root.join("Cargo.toml"), "[package]\nname = \"mtime-repo\"\n").expect("w");
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
