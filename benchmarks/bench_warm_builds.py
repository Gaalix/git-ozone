#!/usr/bin/env python3
"""Benchmark warm incremental build latency for `oz ws spawn` vs cold `git worktree`.

Measures first-build latency across real toolchains after `oz ws spawn` versus
cold `git worktree add` across three ecosystems:
  1. Rust (`cargo check` & `cargo test`, Cargo fingerprint path-invalidation
     probes across `../<repo>.oz/<ws>` vs `./<repo>`, plus optional `sccache`).
  2. TypeScript (`pnpm` / `npm` + `node_modules/.bin/tsc`).
  3. Python (`.venv` + `pytest`, verifying `pyvenv.cfg`, `activate`, shebangs,
     and `sys.prefix` workspace isolation).
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from bench_common import (
    Sandbox,
    build_base_env,
    choose_work_dir_base,
    collect_system_metadata,
    configure_oz_backend,
    detect_no_new_privs,
    discover_binaries,
    init_git_repo,
    log,
    run_cmd,
    summarize_ms,
    write_json,
)


def detect_toolchains() -> Dict[str, Any]:
    """Probe installed toolchain binaries and offline package caches."""
    cargo = shutil.which("cargo")
    rustc = shutil.which("rustc")
    sccache = shutil.which("sccache")
    node = shutil.which("node")
    npm = shutil.which("npm")
    pnpm = shutil.which("pnpm")
    python3 = shutil.which("python3") or sys.executable
    pytest_bin = shutil.which("pytest")
    fusermount3 = shutil.which("fusermount3")

    offline_crates = find_offline_cargo_crates()
    pnpm_store = find_pnpm_store_dir(pnpm) if pnpm else None

    return {
        "cargo": cargo,
        "rustc": rustc,
        "sccache": sccache,
        "node": node,
        "npm": npm,
        "pnpm": pnpm,
        "python3": python3,
        "pytest": pytest_bin,
        "fusermount3": fusermount3,
        "versions": {
            "cargo": cmd_version(cargo, "--version"),
            "rustc": cmd_version(rustc, "--version"),
            "sccache": cmd_version(sccache, "--version"),
            "node": cmd_version(node, "--version"),
            "npm": cmd_version(npm, "--version"),
            "pnpm": cmd_version(pnpm, "--version"),
            "python3": cmd_version(python3, "--version"),
            "pytest": cmd_version(pytest_bin, "--version"),
        },
        "offline_cargo_crates": offline_crates,
        "pnpm_store": str(pnpm_store) if pnpm_store else None,
    }


def cmd_version(exe: Optional[str], arg: str) -> Optional[str]:
    """Return the first line of `<exe> <arg>` if the binary exists."""
    if not exe:
        return None
    try:
        cp = subprocess.run(
            [exe, arg],
            text=True,
            capture_output=True,
            timeout=5,
            check=False,
        )
        out = (cp.stdout or cp.stderr or "").strip().splitlines()
        return out[0].strip() if out else None
    except Exception:
        return None


def find_offline_cargo_crates() -> Dict[str, str]:
    """Find cached crate versions present in both Cargo registry src and cache."""
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    registry = cargo_home / "registry"
    if not registry.exists():
        return {}

    wanted = ("itoa", "cfg-if")
    found: Dict[str, str] = {}
    for crate in wanted:
        src_versions: set[str] = set()
        for match in glob.glob(str(registry / "src" / "*" / f"{crate}-*")):
            p = Path(match)
            if p.is_dir():
                src_versions.add(p.name[len(crate) + 1 :])

        cache_versions: set[str] = set()
        for match in glob.glob(str(registry / "cache" / "*" / f"{crate}-*.crate")):
            name = Path(match).name
            if name.endswith(".crate"):
                cache_versions.add(name[len(crate) + 1 : -len(".crate")])

        both = sorted(src_versions & cache_versions, key=semver_key)
        if both:
            found[crate] = both[-1]
    return found


def semver_key(ver: str) -> Tuple[int, ...]:
    """Best-effort numeric sort key for simple semver strings."""
    parts: List[int] = []
    for piece in re.split(r"[^0-9]+", ver):
        if piece:
            parts.append(int(piece))
    return tuple(parts)


def find_pnpm_store_dir(pnpm_bin: Optional[str]) -> Optional[Path]:
    """Locate pnpm's content-addressable store if already populated on disk."""
    if not pnpm_bin:
        return None
    try:
        cp = subprocess.run(
            [pnpm_bin, "store", "path"],
            text=True,
            capture_output=True,
            timeout=5,
            check=False,
        )
        if cp.returncode == 0 and cp.stdout.strip():
            p = Path(cp.stdout.strip())
            if p.exists():
                return p
    except Exception:
        pass
    default_store = Path.home() / ".local" / "share" / "pnpm" / "store"
    if default_store.exists():
        return default_store
    return None


def format_oz_spawn_error(err: Exception, backend: Optional[str]) -> RuntimeError:
    """Wrap workspace spawn failures with actionable NoNewPrivs hints when relevant."""
    msg = str(err)
    nnp = detect_no_new_privs()
    if (
        "fusermount3" in msg
        or "Operation not permitted" in msg
        or "MountUnavailable" in msg
    ):
        hint = (
            "Live FUSE3 mount failed because NoNewPrivs=1 blocks setuid fusermount3 in this sandbox; "
            "re-run with `--oz-backend cas_hardlink` here or run on a host shell with NoNewPrivs=0."
            if nnp
            else "FUSE3 mount unavailable; verify `/dev/fuse` and `fusermount3` or pass `--oz-backend cas_hardlink`."
        )
        return RuntimeError(f"oz ws spawn failed (backend={backend}): {msg}\nHint: {hint}")
    return RuntimeError(f"oz ws spawn failed (backend={backend}): {msg}")


def oz_spawn_ws(
    oz_bin: str,
    repo_dir: Path,
    ws_name: str,
    env: Dict[str, str],
    backend: Optional[str],
    *,
    no_warm: bool = False,
) -> Tuple[Path, float]:
    """Run `oz ws spawn` inside `repo_dir` and return `(ws_path, elapsed_ms)`."""
    cmd = [oz_bin, "--agent", "ws", "spawn", ws_name, "--json"]
    if no_warm:
        cmd.append("--no-warm")
    try:
        _, out, _, ms = run_cmd(cmd, cwd=repo_dir, env=env, timeout=180.0)
    except Exception as exc:
        raise format_oz_spawn_error(exc, backend) from exc

    ws_path = repo_dir.parent / f"{repo_dir.name}.oz" / ws_name
    try:
        payload = json.loads(out)
        data = payload.get("data") if isinstance(payload, dict) else None
        if isinstance(data, dict) and data.get("path"):
            ws_path = Path(data["path"])
    except Exception:
        pass
    if not ws_path.exists():
        raise RuntimeError(f"spawned workspace path missing: {ws_path}")
    return ws_path, ms


def git_worktree_add(
    git_bin: str,
    repo_dir: Path,
    wt_dir: Path,
    branch: str,
    env: Dict[str, str],
) -> float:
    """Create a fresh git worktree from HEAD on `branch` and return elapsed ms."""
    _, _, _, ms = run_cmd(
        [git_bin, "worktree", "add", "-b", branch, str(wt_dir), "HEAD"],
        cwd=repo_dir,
        env=env,
        timeout=60.0,
    )
    return ms


def git_worktree_remove(
    git_bin: str,
    repo_dir: Path,
    wt_dir: Path,
    branch: str,
    env: Dict[str, str],
) -> None:
    """Remove a git worktree and its temporary branch."""
    run_cmd(
        [git_bin, "worktree", "remove", "--force", str(wt_dir)],
        cwd=repo_dir,
        env=env,
        check=False,
        timeout=60.0,
    )
    run_cmd(
        [git_bin, "branch", "-D", branch],
        cwd=repo_dir,
        env=env,
        check=False,
        timeout=30.0,
    )


def oz_setup_and_register(
    sb: Sandbox,
    oz_bin: str,
    repo_dir: Path,
    env: Dict[str, str],
    backend: Optional[str],
) -> None:
    """Initialize Ozone metadata in `repo_dir`, configure backend, and register cleanup."""
    configure_oz_backend(repo_dir, backend)
    sb.register_oz_repo(repo_dir)
    run_cmd([oz_bin, "--agent", "setup", "--json"], cwd=repo_dir, env=env, timeout=60.0)


def parse_cargo_fingerprint_logs(stderr_text: str) -> Dict[str, Any]:
    """Parse `cargo check -vv` and `CARGO_LOG` stderr lines."""
    fresh = 0
    dirty: List[str] = []
    compiling: List[str] = []
    ro_fp_error = (".fingerprint/" in stderr_text) and (
        "Permission denied" in stderr_text or "os error 13" in stderr_text
    )

    for raw in stderr_text.splitlines():
        line = raw.strip()
        if line.startswith("Fresh "):
            fresh += 1
        elif line.startswith("Dirty "):
            dirty.append(line)
        elif line.startswith("Compiling "):
            compiling.append(line)
        elif "fingerprint error for " in line or "dirty: " in line:
            dirty.append(line)

    return {
        "fresh_units_logged": fresh,
        "dirty_events": len(dirty),
        "dirty_reasons_sample": dirty[:10],
        "compiling_units": len(compiling),
        "compiling_sample": compiling[:10],
        "ro_fingerprint_write_attempted": ro_fp_error,
    }


def count_cargo_tests_passed(stdout_text: str) -> int:
    """Sum `N passed;` counts across cargo test output."""
    total = 0
    for match in re.finditer(r"(\d+)\s+passed;", stdout_text):
        total += int(match.group(1))
    return total


def write_rust_fixture(
    repo_dir: Path,
    *,
    num_modules: int,
    offline_crates: Dict[str, str],
    abs_rerun_in_build_rs: bool = False,
) -> Dict[str, Any]:
    """Generate a realistic multi-crate Cargo workspace with optional offline crate dep."""
    crates_dir = repo_dir / "crates"
    core_src = crates_dir / "corelib" / "src"
    app_src = crates_dir / "app" / "src"
    core_src.mkdir(parents=True, exist_ok=True)
    app_src.mkdir(parents=True, exist_ok=True)

    use_itoa = "itoa" in offline_crates
    use_cfg_if = "cfg-if" in offline_crates

    (repo_dir / ".gitignore").write_text("target/\n", encoding="utf-8")
    (repo_dir / "Cargo.toml").write_text(
        '[workspace]\nresolver = "2"\nmembers = ["crates/corelib", "crates/app"]\n',
        encoding="utf-8",
    )

    core_deps = []
    if use_itoa:
        core_deps.append(f'itoa = "={offline_crates["itoa"]}"')
    if use_cfg_if:
        core_deps.append(f'cfg-if = "={offline_crates["cfg-if"]}"')

    (crates_dir / "corelib" / "Cargo.toml").write_text(
        "[package]\n"
        'name = "corelib"\n'
        'version = "0.1.0"\n'
        'edition = "2021"\n\n'
        "[dependencies]\n"
        + ("\n".join(core_deps) + "\n" if core_deps else ""),
        encoding="utf-8",
    )

    mod_decls: List[str] = []
    sum_terms: List[str] = []
    for idx in range(num_modules):
        mod_name = f"mod_{idx:02d}"
        mod_decls.append(f"pub mod {mod_name};")
        sum_terms.append(f"{mod_name}::compute(seed + {idx + 1})")
        fmt_expr = (
            "let mut buf = itoa::Buffer::new();\n"
            "    buf.format(v).len() as u64 + v"
            if use_itoa
            else "v.wrapping_mul(6364136223846793005).wrapping_add(1)"
        )
        (core_src / f"{mod_name}.rs").write_text(
            f"pub fn compute(seed: u64) -> u64 {{\n"
            f"    let v = seed.wrapping_add({idx * 17 + 3});\n"
            f"    {fmt_expr}\n"
            f"}}\n",
            encoding="utf-8",
        )

    cfg_if_block = (
        "cfg_if::cfg_if! {\n"
        "    if #[cfg(unix)] {\n"
        "        pub const PLATFORM_TAG: &str = \"unix\";\n"
        "    } else {\n"
        "        pub const PLATFORM_TAG: &str = \"other\";\n"
        "    }\n"
        "}\n\n"
        if use_cfg_if
        else 'pub const PLATFORM_TAG: &str = "generic";\n\n'
    )
    (core_src / "lib.rs").write_text(
        "\n".join(mod_decls)
        + "\n\n"
        + cfg_if_block
        + "pub fn aggregate(seed: u64) -> u64 {\n"
        + "    0u64\n"
        + "".join(f"        .wrapping_add({term})\n" for term in sum_terms)
        + "}\n\n"
        + "#[cfg(test)]\n"
        + "mod tests {\n"
        + "    use super::*;\n\n"
        + "    #[test]\n"
        + "    fn aggregate_is_deterministic() {\n"
        + "        assert_eq!(aggregate(42), aggregate(42));\n"
        + "        assert!(!PLATFORM_TAG.is_empty());\n"
        + "    }\n"
        + "}\n",
        encoding="utf-8",
    )

    (crates_dir / "app" / "Cargo.toml").write_text(
        "[package]\n"
        'name = "app"\n'
        'version = "0.1.0"\n'
        'edition = "2021"\n\n'
        "[dependencies]\n"
        'corelib = { path = "../corelib" }\n',
        encoding="utf-8",
    )

    if abs_rerun_in_build_rs:
        rerun_target = str((crates_dir / "app" / "src" / "lib.rs").resolve()).replace("\\", "\\\\")
        rerun_line = f'println!("cargo::rerun-if-changed={rerun_target}");'
    else:
        rerun_line = 'println!("cargo::rerun-if-changed=build.rs");'

    (crates_dir / "app" / "build.rs").write_text(
        "use std::{env, fs, path::Path};\n\n"
        "fn main() {\n"
        f"    {rerun_line}\n"
        '    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");\n'
        '    let out_file = Path::new(&out_dir).join("generated.rs");\n'
        '    fs::write(out_file, "pub const BUILD_MAGIC: u64 = 0x5A17_0001;\\n").expect("write");\n'
        "}\n",
        encoding="utf-8",
    )

    (app_src / "lib.rs").write_text(
        'include!(concat!(env!("OUT_DIR"), "/generated.rs"));\n\n'
        "pub fn run_workload(seed: u64) -> u64 {\n"
        "    corelib::aggregate(seed ^ BUILD_MAGIC)\n"
        "}\n\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    use super::*;\n\n"
        "    #[test]\n"
        "    fn workload_works() {\n"
        "        assert_ne!(run_workload(7), 0);\n"
        "    }\n"
        "}\n",
        encoding="utf-8",
    )

    return {
        "modules": num_modules,
        "uses_offline_registry_crates": [
            c for c in ("itoa", "cfg-if") if c in offline_crates
        ],
        "abs_rerun_in_build_rs": abs_rerun_in_build_rs,
    }


def prepare_cargo_lockfile(
    cargo_bin: str,
    repo_dir: Path,
    env: Dict[str, str],
    num_modules: int,
    offline_crates: Dict[str, str],
    *,
    abs_rerun_in_build_rs: bool = False,
) -> Dict[str, Any]:
    """Create the Rust fixture and generate `Cargo.lock` offline, falling back cleanly."""
    meta = write_rust_fixture(
        repo_dir,
        num_modules=num_modules,
        offline_crates=offline_crates,
        abs_rerun_in_build_rs=abs_rerun_in_build_rs,
    )
    if offline_crates:
        cp = subprocess.run(
            [cargo_bin, "generate-lockfile", "--offline"],
            cwd=repo_dir,
            env=env,
            text=True,
            capture_output=True,
            timeout=60,
            check=False,
        )
        if cp.returncode == 0:
            return meta
        log(
            f"[rust] offline registry lockfile generation failed ({cp.stderr.strip()[:160]}); "
            "falling back to local workspace crates only"
        )
        meta = write_rust_fixture(
            repo_dir,
            num_modules=num_modules,
            offline_crates={},
            abs_rerun_in_build_rs=abs_rerun_in_build_rs,
        )
    run_cmd([cargo_bin, "generate-lockfile", "--offline"], cwd=repo_dir, env=env, timeout=60.0)
    return meta


def check_target_fingerprint_writable(ws_path: Path) -> bool:
    """Return True if files inside `target/debug/.fingerprint` are writable (CoW/FUSE3)."""
    fp_dir = ws_path / "target" / "debug" / ".fingerprint"
    if not fp_dir.exists():
        return True
    for root, _, files in os.walk(fp_dir):
        for name in files:
            p = Path(root) / name
            try:
                mode = p.stat().st_mode
                return bool(mode & 0o200) and os.access(p, os.W_OK)
            except OSError:
                return False
    return True


def run_rust_fingerprint_probe(
    sb: Sandbox,
    bins: Dict[str, Optional[str]],
    tools: Dict[str, Any],
    base_env: Dict[str, str],
    backend: Optional[str],
    name: str,
    *,
    abs_rerun_in_build_rs: bool = False,
    rustc_remap_mode: Optional[str] = None,
) -> Dict[str, Any]:
    """Probe whether Cargo invalidates fingerprints when workspace moves to `../<repo>.oz/<ws>`."""
    cargo_bin = tools["cargo"]
    git_bin = bins["git"] or "git"
    repo_dir = sb.root / f"rust-probe-{name}"
    env = dict(base_env)
    # Isolate RUSTFLAGS so external shell env cannot interfere with fingerprint checks.
    env.pop("RUSTFLAGS", None)
    env.pop("CARGO_TARGET_DIR", None)

    init_git_repo(repo_dir, git_bin=git_bin, env=env)
    prepare_cargo_lockfile(
        cargo_bin,
        repo_dir,
        env,
        num_modules=4,
        offline_crates=tools["offline_cargo_crates"],
        abs_rerun_in_build_rs=abs_rerun_in_build_rs,
    )
    run_cmd([git_bin, "add", "."], cwd=repo_dir, env=env)
    run_cmd([git_bin, "commit", "-q", "-m", "rust probe fixture"], cwd=repo_dir, env=env)

    base_build_env = dict(env)
    if rustc_remap_mode == "per_workspace_path":
        base_build_env["RUSTFLAGS"] = f"--remap-path-prefix={repo_dir.resolve()}=/oz-ws"

    _, _, _, base_ms = run_cmd(
        [cargo_bin, "check", "--workspace", "--offline"],
        cwd=repo_dir,
        env=base_build_env,
        timeout=180.0,
    )
    oz_setup_and_register(sb, bins["oz"], repo_dir, env, backend)
    ws_path, spawn_ms = oz_spawn_ws(bins["oz"], repo_dir, f"probe-{name}", env, backend)

    ws_env = dict(env)
    ws_env["CARGO_LOG"] = "cargo::core::compiler::fingerprint=info"
    if rustc_remap_mode == "per_workspace_path":
        ws_env["RUSTFLAGS"] = f"--remap-path-prefix={ws_path.resolve()}=/oz-ws"

    rc, _, err, check_ms = run_cmd(
        [cargo_bin, "check", "-vv", "--workspace", "--offline"],
        cwd=ws_path,
        env=ws_env,
        check=False,
        timeout=180.0,
    )
    fp = parse_cargo_fingerprint_logs(err)

    # Under cas_hardlink (0444 target/), Cargo aborts when trying to update `.fingerprint`
    # for an invalidated unit. Inspect Cargo `-vv` Dirty diagnostics on a writable CoW copy.
    if rc != 0 and fp.get("ro_fingerprint_write_attempted"):
        cow_copy = sb.root / f"cow-diag-{name}"
        if cow_copy.exists():
            shutil.rmtree(cow_copy)
        shutil.copytree(ws_path, cow_copy, symlinks=True)
        for root_dir, dirs, files in os.walk(cow_copy / "target"):
            for d in dirs:
                dp = os.path.join(root_dir, d)
                os.chmod(dp, os.stat(dp).st_mode | 0o700)
            for f in files:
                fpath = os.path.join(root_dir, f)
                if not os.path.islink(fpath):
                    os.chmod(fpath, os.stat(fpath).st_mode | 0o200)
        cow_env = dict(env)
        cow_env["CARGO_LOG"] = "cargo::core::compiler::fingerprint=info"
        if rustc_remap_mode == "per_workspace_path":
            cow_env["RUSTFLAGS"] = f"--remap-path-prefix={cow_copy.resolve()}=/oz-ws"
        _, _, cow_err, _ = run_cmd(
            [cargo_bin, "check", "-vv", "--workspace", "--offline"],
            cwd=cow_copy,
            env=cow_env,
            check=False,
            timeout=180.0,
        )
        cow_fp = parse_cargo_fingerprint_logs(cow_err)
        fp["dirty_events"] = cow_fp["dirty_events"]
        fp["dirty_reasons_sample"] = cow_fp["dirty_reasons_sample"]
        fp["fresh_units_logged"] = cow_fp["fresh_units_logged"]
        fp["compiling_units"] = cow_fp["compiling_units"]
        fp["compiling_sample"] = cow_fp["compiling_sample"]

    invalidated = (
        fp["compiling_units"] > 0
        or fp["dirty_events"] > 0
        or fp["ro_fingerprint_write_attempted"]
    )
    return {
        "probe": name,
        "exit_code": rc,
        "base_check_ms": round(base_ms, 2),
        "spawn_ms": round(spawn_ms, 2),
        "ws_first_check_ms": round(check_ms, 2),
        "target_writable": check_target_fingerprint_writable(ws_path),
        "fingerprint": fp,
        "invalidated": invalidated,
        "stderr_tail": "\n".join(err.strip().splitlines()[-8:]) if rc != 0 else "",
    }


def run_rust_sccache_probe(
    sb: Sandbox,
    bins: Dict[str, Optional[str]],
    tools: Dict[str, Any],
    base_env: Dict[str, str],
    backend: Optional[str],
    num_modules: int,
) -> Dict[str, Any]:
    """Compare cold `git worktree` + `sccache` vs warm `oz ws spawn`."""
    sccache_bin = tools["sccache"]
    cargo_bin = tools["cargo"]
    git_bin = bins["git"] or "git"

    sc_root = sb.root / "sccache-isolated"
    sc_dir = sc_root / "cache"
    sc_sock = sc_root / "sccache.sock"
    sc_dir.mkdir(parents=True, exist_ok=True)

    sc_env = dict(base_env)
    sc_env["SCCACHE_DIR"] = str(sc_dir)
    sc_env["SCCACHE_SERVER_UDS"] = str(sc_sock)
    sc_env["RUSTC_WRAPPER"] = sccache_bin
    sc_env["CARGO_INCREMENTAL"] = "0"
    sc_env.pop("RUSTFLAGS", None)

    def read_sc_stats() -> Dict[str, Any]:
        cp = subprocess.run(
            [sccache_bin, "--show-stats", "--stats-format", "json"],
            env=sc_env,
            text=True,
            capture_output=True,
            timeout=10,
            check=False,
        )
        if cp.returncode == 0 and cp.stdout.strip():
            return json.loads(cp.stdout)
        return {}

    def extract_counts(raw: Dict[str, Any]) -> Dict[str, int]:
        stats = raw.get("stats", {})
        hits = sum(stats.get("cache_hits", {}).get("counts", {}).values())
        misses = sum(stats.get("cache_misses", {}).get("counts", {}).values())
        return {
            "compile_requests": int(stats.get("compile_requests", 0)),
            "cache_hits": int(hits),
            "cache_misses": int(misses),
        }

    repo_dir = sb.root / "rust-sccache-repo"
    init_git_repo(repo_dir, git_bin=git_bin, env=sc_env)
    prepare_cargo_lockfile(
        cargo_bin,
        repo_dir,
        sc_env,
        num_modules=num_modules,
        offline_crates=tools["offline_cargo_crates"],
    )
    run_cmd([git_bin, "add", "."], cwd=repo_dir, env=sc_env)
    run_cmd([git_bin, "commit", "-q", "-m", "rust sccache fixture"], cwd=repo_dir, env=sc_env)

    try:
        # 1) Cold git worktree #1 populates sccache from empty cache.
        wt1 = sb.root / "rust-sc-wt1"
        git_worktree_add(git_bin, repo_dir, wt1, "sc-wt1", sc_env)
        _, _, _, wt1_ms = run_cmd(
            [cargo_bin, "check", "--workspace", "--offline"],
            cwd=wt1,
            env=sc_env,
            timeout=180.0,
        )
        stats_after_wt1 = extract_counts(read_sc_stats())
        git_worktree_remove(git_bin, repo_dir, wt1, "sc-wt1", sc_env)

        # 2) Cold git worktree #2 uses the populated sccache across a different directory path.
        wt2 = sb.root / "rust-sc-wt2"
        git_worktree_add(git_bin, repo_dir, wt2, "sc-wt2", sc_env)
        _, _, _, wt2_ms = run_cmd(
            [cargo_bin, "check", "--workspace", "--offline"],
            cwd=wt2,
            env=sc_env,
            timeout=180.0,
        )
        stats_after_wt2 = extract_counts(read_sc_stats())
        git_worktree_remove(git_bin, repo_dir, wt2, "sc-wt2", sc_env)

        # 3) Warm base repo once (with sccache), record baseline stats, then spawn `oz ws spawn`.
        _, _, _, base_warm_ms = run_cmd(
            [cargo_bin, "check", "--workspace", "--offline"],
            cwd=repo_dir,
            env=sc_env,
            timeout=180.0,
        )
        stats_after_base = extract_counts(read_sc_stats())
        oz_setup_and_register(sb, bins["oz"], repo_dir, sc_env, backend)
        ws_sc, oz_spawn_ms = oz_spawn_ws(bins["oz"], repo_dir, "ws-sccache", sc_env, backend)
        _, _, _, oz_warm_check_ms = run_cmd(
            [cargo_bin, "check", "--workspace", "--offline"],
            cwd=ws_sc,
            env=sc_env,
            timeout=180.0,
        )
        stats_after_oz = extract_counts(read_sc_stats())

        return {
            "status": "ok",
            "cold_worktree_miss_ms": round(wt1_ms, 2),
            "cold_worktree_sccache_warm_ms": round(wt2_ms, 2),
            "base_warm_ms": round(base_warm_ms, 2),
            "oz_spawn_ms": round(oz_spawn_ms, 2),
            "oz_warm_first_check_ms": round(oz_warm_check_ms, 2),
            "stats_after_wt1": stats_after_wt1,
            "stats_delta_wt2": {
                "hits": stats_after_wt2["cache_hits"] - stats_after_wt1["cache_hits"],
                "misses": stats_after_wt2["cache_misses"] - stats_after_wt1["cache_misses"],
            },
            "stats_delta_oz_warm": {
                "compile_requests": stats_after_oz["compile_requests"]
                - stats_after_base["compile_requests"],
                "hits": stats_after_oz["cache_hits"] - stats_after_base["cache_hits"],
                "misses": stats_after_oz["cache_misses"] - stats_after_base["cache_misses"],
            },
        }
    finally:
        subprocess.run(
            [sccache_bin, "--stop-server"],
            env=sc_env,
            text=True,
            capture_output=True,
            timeout=5,
            check=False,
        )


def bench_rust(
    sb: Sandbox,
    bins: Dict[str, Optional[str]],
    tools: Dict[str, Any],
    base_env: Dict[str, str],
    backend: Optional[str],
    *,
    num_workspaces: int,
    quick: bool,
    run_sccache: bool,
) -> Dict[str, Any]:
    """Run Rust warm build benchmark, fingerprint path probes, and optional sccache probe."""
    cargo_bin = tools["cargo"]
    if not cargo_bin:
        return {"status": "skipped", "reason": "cargo not installed"}

    git_bin = bins["git"] or "git"
    num_modules = 6 if quick else 16
    env = dict(base_env)
    env.pop("RUSTFLAGS", None)
    env.pop("CARGO_TARGET_DIR", None)

    repo_dir = sb.root / "rust-main"
    init_git_repo(repo_dir, git_bin=git_bin, env=env)
    fixture_meta = prepare_cargo_lockfile(
        cargo_bin,
        repo_dir,
        env,
        num_modules=num_modules,
        offline_crates=tools["offline_cargo_crates"],
    )
    run_cmd([git_bin, "add", "."], cwd=repo_dir, env=env)
    run_cmd([git_bin, "commit", "-q", "-m", "initial rust fixture"], cwd=repo_dir, env=env)

    # 1) Cold git worktree measurement (`git worktree add` + full `cargo check` & `cargo test`).
    cold_add_samples: List[float] = []
    cold_check_samples: List[float] = []
    cold_test_samples: List[float] = []
    cold_total_samples: List[float] = []
    cold_tests_passed = 0

    for i in range(num_workspaces):
        wt_dir = sb.root / f"rust-cold-wt-{i}"
        branch = f"cold-wt-{i}"
        add_ms = git_worktree_add(git_bin, repo_dir, wt_dir, branch, env)
        _, _, _, chk_ms = run_cmd(
            [cargo_bin, "check", "--workspace", "--offline"],
            cwd=wt_dir,
            env=env,
            timeout=180.0,
        )
        _, test_out, _, tst_ms = run_cmd(
            [cargo_bin, "test", "--workspace", "--offline"],
            cwd=wt_dir,
            env=env,
            timeout=180.0,
        )
        cold_tests_passed = count_cargo_tests_passed(test_out)
        cold_add_samples.append(add_ms)
        cold_check_samples.append(chk_ms)
        cold_test_samples.append(tst_ms)
        cold_total_samples.append(add_ms + chk_ms + tst_ms)
        git_worktree_remove(git_bin, repo_dir, wt_dir, branch, env)

    # 2) Warm base repo once with both `cargo check` and `cargo test`.
    _, _, _, base_warm_check_ms = run_cmd(
        [cargo_bin, "check", "--workspace", "--offline"],
        cwd=repo_dir,
        env=env,
        timeout=180.0,
    )
    _, _, _, base_warm_test_ms = run_cmd(
        [cargo_bin, "test", "--workspace", "--offline"],
        cwd=repo_dir,
        env=env,
        timeout=180.0,
    )

    oz_setup_and_register(sb, bins["oz"], repo_dir, env, backend)

    oz_spawn_samples: List[float] = []
    oz_check_samples: List[float] = []
    oz_test_samples: List[float] = []
    oz_total_samples: List[float] = []
    oz_fp_reports: List[Dict[str, Any]] = []
    oz_tests_passed = 0
    incremental_edit_probe: Optional[Dict[str, Any]] = None

    fp_env = dict(env)
    fp_env["CARGO_LOG"] = "cargo::core::compiler::fingerprint=info"

    for i in range(num_workspaces):
        ws_path, spawn_ms = oz_spawn_ws(bins["oz"], repo_dir, f"rust-ws-{i}", env, backend)
        _, _, chk_err, chk_ms = run_cmd(
            [cargo_bin, "check", "-vv", "--workspace", "--offline"],
            cwd=ws_path,
            env=fp_env,
            timeout=180.0,
        )
        fp = parse_cargo_fingerprint_logs(chk_err)
        _, tst_out, _, tst_ms = run_cmd(
            [cargo_bin, "test", "--workspace", "--offline"],
            cwd=ws_path,
            env=fp_env,
            timeout=180.0,
        )
        oz_tests_passed = count_cargo_tests_passed(tst_out)
        oz_spawn_samples.append(spawn_ms)
        oz_check_samples.append(chk_ms)
        oz_test_samples.append(tst_ms)
        oz_total_samples.append(spawn_ms + chk_ms + tst_ms)
        oz_fp_reports.append(fp)

        if i == 0:
            writable = check_target_fingerprint_writable(ws_path)
            if writable:
                target_file = ws_path / "crates" / "app" / "src" / "lib.rs"
                orig = target_file.read_text(encoding="utf-8")
                target_file.write_text(
                    orig + "\npub fn incremental_marker() -> u64 { BUILD_MAGIC }\n",
                    encoding="utf-8",
                )
                _, _, inc_err, inc_ms = run_cmd(
                    [cargo_bin, "check", "--workspace", "--offline"],
                    cwd=ws_path,
                    env=fp_env,
                    timeout=180.0,
                )
                incremental_edit_probe = {
                    "status": "measured",
                    "oz_incremental_edit_check_ms": round(inc_ms, 2),
                    "fingerprint": parse_cargo_fingerprint_logs(inc_err),
                }
            else:
                incremental_edit_probe = {
                    "status": "not_writable_under_backend",
                    "backend": backend or "auto",
                    "reason": (
                        "cas_hardlink materializes target/ with 0444 CAS hardlinks; "
                        "no-op warm cargo check/test stay Fresh, while source edits require "
                        "FUSE3/reflink copy-up or --target-dir override."
                    ),
                }

    # 3) Path-change fingerprint probes across `./repo` -> `../repo.oz/probe-*`.
    log("[rust] running Cargo path-invalidation fingerprint probes...")
    probe_portable = run_rust_fingerprint_probe(
        sb,
        bins,
        tools,
        base_env,
        backend,
        "portable",
        abs_rerun_in_build_rs=False,
    )
    probe_abs_rerun = run_rust_fingerprint_probe(
        sb,
        bins,
        tools,
        base_env,
        backend,
        "abs-rerun",
        abs_rerun_in_build_rs=True,
    )
    probe_per_ws_remap = run_rust_fingerprint_probe(
        sb,
        bins,
        tools,
        base_env,
        backend,
        "per-ws-remap",
        rustc_remap_mode="per_workspace_path",
    )

    sccache_probe: Dict[str, Any] = {"status": "skipped"}
    if run_sccache and tools.get("sccache"):
        log("[rust] running isolated sccache probe...")
        sccache_probe = run_rust_sccache_probe(
            sb, bins, tools, base_env, backend, num_modules=num_modules
        )
    elif not run_sccache:
        sccache_probe = {"status": "skipped", "reason": "--skip-sccache specified"}
    else:
        sccache_probe = {"status": "skipped", "reason": "sccache not installed"}

    cold_total_med = summarize_ms(cold_total_samples)["median_ms"] or 1.0
    oz_total_med = summarize_ms(oz_total_samples)["median_ms"] or 1.0
    cold_check_med = summarize_ms(cold_check_samples)["median_ms"] or 1.0
    oz_check_med = summarize_ms(oz_check_samples)["median_ms"] or 1.0

    return {
        "status": "ok",
        "fixture": fixture_meta,
        "workspaces_measured": num_workspaces,
        "base_warm_ms": {
            "cargo_check_ms": round(base_warm_check_ms, 2),
            "cargo_test_ms": round(base_warm_test_ms, 2),
        },
        "cold_git_worktree": {
            "worktree_add": summarize_ms(cold_add_samples),
            "cargo_check": summarize_ms(cold_check_samples),
            "cargo_test": summarize_ms(cold_test_samples),
            "total": summarize_ms(cold_total_samples),
            "tests_passed": cold_tests_passed,
        },
        "oz_warm": {
            "spawn": summarize_ms(oz_spawn_samples),
            "first_cargo_check": summarize_ms(oz_check_samples),
            "first_cargo_test": summarize_ms(oz_test_samples),
            "total": summarize_ms(oz_total_samples),
            "tests_passed": oz_tests_passed,
            "fingerprint_samples": oz_fp_reports,
        },
        "speedup": {
            "first_check_x": round(cold_check_med / max(oz_check_med, 0.01), 2),
            "end_to_end_x": round(cold_total_med / max(oz_total_med, 0.01), 2),
        },
        "incremental_edit_probe": incremental_edit_probe,
        "path_invalidation_probes": {
            "portable_workspace": probe_portable,
            "absolute_rerun_if_changed": probe_abs_rerun,
            "per_workspace_remap_path_prefix": probe_per_ws_remap,
        },
        "sccache_probe": sccache_probe,
    }


def write_typescript_fixture(repo_dir: Path, num_modules: int) -> None:
    """Create a multi-file TypeScript project using local `node_modules/.bin/tsc`."""
    src_dir = repo_dir / "src"
    src_dir.mkdir(parents=True, exist_ok=True)
    (repo_dir / ".gitignore").write_text("node_modules/\ndist/\n*.tsbuildinfo\n", encoding="utf-8")
    (repo_dir / "package.json").write_text(
        json.dumps(
            {
                "name": "oz-ts-warm-fixture",
                "version": "1.0.0",
                "private": True,
                "scripts": {"build": "tsc", "typecheck": "tsc --noEmit"},
                "devDependencies": {"typescript": "*"},
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    (repo_dir / "tsconfig.json").write_text(
        json.dumps(
            {
                "compilerOptions": {
                    "target": "ES2022",
                    "module": "CommonJS",
                    "strict": True,
                    "outDir": "dist",
                    "rootDir": "src",
                    "incremental": True,
                    "tsBuildInfoFile": "dist/tsconfig.tsbuildinfo",
                    "skipLibCheck": True,
                },
                "include": ["src/**/*.ts"],
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )

    imports: List[str] = []
    calls: List[str] = []
    for idx in range(num_modules):
        mod = f"calc_{idx:02d}"
        (src_dir / f"{mod}.ts").write_text(
            f"export function compute{idx:02d}(x: number): number {{\n"
            f"  return (x + {idx + 1}) * {(idx % 7) + 3};\n"
            f"}}\n",
            encoding="utf-8",
        )
        imports.append(f'import {{ compute{idx:02d} }} from "./{mod}";')
        calls.append(f"compute{idx:02d}(seed)")

    (src_dir / "index.ts").write_text(
        "\n".join(imports)
        + "\n\nexport function runAll(seed: number): number {\n"
        + "  return "
        + " + ".join(calls)
        + ";\n}\n",
        encoding="utf-8",
    )


def install_typescript_deps(
    workdir: Path,
    tools: Dict[str, Any],
    env: Dict[str, str],
) -> Tuple[str, float]:
    """Install `typescript` offline via `pnpm` (preferred) or `npm`."""
    pnpm_bin = tools.get("pnpm")
    npm_bin = tools.get("npm")

    if pnpm_bin and tools.get("pnpm_store"):
        cp_start = time.perf_counter()
        cp = subprocess.run(
            [pnpm_bin, "install", "--offline", "--ignore-scripts"],
            cwd=workdir,
            env=env,
            text=True,
            capture_output=True,
            timeout=120,
            check=False,
        )
        elapsed_ms = (time.perf_counter() - cp_start) * 1000.0
        if cp.returncode == 0 and (workdir / "node_modules" / ".bin" / "tsc").exists():
            return "pnpm --offline", elapsed_ms

    if npm_bin:
        cp_start = time.perf_counter()
        cp = subprocess.run(
            [npm_bin, "install", "--offline", "--no-audit", "--no-fund"],
            cwd=workdir,
            env=env,
            text=True,
            capture_output=True,
            timeout=120,
            check=False,
        )
        elapsed_ms = (time.perf_counter() - cp_start) * 1000.0
        if cp.returncode == 0 and (workdir / "node_modules" / ".bin" / "tsc").exists():
            return "npm --offline", elapsed_ms

    raise RuntimeError(
        "No offline TypeScript package found in pnpm store or npm cache; cannot install tsc offline"
    )


def bench_typescript(
    sb: Sandbox,
    bins: Dict[str, Optional[str]],
    tools: Dict[str, Any],
    base_env: Dict[str, str],
    backend: Optional[str],
    *,
    num_workspaces: int,
    quick: bool,
) -> Dict[str, Any]:
    """Measure cold `git worktree` (install + tsc) vs `oz ws spawn` (warm node_modules + tsc)."""
    if not tools.get("node") or (not tools.get("pnpm") and not tools.get("npm")):
        return {"status": "skipped", "reason": "node and pnpm/npm not available"}
    if not tools.get("pnpm_store") and not tools.get("npm"):
        return {"status": "skipped", "reason": "no offline pnpm/npm store found"}

    git_bin = bins["git"] or "git"
    node_bin = tools["node"]
    num_modules = 10 if quick else 30
    env = dict(base_env)

    repo_dir = sb.root / "ts-main"
    init_git_repo(repo_dir, git_bin=git_bin, env=env)
    write_typescript_fixture(repo_dir, num_modules=num_modules)
    run_cmd([git_bin, "add", "."], cwd=repo_dir, env=env)
    run_cmd([git_bin, "commit", "-q", "-m", "initial ts fixture"], cwd=repo_dir, env=env)

    try:
        pm_used, base_install_ms = install_typescript_deps(repo_dir, tools, env)
    except RuntimeError as exc:
        return {"status": "skipped", "reason": str(exc)}

    _, _, _, base_tsc_ms = run_cmd(
        [str(repo_dir / "node_modules" / ".bin" / "tsc")],
        cwd=repo_dir,
        env=env,
        timeout=120.0,
    )

    cold_add_samples: List[float] = []
    cold_install_samples: List[float] = []
    cold_tsc_samples: List[float] = []
    cold_total_samples: List[float] = []

    for i in range(num_workspaces):
        wt_dir = sb.root / f"ts-cold-wt-{i}"
        branch = f"ts-cold-{i}"
        add_ms = git_worktree_add(git_bin, repo_dir, wt_dir, branch, env)
        _, inst_ms = install_typescript_deps(wt_dir, tools, env)
        _, _, _, tsc_ms = run_cmd(
            [str(wt_dir / "node_modules" / ".bin" / "tsc")],
            cwd=wt_dir,
            env=env,
            timeout=120.0,
        )
        run_cmd(
            [node_bin, "-e", "const m = require('./dist/index.js'); if (m.runAll(10) <= 0) process.exit(1);"],
            cwd=wt_dir,
            env=env,
            timeout=30.0,
        )
        cold_add_samples.append(add_ms)
        cold_install_samples.append(inst_ms)
        cold_tsc_samples.append(tsc_ms)
        cold_total_samples.append(add_ms + inst_ms + tsc_ms)
        git_worktree_remove(git_bin, repo_dir, wt_dir, branch, env)

    oz_setup_and_register(sb, bins["oz"], repo_dir, env, backend)
    oz_spawn_samples: List[float] = []
    oz_tsc_noemit_samples: List[float] = []
    oz_tsc_emit_samples: List[float] = []
    oz_total_samples: List[float] = []
    oz_inc_edit_samples: List[float] = []

    for i in range(num_workspaces):
        ws_path, spawn_ms = oz_spawn_ws(bins["oz"], repo_dir, f"ts-ws-{i}", env, backend)
        tsc_bin = str(ws_path / "node_modules" / ".bin" / "tsc")
        _, _, _, noemit_ms = run_cmd(
            [tsc_bin, "--noEmit"],
            cwd=ws_path,
            env=env,
            timeout=120.0,
        )
        _, _, _, emit_ms = run_cmd(
            [tsc_bin],
            cwd=ws_path,
            env=env,
            timeout=120.0,
        )
        run_cmd(
            [node_bin, "-e", "const m = require('./dist/index.js'); if (m.runAll(10) <= 0) process.exit(1);"],
            cwd=ws_path,
            env=env,
            timeout=30.0,
        )
        # Incremental edit in tracked src file + recompile into dist/.
        mod_file = ws_path / "src" / "calc_00.ts"
        mod_file.write_text(
            mod_file.read_text(encoding="utf-8") + "\nexport const OZ_MARKER = 1;\n",
            encoding="utf-8",
        )
        _, _, _, inc_ms = run_cmd(
            [tsc_bin, "--noEmit"],
            cwd=ws_path,
            env=env,
            timeout=120.0,
        )
        oz_spawn_samples.append(spawn_ms)
        oz_tsc_noemit_samples.append(noemit_ms)
        oz_tsc_emit_samples.append(emit_ms)
        oz_total_samples.append(spawn_ms + emit_ms)
        oz_inc_edit_samples.append(inc_ms)

    cold_med = summarize_ms(cold_total_samples)["median_ms"] or 1.0
    oz_med = summarize_ms(oz_total_samples)["median_ms"] or 1.0
    return {
        "status": "ok",
        "package_manager": pm_used,
        "modules": num_modules,
        "workspaces_measured": num_workspaces,
        "base_setup_ms": {
            "install_ms": round(base_install_ms, 2),
            "tsc_ms": round(base_tsc_ms, 2),
        },
        "cold_git_worktree": {
            "worktree_add": summarize_ms(cold_add_samples),
            "install": summarize_ms(cold_install_samples),
            "tsc_build": summarize_ms(cold_tsc_samples),
            "total": summarize_ms(cold_total_samples),
        },
        "oz_warm": {
            "spawn": summarize_ms(oz_spawn_samples),
            "first_tsc_noemit": summarize_ms(oz_tsc_noemit_samples),
            "first_tsc_emit": summarize_ms(oz_tsc_emit_samples),
            "incremental_edit_noemit": summarize_ms(oz_inc_edit_samples),
            "total_emit": summarize_ms(oz_total_samples),
        },
        "speedup": {
            "end_to_end_emit_x": round(cold_med / max(oz_med, 0.01), 2),
        },
    }


def write_python_fixture(repo_dir: Path, num_tests: int) -> None:
    """Create a realistic Python package + pytest suite."""
    pkg_dir = repo_dir / "pypkg"
    tests_dir = repo_dir / "tests"
    pkg_dir.mkdir(parents=True, exist_ok=True)
    tests_dir.mkdir(parents=True, exist_ok=True)

    (repo_dir / ".gitignore").write_text(".venv/\n__pycache__/\n.pytest_cache/\n.pytest_tmp/\n", encoding="utf-8")
    (repo_dir / "pyproject.toml").write_text(
        "[project]\n"
        'name = "pypkg"\n'
        'version = "0.1.0"\n'
        'requires-python = ">=3.10"\n',
        encoding="utf-8",
    )
    (pkg_dir / "__init__.py").write_text("from .core import compute\n", encoding="utf-8")
    (pkg_dir / "core.py").write_text(
        "def compute(n: int) -> int:\n"
        "    return (n * (n + 1)) // 2\n",
        encoding="utf-8",
    )
    test_lines = [
        "import pathlib",
        "import sys",
        "from pypkg import compute",
        "",
        "def test_sys_prefix_is_venv():",
        "    assert pathlib.Path(sys.prefix).name == '.venv'",
        "",
    ]
    for i in range(num_tests):
        test_lines.extend(
            [
                f"def test_compute_case_{i:02d}():",
                f"    assert compute({i + 1}) == {((i + 1) * (i + 2)) // 2}",
                "",
            ]
        )
    (tests_dir / "test_core.py").write_text("\n".join(test_lines), encoding="utf-8")


def create_python_venv(python_bin: str, workdir: Path, env: Dict[str, str]) -> float:
    """Create `.venv` with system site-packages (pytest) and a real `.venv/bin/pytest` shim."""
    t0 = time.perf_counter()
    venv_dir = workdir / ".venv"
    run_cmd(
        [python_bin, "-m", "venv", "--system-site-packages", "--without-pip", str(venv_dir)],
        cwd=workdir,
        env=env,
        timeout=60.0,
    )
    venv_py = (venv_dir / "bin" / "python3").resolve()
    pytest_shim = venv_dir / "bin" / "pytest"
    if not pytest_shim.exists():
        # Typical pip/uv console script shebang pointing at `<workdir>/.venv/bin/python3`
        pytest_shim.write_text(
            f"#!{venv_dir / 'bin' / 'python3'}\n"
            "import sys\n"
            "from pytest import console_main\n"
            "if __name__ == '__main__':\n"
            "    sys.exit(console_main())\n",
            encoding="utf-8",
        )
        pytest_shim.chmod(0o755)
    # Also create an editable-style `.pth` entry referencing `workdir` to exercise oz-warm rewriting.
    for sp in glob.glob(str(venv_dir / "lib" / "python*" / "site-packages")):
        (Path(sp) / "pypkg_editable.pth").write_text(f"{workdir.resolve()}\n", encoding="utf-8")
        break
    assert venv_py.exists() or (venv_dir / "bin" / "python3").is_symlink()
    return (time.perf_counter() - t0) * 1000.0


def verify_python_ws_isolation(ws_path: Path, main_repo: Path, env: Dict[str, str]) -> Dict[str, Any]:
    """Verify `.venv` inside `ws_path` is isolated from `main_repo` across shebang/cfg/activate/.pth."""
    venv_dir = ws_path / ".venv"
    venv_py = venv_dir / "bin" / "python3"
    pytest_shim = venv_dir / "bin" / "pytest"

    _, sys_prefix_out, _, _ = run_cmd(
        [str(venv_py), "-c", "import sys, pathlib; print(pathlib.Path(sys.prefix).resolve())"],
        cwd=ws_path,
        env=env,
        timeout=15.0,
    )
    _, activate_venv_out, _, _ = run_cmd(
        ["bash", "-c", "source .venv/bin/activate && printf '%s' \"$VIRTUAL_ENV\""],
        cwd=ws_path,
        env=env,
        timeout=15.0,
    )
    # Run from an unrelated directory (`ws_path.parent`) so `.pth` dynamic resolution is proven.
    _, pkg_origin_out, _, _ = run_cmd(
        [
            str(venv_py),
            "-c",
            "import pypkg, pathlib; print(pathlib.Path(pypkg.__file__).resolve())",
        ],
        cwd=ws_path.parent,
        env=env,
        timeout=15.0,
    )
    shim_text = pytest_shim.read_text(encoding="utf-8")
    shim_lines = shim_text.splitlines()
    shebang_head = " | ".join(shim_lines[:2]) if shim_lines else ""
    pyvenv_cfg = (venv_dir / "pyvenv.cfg").read_text(encoding="utf-8")

    main_str = str(main_repo.resolve())
    pth_detached = True
    for sp in glob.glob(str(venv_dir / "lib" / "python*" / "site-packages")):
        pth = Path(sp) / "pypkg_editable.pth"
        if pth.exists():
            content = pth.read_text(encoding="utf-8").strip()
            pth_detached = main_str not in content
        break

    pkg_file = Path(pkg_origin_out.strip()).resolve()
    is_trampoline = (
        shim_lines[:1] == ["#!/bin/sh"]
        and len(shim_lines) >= 2
        and 'dirname "$0"' in shim_lines[1]
        and "python3" in shim_lines[1]
    )
    shebang_ok = (main_str not in shim_text) and (
        is_trampoline or str(venv_dir.resolve()) in shim_lines[0]
    )

    return {
        "sys_prefix_matches_ws_venv": Path(sys_prefix_out.strip()).resolve() == venv_dir.resolve(),
        "activate_virtual_env_matches_ws_venv": Path(activate_venv_out.strip()).resolve()
        == venv_dir.resolve(),
        "pytest_shebang_rewritten": shebang_ok,
        "pyvenv_cfg_detached_from_main": main_str not in pyvenv_cfg,
        "pth_rewritten_to_ws": pth_detached and pkg_file == (ws_path / "pypkg" / "__init__.py").resolve(),
        "shebang_line": shebang_head,
    }


def bench_python(
    sb: Sandbox,
    bins: Dict[str, Optional[str]],
    tools: Dict[str, Any],
    base_env: Dict[str, str],
    backend: Optional[str],
    *,
    num_workspaces: int,
    quick: bool,
) -> Dict[str, Any]:
    """Measure cold `git worktree` (venv + pytest) vs `oz ws spawn` (warm .venv + pytest)."""
    python_bin = tools.get("python3")
    if not python_bin:
        return {"status": "skipped", "reason": "python3 not installed"}

    env = dict(base_env)
    # Check whether system site-packages provides pytest inside venvs.
    probe = subprocess.run(
        [python_bin, "-c", "import pytest; print(pytest.__version__)"],
        env=env,
        text=True,
        capture_output=True,
        timeout=10,
        check=False,
    )
    if probe.returncode != 0:
        return {"status": "skipped", "reason": "pytest importable by python3 not available"}

    git_bin = bins["git"] or "git"
    num_tests = 8 if quick else 24

    repo_dir = sb.root / "py-main"
    init_git_repo(repo_dir, git_bin=git_bin, env=env)
    write_python_fixture(repo_dir, num_tests=num_tests)
    run_cmd([git_bin, "add", "."], cwd=repo_dir, env=env)
    run_cmd([git_bin, "commit", "-q", "-m", "initial python fixture"], cwd=repo_dir, env=env)

    base_venv_ms = create_python_venv(python_bin, repo_dir, env)
    _, _, _, base_pytest_ms = run_cmd(
        [str(repo_dir / ".venv" / "bin" / "pytest"), "-q", "-o", "cache_dir=.pytest_tmp"],
        cwd=repo_dir,
        env=env,
        timeout=60.0,
    )

    cold_add_samples: List[float] = []
    cold_venv_samples: List[float] = []
    cold_pytest_samples: List[float] = []
    cold_total_samples: List[float] = []

    for i in range(num_workspaces):
        wt_dir = sb.root / f"py-cold-wt-{i}"
        branch = f"py-cold-{i}"
        add_ms = git_worktree_add(git_bin, repo_dir, wt_dir, branch, env)
        venv_ms = create_python_venv(python_bin, wt_dir, env)
        _, _, _, py_ms = run_cmd(
            [str(wt_dir / ".venv" / "bin" / "pytest"), "-q", "-o", "cache_dir=.pytest_tmp"],
            cwd=wt_dir,
            env=env,
            timeout=60.0,
        )
        cold_add_samples.append(add_ms)
        cold_venv_samples.append(venv_ms)
        cold_pytest_samples.append(py_ms)
        cold_total_samples.append(add_ms + venv_ms + py_ms)
        git_worktree_remove(git_bin, repo_dir, wt_dir, branch, env)

    oz_setup_and_register(sb, bins["oz"], repo_dir, env, backend)
    oz_spawn_samples: List[float] = []
    oz_pytest_samples: List[float] = []
    oz_total_samples: List[float] = []
    isolation_checks: List[Dict[str, Any]] = []

    for i in range(num_workspaces):
        ws_path, spawn_ms = oz_spawn_ws(bins["oz"], repo_dir, f"py-ws-{i}", env, backend)
        _, _, _, py_ms = run_cmd(
            [str(ws_path / ".venv" / "bin" / "pytest"), "-q", "-o", "cache_dir=.pytest_tmp"],
            cwd=ws_path,
            env=env,
            timeout=60.0,
        )
        oz_spawn_samples.append(spawn_ms)
        oz_pytest_samples.append(py_ms)
        oz_total_samples.append(spawn_ms + py_ms)
        isolation_checks.append(verify_python_ws_isolation(ws_path, repo_dir, env))

    cold_med = summarize_ms(cold_total_samples)["median_ms"] or 1.0
    oz_med = summarize_ms(oz_total_samples)["median_ms"] or 1.0
    return {
        "status": "ok",
        "tests_count": num_tests + 1,
        "workspaces_measured": num_workspaces,
        "base_setup_ms": {
            "venv_create_ms": round(base_venv_ms, 2),
            "pytest_ms": round(base_pytest_ms, 2),
        },
        "cold_git_worktree": {
            "worktree_add": summarize_ms(cold_add_samples),
            "venv_create": summarize_ms(cold_venv_samples),
            "pytest": summarize_ms(cold_pytest_samples),
            "total": summarize_ms(cold_total_samples),
        },
        "oz_warm": {
            "spawn": summarize_ms(oz_spawn_samples),
            "first_pytest": summarize_ms(oz_pytest_samples),
            "total": summarize_ms(oz_total_samples),
            "isolation": isolation_checks[0] if isolation_checks else {},
            "all_workspaces_isolated": all(
                c["sys_prefix_matches_ws_venv"]
                and c["activate_virtual_env_matches_ws_venv"]
                and c["pytest_shebang_rewritten"]
                and c["pyvenv_cfg_detached_from_main"]
                and c["pth_rewritten_to_ws"]
                for c in isolation_checks
            ),
        },
        "speedup": {
            "end_to_end_x": round(cold_med / max(oz_med, 0.01), 2),
        },
    }


def fmt_ms(stats: Optional[Dict[str, Any]]) -> str:
    """Format median ms from a `summarize_ms` dict."""
    if not stats or stats.get("median_ms") is None:
        return "n/a"
    return f"{stats['median_ms']:.1f} ms"


def render_markdown(report: Dict[str, Any]) -> str:
    """Render a concise Markdown summary from the warm builds JSON report."""
    lines = [
        "# Ozone Phase 5A Warm Builds Benchmark",
        "",
        f"- **Mode:** `{report['mode']}` (workspaces per scenario: `{report['workspaces_per_scenario']}`)",
        f"- **Backend:** `{report['oz_backend']}` (`NoNewPrivs={report['no_new_privs']}`)",
        "",
        "## First-Build Latency (`oz ws spawn` vs Cold `git worktree`)",
        "",
        "| Ecosystem | Cold `git worktree` (setup + build) | `oz ws spawn` + First Build | Speedup |",
        "|---|---:|---:|---:|",
    ]

    rust = report["scenarios"].get("rust", {})
    if rust.get("status") == "ok":
        lines.append(
            f"| Rust (`cargo check` + `cargo test`) "
            f"| {fmt_ms(rust['cold_git_worktree']['total'])} "
            f"| {fmt_ms(rust['oz_warm']['total'])} "
            f"| {rust['speedup']['end_to_end_x']:.2f}x |"
        )
    else:
        lines.append(f"| Rust | skipped ({rust.get('reason', 'n/a')}) | — | — |")

    ts = report["scenarios"].get("typescript", {})
    if ts.get("status") == "ok":
        lines.append(
            f"| TypeScript (`{ts['package_manager']}` + `tsc`) "
            f"| {fmt_ms(ts['cold_git_worktree']['total'])} "
            f"| {fmt_ms(ts['oz_warm']['total_emit'])} "
            f"| {ts['speedup']['end_to_end_emit_x']:.2f}x |"
        )
    else:
        lines.append(f"| TypeScript | skipped ({ts.get('reason', 'n/a')}) | — | — |")

    py = report["scenarios"].get("python", {})
    if py.get("status") == "ok":
        lines.append(
            f"| Python (`.venv` + `pytest`) "
            f"| {fmt_ms(py['cold_git_worktree']['total'])} "
            f"| {fmt_ms(py['oz_warm']['total'])} "
            f"| {py['speedup']['end_to_end_x']:.2f}x |"
        )
    else:
        lines.append(f"| Python | skipped ({py.get('reason', 'n/a')}) | — | — |")

    if rust.get("status") == "ok":
        fp_samples = rust.get("oz_warm", {}).get("fingerprint_samples", [])
        sample0 = fp_samples[0] if fp_samples else {}
        lines.extend(
            [
                "",
                "## Cargo Fingerprint Path-Change Analysis (`./<repo>` -> `../<repo>.oz/<ws>`)",
                "",
                f"- `main_rust_fixture`: `fresh_units={sample0.get('fresh_units_logged', 0)}`, "
                f"`compiling={sample0.get('compiling_units', 0)}`, "
                f"`dirty_events={sample0.get('dirty_events', 0)}`, "
                f"`first_check={fmt_ms(rust['oz_warm']['first_cargo_check'])}` "
                f"(vs cold `{fmt_ms(rust['cold_git_worktree']['cargo_check'])}`, "
                f"{rust['speedup']['first_check_x']:.2f}x faster)",
            ]
        )
        for key, probe in rust.get("path_invalidation_probes", {}).items():
            fp = probe.get("fingerprint", {})
            lines.append(
                f"- `{key}`: `invalidated={probe['invalidated']}`, "
                f"`fresh_logged={fp.get('fresh_units_logged')}`, "
                f"`compiling={fp.get('compiling_units')}`, "
                f"`dirty_events={fp.get('dirty_events')}`, "
                f"`ws_check={probe['ws_first_check_ms']:.1f} ms`"
            )
            for reason in fp.get("dirty_reasons_sample", [])[:3]:
                lines.append(f"  - `{reason}`")

        sc = rust.get("sccache_probe", {})
        if sc.get("status") == "ok":
            lines.extend(
                [
                    "",
                    "## Optional `sccache` Rust Probe (Isolated `SCCACHE_DIR`)",
                    "",
                    f"- Cold `git worktree` #1 (empty `sccache`): `{sc['cold_worktree_miss_ms']:.1f} ms`",
                    f"- Cold `git worktree` #2 (`sccache` warm; hits={sc['stats_delta_wt2']['hits']}, misses={sc['stats_delta_wt2']['misses']}): `{sc['cold_worktree_sccache_warm_ms']:.1f} ms`",
                    f"- `oz ws spawn` + warm `cargo check` (`{sc['stats_delta_oz_warm']['compile_requests']}` compile requests): `{sc['oz_warm_first_check_ms']:.1f} ms` (spawn `{sc['oz_spawn_ms']:.1f} ms`)",
                ]
            )

    if py.get("status") == "ok":
        iso = py["oz_warm"].get("isolation", {})
        lines.extend(
            [
                "",
                "## Python `.venv` Workspace Isolation Verification",
                "",
                f"- All spawned workspaces isolated: `{py['oz_warm']['all_workspaces_isolated']}`",
                f"- `sys.prefix` == `<ws>/.venv`: `{iso.get('sys_prefix_matches_ws_venv')}`",
                f"- `source .venv/bin/activate` `$VIRTUAL_ENV` == `<ws>/.venv`: `{iso.get('activate_virtual_env_matches_ws_venv')}`",
                f"- `.venv/bin/pytest` shebang rewritten: `{iso.get('pytest_shebang_rewritten')}` (`{iso.get('shebang_line')}`)",
                f"- `pyvenv.cfg` detached from main repo: `{iso.get('pyvenv_cfg_detached_from_main')}`",
                f"- `.pth` path rewritten to workspace: `{iso.get('pth_rewritten_to_ws')}`",
            ]
        )

    lines.append("")
    return "\n".join(lines)


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Benchmark warm incremental builds after `oz ws spawn` vs cold `git worktree`."
    )
    parser.add_argument(
        "--quick",
        "--smoke",
        dest="quick",
        action="store_true",
        help="Run smoke-scale measurement (1 workspace per scenario, compact fixtures)",
    )
    parser.add_argument(
        "--workspaces",
        type=int,
        default=None,
        help="Workspaces/iterations per scenario (default: 1 in --quick mode, 3 otherwise)",
    )
    parser.add_argument("--oz-bin", default=None, help="Explicit path to `oz` binary")
    parser.add_argument("--vfsd-bin", default=None, help="Explicit path to `oz-vfsd` binary")
    parser.add_argument(
        "--oz-backend",
        default="auto",
        help="Ozone materialization backend (default: auto; e.g. cas_hardlink, linux_fuse3)",
    )
    parser.add_argument("--work-dir", default=None, help="Base directory for temporary repos")
    parser.add_argument(
        "--output",
        default=str(REPO_ROOT / "benchmarks" / "results" / "warm_builds.json"),
        help="Path to write JSON results",
    )
    parser.add_argument(
        "--markdown-out",
        default=None,
        help="Optional path to write Markdown summary (default: sibling WARM_BUILDS.md next to --output)",
    )
    parser.add_argument(
        "--skip-sccache",
        action="store_true",
        help="Skip optional sccache Rust probe",
    )
    parser.add_argument(
        "--keep-sandbox",
        action="store_true",
        help="Preserve temporary benchmark directories after exit",
    )
    return parser.parse_args(argv)


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parse_args(argv)
    num_ws = args.workspaces if args.workspaces is not None else (1 if args.quick else 3)
    bins = discover_binaries(oz_bin_override=args.oz_bin, vfsd_bin_override=args.vfsd_bin)
    if not bins.get("oz"):
        raise SystemExit("ERROR: `oz` binary not found; run `cargo build --bin oz --bin oz-vfsd` or pass --oz-bin")

    base_env = build_base_env(bins)
    work_dir = choose_work_dir_base(args.work_dir)
    tools = detect_toolchains()
    sys_meta = collect_system_metadata(work_dir, bins)

    log(
        f"Starting warm builds benchmark (mode={'quick' if args.quick else 'full'}, "
        f"workspaces={num_ws}, backend={args.oz_backend}, work_dir={work_dir})"
    )

    with Sandbox(
        prefix="ozb-warm-",
        base_dir=work_dir,
        bins=bins,
        keep=args.keep_sandbox,
    ) as sb:
        rust_res = bench_rust(
            sb,
            bins,
            tools,
            base_env,
            args.oz_backend,
            num_workspaces=num_ws,
            quick=args.quick,
            run_sccache=not args.skip_sccache,
        )
        ts_res = bench_typescript(
            sb,
            bins,
            tools,
            base_env,
            args.oz_backend,
            num_workspaces=num_ws,
            quick=args.quick,
        )
        py_res = bench_python(
            sb,
            bins,
            tools,
            base_env,
            args.oz_backend,
            num_workspaces=num_ws,
            quick=args.quick,
        )

    report = {
        "benchmark": "warm_builds",
        "mode": "quick" if args.quick else "full",
        "workspaces_per_scenario": num_ws,
        "oz_backend": args.oz_backend,
        "no_new_privs": detect_no_new_privs(),
        "system": sys_meta,
        "toolchains": tools,
        "scenarios": {
            "rust": rust_res,
            "typescript": ts_res,
            "python": py_res,
        },
    }

    out_path = Path(args.output)
    write_json(out_path, report)

    md_text = render_markdown(report)
    md_path = (
        Path(args.markdown_out)
        if args.markdown_out
        else out_path.with_name("WARM_BUILDS.md")
    )
    md_path.parent.mkdir(parents=True, exist_ok=True)
    md_path.write_text(md_text, encoding="utf-8")

    print(md_text)
    log(f"Wrote JSON report: {out_path}")
    log(f"Wrote Markdown report: {md_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
