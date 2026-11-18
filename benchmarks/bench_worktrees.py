#!/usr/bin/env python3
# Copyright (c) 2026 Ozone Contributors
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Benchmark Suite 1: Multi-Agent Worktree Spawn Time & Physical Disk Footprint.

Compares spawning N isolated workspaces (default: 20) on a realistic Rust + Node + Python
repository with warm caches (`target/`, `node_modules/`, `.venv/`):
  - `git worktree add` (raw / cold cache)
  - `git worktree add` + warm cache copy (warm-cache parity)
  - `oz ws spawn` (CoW / FUSE3 / APFS / ProjFS with automatic warm cache baseline)
  - `jj workspace add`, `worktrunk`, `simgit` (when installed on PATH; never fabricated)

Measures wall-clock spawn time (total, mean, p50, p95, first vs steady-state),
real physical disk usage (`st_blocks` deduplicated by `(dev, ino)` excluding base repo
inodes + `statvfs` filesystem delta), and verifies cross-workspace isolation.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple

from bench_common import (
    Sandbox,
    build_base_env,
    choose_work_dir_base,
    collect_inodes,
    collect_system_metadata,
    configure_oz_backend,
    detect_no_new_privs,
    discover_binaries,
    fs_allocated_bytes,
    fs_free_bytes,
    init_git_repo,
    is_linux,
    is_windows,
    log,
    measure_disk_usage,
    run_cmd,
    summarize_ms,
    sync_fs,
    write_json,
)

MIB = 1024 * 1024
ROADMAP_LATENCY_TARGET_MS = 500.0
ROADMAP_EXTRA_DISK_TARGET_BYTES = 50 * MIB


def _write_filled_file(path: Path, size_bytes: int, seed_byte: int, mtime_secs: Optional[float] = None) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    chunk_size = min(MIB, max(4096, size_bytes))
    # Deterministic non-zero pattern so filesystems never elide zero blocks into sparse holes.
    pattern = bytes([(seed_byte + (i % 251)) & 0xFF or 1 for i in range(4096)])
    chunk = (pattern * ((chunk_size // len(pattern)) + 1))[:chunk_size]
    remaining = size_bytes
    with path.open("wb") as fh:
        while remaining > 0:
            to_write = chunk if remaining >= len(chunk) else chunk[:remaining]
            fh.write(to_write)
            remaining -= len(to_write)
    if mtime_secs is not None:
        os.utime(path, (mtime_secs, mtime_secs))


def generate_realistic_repo(
    repo_dir: Path,
    git_bin: str,
    env: Dict[str, str],
    source_files: int,
    cache_mb: int,
    cache_files: int,
) -> Dict[str, Any]:
    """Generate a realistic Rust + Node + Python repo with committed sources and warm caches."""
    t0 = time.perf_counter()
    init_git_repo(repo_dir, git_bin=git_bin, env=env)

    now = time.time()
    source_mtime = now - 300.0
    cache_mtime = now - 60.0

    # Manifests & .gitignore
    (repo_dir / ".gitignore").write_text("/target\n/node_modules\n/.venv\n", encoding="utf-8")
    (repo_dir / "README.md").write_text("# Ozone Benchmark Monorepo\n", encoding="utf-8")
    (repo_dir / "Cargo.toml").write_text(
        '[workspace]\nmembers = ["crates/svc_000"]\nresolver = "2"\n',
        encoding="utf-8",
    )
    (repo_dir / "package.json").write_text(
        '{"name": "oz-bench-monorepo", "version": "1.0.0", "private": true}\n',
        encoding="utf-8",
    )
    (repo_dir / "pyproject.toml").write_text(
        '[project]\nname = "oz-bench-monorepo"\nversion = "1.0.0"\n',
        encoding="utf-8",
    )

    # Generate committed Rust + TypeScript source files
    rust_count = max(10, source_files // 2)
    ts_count = max(10, source_files - rust_count)

    svc0_dir = repo_dir / "crates" / "svc_000"
    (svc0_dir / "src").mkdir(parents=True, exist_ok=True)
    (svc0_dir / "Cargo.toml").write_text(
        '[package]\nname = "svc_000"\nversion = "0.1.0"\nedition = "2021"\n',
        encoding="utf-8",
    )
    (svc0_dir / "src" / "lib.rs").write_text(
        "//! Benchmark anchor crate.\npub fn version_tag() -> &'static str {\n    \"baseline-v1\"\n}\n",
        encoding="utf-8",
    )

    for i in range(1, rust_count):
        crate_idx = i % 10
        p = repo_dir / "crates" / f"svc_{crate_idx:03d}" / "src" / f"mod_{i:04d}.rs"
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(
            f"//! Auto-generated benchmark Rust module {i}.\n"
            f"pub fn compute_{i}(x: u64) -> u64 {{\n"
            f"    x.wrapping_mul({i + 17}).wrapping_add(31)\n"
            f"}}\n",
            encoding="utf-8",
        )

    for i in range(ts_count):
        pkg_idx = i % 10
        p = repo_dir / "web" / f"pkg_{pkg_idx:03d}" / "src" / f"comp_{i:04d}.ts"
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(
            f"// Benchmark TypeScript component {i}\n"
            f"export function renderComponent{i}(id: string): string {{\n"
            f"  return `comp-{i}:${{id}}`;\n"
            f"}}\n",
            encoding="utf-8",
        )

    run_cmd([git_bin, "add", "-A"], cwd=repo_dir, env=env)
    run_cmd([git_bin, "commit", "-q", "-m", "chore: seed realistic Rust + Node + Python monorepo"], cwd=repo_dir, env=env)

    # Backdate tracked source files so source_mtime <= cache_mtime (realistic post-build state).
    for root_name in ("crates", "web", "Cargo.toml", "package.json", "pyproject.toml", "README.md"):
        p = repo_dir / root_name
        if p.is_file():
            os.utime(p, (source_mtime, source_mtime))
        elif p.is_dir():
            for dirpath, _, filenames in os.walk(p):
                for fn in filenames:
                    os.utime(os.path.join(dirpath, fn), (source_mtime, source_mtime))

    # Generate warm caches (`target/`, `node_modules/`, `.venv/`) totaling `cache_mb` MiB
    # distributed across `cache_files` files (both bulk binary artifacts + realistic many-file trees).
    total_cache_bytes = max(1, cache_mb) * MIB
    rust_bytes = int(total_cache_bytes * 0.50)
    node_bytes = int(total_cache_bytes * 0.40)
    py_bytes = total_cache_bytes - rust_bytes - node_bytes

    n_files = max(30, cache_files)
    rust_small_files = max(10, int(n_files * 0.35))
    node_small_files = max(10, int(n_files * 0.50))
    py_small_files = max(5, n_files - rust_small_files - node_small_files)

    # 1. Rust `target/debug/deps` + `.fingerprint`
    deps_dir = repo_dir / "target" / "debug" / "deps"
    fp_dir = repo_dir / "target" / "debug" / ".fingerprint"
    deps_dir.mkdir(parents=True, exist_ok=True)
    fp_dir.mkdir(parents=True, exist_ok=True)

    rust_blob_count = min(32, max(4, cache_mb // 64))
    small_payload_rust = b"rlib-meta-placeholder-" * 16  # 352 bytes
    rust_small_total = rust_small_files * len(small_payload_rust)
    rust_bulk = max(0, rust_bytes - rust_small_total)
    per_rlib = rust_bulk // rust_blob_count if rust_blob_count else 0

    for i in range(rust_blob_count):
        sz = per_rlib + (rust_bulk - per_rlib * rust_blob_count if i == 0 else 0)
        if sz > 0:
            _write_filled_file(deps_dir / f"libcore_{i:03d}.rlib", sz, (i * 13 + 7) & 0xFF, cache_mtime)

    for i in range(rust_small_files):
        if i % 3 == 0:
            p = fp_dir / f"crate_{i % 64:03d}" / f"dep_{i:04d}.stamp"
        else:
            p = deps_dir / f"libdep_{i:04d}.rmeta"
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(small_payload_rust)
        os.utime(p, (cache_mtime, cache_mtime))

    # 2. Node `node_modules/pkg-NNN`
    nm_dir = repo_dir / "node_modules"
    nm_dir.mkdir(parents=True, exist_ok=True)
    node_blob_count = min(24, max(4, cache_mb // 80))
    small_payload_node = b"module.exports = { ok: true, id: 'oz-bench-pkg' };\n" * 8
    node_small_total = node_small_files * len(small_payload_node)
    node_bulk = max(0, node_bytes - node_small_total)
    per_bundle = node_bulk // node_blob_count if node_blob_count else 0

    for i in range(node_blob_count):
        sz = per_bundle + (node_bulk - per_bundle * node_blob_count if i == 0 else 0)
        if sz > 0:
            _write_filled_file(
                nm_dir / f"pkg-{i:03d}" / "dist" / "bundle.bin",
                sz,
                (i * 29 + 3) & 0xFF,
                cache_mtime,
            )

    pkg_buckets = max(20, min(200, node_small_files // 5))
    for i in range(node_small_files):
        pkg_dir = nm_dir / f"pkg-{i % pkg_buckets:03d}"
        p = pkg_dir / ("index.js" if i < pkg_buckets else f"lib/mod_{i:04d}.js")
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(small_payload_node)
        os.utime(p, (cache_mtime, cache_mtime))

    # 3. Python `.venv/lib/python3.12/site-packages`
    sp_dir = repo_dir / ".venv" / "lib" / "python3.12" / "site-packages"
    venv_bin = repo_dir / ".venv" / "bin"
    sp_dir.mkdir(parents=True, exist_ok=True)
    venv_bin.mkdir(parents=True, exist_ok=True)
    (repo_dir / ".venv" / "pyvenv.cfg").write_text("home = /usr/bin\n", encoding="utf-8")

    py_blob_count = max(2, min(8, cache_mb // 128 or 2))
    small_payload_py = b"# benchmark python package module\nVALUE = 42\n" * 8
    py_small_total = py_small_files * len(small_payload_py)
    py_bulk = max(0, py_bytes - py_small_total)
    per_wheel = py_bulk // py_blob_count if py_blob_count else 0

    for i in range(py_blob_count):
        sz = per_wheel + (py_bulk - per_wheel * py_blob_count if i == 0 else 0)
        if sz > 0:
            _write_filled_file(
                sp_dir / f"native_ext_{i:02d}" / "weights.bin",
                sz,
                (i * 41 + 11) & 0xFF,
                cache_mtime,
            )

    for i in range(py_small_files):
        p = sp_dir / f"pypkg_{i % 20:02d}" / f"mod_{i:04d}.py"
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(small_payload_py)
        os.utime(p, (cache_mtime, cache_mtime))

    sync_fs()
    cache_usage = measure_disk_usage(
        [repo_dir / "target", repo_dir / "node_modules", repo_dir / ".venv"]
    )
    source_usage = measure_disk_usage([repo_dir / "crates", repo_dir / "web"])
    elapsed_ms = (time.perf_counter() - t0) * 1000.0

    return {
        "setup_elapsed_ms": round(elapsed_ms, 2),
        "source_file_count": source_usage["file_count"],
        "source_physical_bytes": source_usage["physical_blocks_bytes"],
        "cache_file_count": cache_usage["file_count"],
        "cache_physical_bytes": cache_usage["physical_blocks_bytes"],
        "cache_apparent_bytes": cache_usage["apparent_bytes"],
    }


def _copy_cache_dir(src: Path, dst: Path) -> None:
    """Copy a warm cache directory without CoW reflinks on Linux (`cp -R --reflink=never`)."""
    if not src.exists():
        return
    if is_linux() and shutil.which("cp"):
        run_cmd(["cp", "-a", "--reflink=never", str(src), str(dst)], check=True)
    elif not is_windows() and shutil.which("cp"):
        run_cmd(["cp", "-Rp", str(src), str(dst)], check=True)
    else:
        shutil.copytree(src, dst, symlinks=True)


def bench_git_worktrees(
    repo_dir: Path,
    wt_parent: Path,
    git_bin: str,
    env: Dict[str, str],
    agents: int,
    copy_warm_caches: bool,
    base_inodes: set,
    base_info: Dict[str, Any],
) -> Dict[str, Any]:
    """Benchmark `git worktree add` (either raw/cold or with warm cache copy)."""
    label = "git_worktree_warm_copy" if copy_warm_caches else "git_worktree_raw"
    wt_parent.mkdir(parents=True, exist_ok=True)

    # Check free disk space before warm cache copy so we never exhaust user disk.
    if copy_warm_caches:
        estimated_needed = int(base_info["cache_physical_bytes"] * agents * 1.15)
        free_now = fs_free_bytes(wt_parent)
        if estimated_needed > free_now * 0.85:
            return {
                "available": True,
                "skipped": True,
                "warm_cache": True,
                "reason": (
                    f"Skipped {label}: requires ~{estimated_needed / (1024**3):.2f} GiB free, "
                    f"only {free_now / (1024**3):.2f} GiB available."
                ),
            }

    sync_fs()
    fs_before = fs_allocated_bytes(repo_dir)
    per_ws_ms: List[float] = []
    created_paths: List[Path] = []
    branch_prefix = "bench-git-warm" if copy_warm_caches else "bench-git-raw"

    t_total_0 = time.perf_counter()
    for i in range(1, agents + 1):
        ws_dir = wt_parent / f"ws-{i}"
        branch = f"{branch_prefix}-{i}"
        t0 = time.perf_counter()
        run_cmd(
            [git_bin, "worktree", "add", "-q", "-b", branch, str(ws_dir), "main"],
            cwd=repo_dir,
            env=env,
        )
        if copy_warm_caches:
            for cache_name in ("target", "node_modules", ".venv"):
                _copy_cache_dir(repo_dir / cache_name, ws_dir / cache_name)
        per_ws_ms.append((time.perf_counter() - t0) * 1000.0)
        created_paths.append(ws_dir)

    total_wall_ms = (time.perf_counter() - t_total_0) * 1000.0
    sync_fs()
    fs_after = fs_allocated_bytes(repo_dir)
    fs_delta = max(0, fs_after - fs_before)

    usage = measure_disk_usage(
        [*created_paths, repo_dir / ".git" / "worktrees"],
        exclude_inodes=base_inodes,
    )

    # Isolation verification
    isolation_ok = False
    if len(created_paths) >= 2:
        sentinel = created_paths[0] / "crates" / "svc_000" / "src" / "iso_check.rs"
        sentinel.write_text("pub const ISO: u8 = 1;\n", encoding="utf-8")
        isolation_ok = not (created_paths[1] / "crates" / "svc_000" / "src" / "iso_check.rs").exists()
        sentinel.unlink()

    # Clean up git worktrees & branches so disk space is freed before next tool
    for i, ws_dir in enumerate(created_paths, start=1):
        run_cmd([git_bin, "worktree", "remove", "--force", str(ws_dir)], cwd=repo_dir, env=env, check=False)
        if ws_dir.exists():
            shutil.rmtree(ws_dir, ignore_errors=True)
        run_cmd([git_bin, "branch", "-D", f"{branch_prefix}-{i}"], cwd=repo_dir, env=env, check=False)
    run_cmd([git_bin, "worktree", "prune"], cwd=repo_dir, env=env, check=False)
    shutil.rmtree(wt_parent, ignore_errors=True)

    return {
        "available": True,
        "skipped": False,
        "warm_cache": copy_warm_caches,
        "agents": agents,
        "spawn_latency_ms": summarize_ms(per_ws_ms),
        "first_spawn_ms": round(per_ws_ms[0], 3) if per_ws_ms else 0.0,
        "steady_state_ms": summarize_ms(per_ws_ms[1:]) if len(per_ws_ms) > 1 else summarize_ms(per_ws_ms),
        "wall_total_ms": round(total_wall_ms, 3),
        "extra_physical_bytes_st_blocks": usage["physical_blocks_bytes"],
        "extra_physical_bytes_fs_delta": fs_delta,
        "apparent_total_bytes": usage["apparent_bytes"],
        "extra_file_count": usage["file_count"],
        "isolation_verified": isolation_ok,
    }


def bench_oz_workspaces(
    sandbox: Sandbox,
    repo_dir: Path,
    oz_bin: str,
    env: Dict[str, str],
    agents: int,
    base_inodes: set,
    base_info: Dict[str, Any],
    oz_backend: str = "auto",
) -> Dict[str, Any]:
    """Benchmark `oz ws spawn` with warm caches enabled + isolation & disk footprint checks."""
    sandbox.register_oz_repo(repo_dir)
    configure_oz_backend(repo_dir, oz_backend)
    sync_fs()
    fs_before = fs_allocated_bytes(repo_dir)

    per_ws_ms: List[float] = []
    engine_ms: List[float] = []
    backends: List[str] = []
    baseline_sources: List[Optional[str]] = []
    ws_paths: List[Path] = []
    mounted_count = 0

    t_total_0 = time.perf_counter()
    for i in range(1, agents + 1):
        ws_name = f"agent-{i}"
        branch = f"feat/agent-{i}"
        rc, stdout, stderr, elapsed_ms = run_cmd(
            [oz_bin, "ws", "spawn", ws_name, "--branch", branch, "--json"],
            cwd=repo_dir,
            env=env,
            check=False,
        )
        if rc != 0:
            if detect_no_new_privs() and "fusermount3" in stdout + stderr:
                raise RuntimeError(
                    "oz ws spawn failed because this shell has `NoNewPrivs: 1` (preventing setuid `fusermount3`).\n"
                    "Run in a normal user shell for FUSE3 benchmarks, or pass `--oz-backend cas_hardlink` "
                    "to benchmark the universal fallback backend.\n"
                    f"Raw stdout:\n{stdout}\nRaw stderr:\n{stderr}"
                )
            raise RuntimeError(f"oz ws spawn {ws_name} failed (exit {rc}):\nstdout:\n{stdout}\nstderr:\n{stderr}")
        per_ws_ms.append(elapsed_ms)
        parsed = json.loads(stdout)
        if not parsed.get("ok"):
            raise RuntimeError(f"oz ws spawn {ws_name} returned ok=false: {stdout}")
        data = parsed["data"]
        ws_path = Path(data["worktree_path"])
        sandbox.register_mount(ws_path)
        ws_paths.append(ws_path)
        backends.append(str(data.get("backend")))
        baseline_sources.append(data.get("baseline_source"))
        if data.get("mounted"):
            mounted_count += 1
        if isinstance(data.get("spawn_elapsed_ms"), (int, float)):
            engine_ms.append(float(data["spawn_elapsed_ms"]))

    total_wall_ms = (time.perf_counter() - t_total_0) * 1000.0
    sync_fs()
    fs_after = fs_allocated_bytes(repo_dir)
    fs_delta = max(0, fs_after - fs_before)

    # Measure real backing storage on disk (excluding base repo inodes which are hardlinked
    # into the immutable warm cache baseline on ext4, and NOT walking virtual FUSE mounts).
    ozone_dir = repo_dir / ".git" / "ozone"
    backend_set = sorted(set(backends))
    is_vfs_backend = any(b in ("linux_fuse3", "windows_projfs") for b in backend_set)

    backing_paths = [
        ozone_dir / "ws",
        ozone_dir / "baselines",
        ozone_dir / "state",
        ozone_dir / "oplog",
        repo_dir / ".git" / "worktrees",
    ]
    if not is_vfs_backend:
        # For non-VFS backends (APFS clonefile, Btrfs/XFS reflink, CAS hardlink), include workspace dirs too.
        backing_paths.extend(ws_paths)

    backing_usage = measure_disk_usage(backing_paths, exclude_inodes=base_inodes)
    ws_only_usage = measure_disk_usage(
        [ozone_dir / "ws", ozone_dir / "state", repo_dir / ".git" / "worktrees"],
        exclude_inodes=base_inodes,
    )

    # On native CoW filesystems (APFS/Btrfs/ReFS), `st_blocks` counts shared reflink extents
    # per inode, so `fs_delta` reflects true physical space while `st_blocks` is an upper bound.
    # On ext4 + FUSE3, `backing_usage["physical_blocks_bytes"]` is exact physical disk used!
    effective_physical_bytes = (
        backing_usage["physical_blocks_bytes"]
        if is_vfs_backend
        else min(backing_usage["physical_blocks_bytes"], fs_delta) if fs_delta > 0 else backing_usage["physical_blocks_bytes"]
    )
    apparent_total_bytes = agents * (base_info["source_physical_bytes"] + base_info["cache_apparent_bytes"])

    # Verify warm cache visibility AND strict workspace isolation between agent-1 and agent-2!
    warm_cache_visible = False
    isolation_verified = False
    if len(ws_paths) >= 2:
        ws1, ws2 = ws_paths[0], ws_paths[1]
        rlib1 = ws1 / "target" / "debug" / "deps" / "libcore_000.rlib"
        rlib2 = ws2 / "target" / "debug" / "deps" / "libcore_000.rlib"
        pkg1 = ws1 / "node_modules" / "pkg-000" / "index.js"
        warm_cache_visible = (
            rlib1.is_file()
            and rlib2.is_file()
            and pkg1.is_file()
            and rlib1.stat().st_size == (repo_dir / "target" / "debug" / "deps" / "libcore_000.rlib").stat().st_size
        )

        # Write a new file and modify a tracked file in ws1; verify ws2 & main repo are untouched.
        sentinel_rel = Path("crates/svc_000/src/isolation_sentinel.rs")
        mod_rel = Path("crates/svc_000/src/lib.rs")
        orig_lib = (repo_dir / mod_rel).read_text(encoding="utf-8")

        (ws1 / sentinel_rel).write_text("pub const AGENT_ONE_ONLY: bool = true;\n", encoding="utf-8")
        (ws1 / mod_rel).write_text(orig_lib + "\npub fn agent_one_edit() -> u32 { 1 }\n", encoding="utf-8")

        ws2_sees_sentinel = (ws2 / sentinel_rel).exists()
        main_sees_sentinel = (repo_dir / sentinel_rel).exists()
        ws2_lib = (ws2 / mod_rel).read_text(encoding="utf-8")
        main_lib = (repo_dir / mod_rel).read_text(encoding="utf-8")

        # Also check `oz status --json` in ws1 vs ws2
        _, st1_out, _, _ = run_cmd([oz_bin, "status", "--json"], cwd=ws1, env=env)
        _, st2_out, _, _ = run_cmd([oz_bin, "status", "--json"], cwd=ws2, env=env)
        st1 = json.loads(st1_out)
        st2 = json.loads(st2_out)
        ws1_dirty = len(st1.get("data", {}).get("changes", []))
        ws2_dirty = len(st2.get("data", {}).get("changes", []))

        isolation_verified = (
            (not ws2_sees_sentinel)
            and (not main_sees_sentinel)
            and (ws2_lib == orig_lib)
            and (main_lib == orig_lib)
            and (ws1_dirty == 2)
            and (ws2_dirty == 0)
        )

    # Clean up all Ozone workspaces & FUSE mounts cleanly
    sandbox.cleanup_oz_repo(repo_dir)

    summary_all = summarize_ms(per_ws_ms)
    summary_steady = summarize_ms(per_ws_ms[1:]) if len(per_ws_ms) > 1 else summary_all

    return {
        "available": True,
        "skipped": False,
        "warm_cache": True,
        "warm_cache_visible": warm_cache_visible,
        "agents": agents,
        "backends": backend_set,
        "mounted_count": mounted_count,
        "baseline_sources": baseline_sources,
        "spawn_latency_ms": summary_all,
        "engine_spawn_latency_ms": summarize_ms(engine_ms),
        "first_spawn_ms": round(per_ws_ms[0], 3) if per_ws_ms else 0.0,
        "steady_state_ms": summary_steady,
        "wall_total_ms": round(total_wall_ms, 3),
        "extra_physical_bytes_st_blocks": backing_usage["physical_blocks_bytes"],
        "extra_physical_bytes_ws_only": ws_only_usage["physical_blocks_bytes"],
        "extra_physical_bytes_fs_delta": fs_delta,
        "effective_extra_physical_bytes": effective_physical_bytes,
        "apparent_total_bytes": apparent_total_bytes,
        "isolation_verified": isolation_verified,
        "roadmap_gates": {
            "latency_target_ms": ROADMAP_LATENCY_TARGET_MS,
            "extra_disk_target_bytes": ROADMAP_EXTRA_DISK_TARGET_BYTES,
            "mean_latency_pass": summary_all["mean_ms"] < ROADMAP_LATENCY_TARGET_MS,
            "steady_state_p95_pass": summary_steady["p95_ms"] < ROADMAP_LATENCY_TARGET_MS,
            "extra_disk_pass": effective_physical_bytes < ROADMAP_EXTRA_DISK_TARGET_BYTES,
        },
    }


def bench_optional_jj(
    repo_dir: Path,
    wt_parent: Path,
    jj_bin: Optional[str],
    env: Dict[str, str],
    agents: int,
    base_inodes: set,
) -> Dict[str, Any]:
    if not jj_bin:
        return {"available": False, "reason": "jj not found on PATH"}

    wt_parent.mkdir(parents=True, exist_ok=True)
    # Colocate jj on a lightweight copy or directly; init colocated jj repo
    rc, _, stderr, _ = run_cmd([jj_bin, "git", "init", "--colocate"], cwd=repo_dir, env=env, check=False)
    if rc != 0:
        return {"available": True, "skipped": True, "reason": f"jj git init --colocate failed: {stderr.strip()}"}

    sync_fs()
    fs_before = fs_allocated_bytes(repo_dir)
    per_ws_ms: List[float] = []
    created_paths: List[Path] = []

    t_total_0 = time.perf_counter()
    for i in range(1, agents + 1):
        ws_dir = wt_parent / f"jj-ws-{i}"
        t0 = time.perf_counter()
        run_cmd(
            [jj_bin, "workspace", "add", str(ws_dir), "--name", f"agent-{i}"],
            cwd=repo_dir,
            env=env,
        )
        per_ws_ms.append((time.perf_counter() - t0) * 1000.0)
        created_paths.append(ws_dir)

    total_wall_ms = (time.perf_counter() - t_total_0) * 1000.0
    sync_fs()
    fs_after = fs_allocated_bytes(repo_dir)
    usage = measure_disk_usage(created_paths, exclude_inodes=base_inodes)

    # Cleanup
    for i, ws_dir in enumerate(created_paths, start=1):
        run_cmd([jj_bin, "workspace", "forget", f"agent-{i}"], cwd=repo_dir, env=env, check=False)
        shutil.rmtree(ws_dir, ignore_errors=True)
    shutil.rmtree(repo_dir / ".jj", ignore_errors=True)
    shutil.rmtree(wt_parent, ignore_errors=True)

    return {
        "available": True,
        "skipped": False,
        "warm_cache": False,
        "agents": agents,
        "spawn_latency_ms": summarize_ms(per_ws_ms),
        "wall_total_ms": round(total_wall_ms, 3),
        "extra_physical_bytes_st_blocks": usage["physical_blocks_bytes"],
        "extra_physical_bytes_fs_delta": max(0, fs_after - fs_before),
        "apparent_total_bytes": usage["apparent_bytes"],
    }


def bench_optional_worktrunk(
    repo_dir: Path,
    wt_bin: Optional[str],
    env: Dict[str, str],
    agents: int,
    base_inodes: set,
) -> Dict[str, Any]:
    if not wt_bin:
        return {"available": False, "reason": "worktrunk (wt) not found on PATH"}

    sync_fs()
    fs_before = fs_allocated_bytes(repo_dir)
    per_ws_ms: List[float] = []
    created_branches: List[str] = []

    t_total_0 = time.perf_counter()
    for i in range(1, agents + 1):
        branch = f"wt-bench-{i}"
        t0 = time.perf_counter()
        rc, _, stderr, _ = run_cmd(
            [wt_bin, "switch", "--create", branch],
            cwd=repo_dir,
            env=env,
            check=False,
        )
        if rc != 0:
            return {
                "available": True,
                "skipped": True,
                "reason": f"worktrunk switch --create failed: {stderr.strip()}",
            }
        per_ws_ms.append((time.perf_counter() - t0) * 1000.0)
        created_branches.append(branch)

    total_wall_ms = (time.perf_counter() - t_total_0) * 1000.0
    sync_fs()
    fs_after = fs_allocated_bytes(repo_dir)

    for branch in created_branches:
        run_cmd([wt_bin, "remove", branch, "--force"], cwd=repo_dir, env=env, check=False)

    return {
        "available": True,
        "skipped": False,
        "warm_cache": False,
        "agents": agents,
        "spawn_latency_ms": summarize_ms(per_ws_ms),
        "wall_total_ms": round(total_wall_ms, 3),
        "extra_physical_bytes_fs_delta": max(0, fs_after - fs_before),
    }


def bench_optional_simgit(
    repo_dir: Path,
    simgit_bin: Optional[str],
    env: Dict[str, str],
    agents: int,
) -> Dict[str, Any]:
    if not simgit_bin:
        return {"available": False, "reason": "simgit not found on PATH"}
    return {
        "available": True,
        "skipped": True,
        "reason": "simgit detected; manual invocation template required for installed CLI version",
    }


def run_benchmark(args: argparse.Namespace) -> Dict[str, Any]:
    bins = discover_binaries(args.oz_bin, args.vfsd_bin)
    if not bins["oz"]:
        raise SystemExit("ERROR: `oz` binary not found. Build with `cargo build --release` or pass --oz-bin.")
    if not bins["git"]:
        raise SystemExit("ERROR: `git` binary not found on PATH.")

    work_base = choose_work_dir_base(args.work_dir)
    env = build_base_env(bins)
    metadata = collect_system_metadata(work_base, bins)

    log(f"[worktrees] Sandbox base: {work_base} (fs={metadata['work_dir_filesystem']})")
    log(
        f"[worktrees] Generating repo: {args.source_files} source files, "
        f"{args.cache_mb} MiB warm cache across {args.cache_files} files..."
    )

    with Sandbox(prefix="ozb-wt-", base_dir=work_base, bins=bins, keep=args.keep_sandbox) as sb:
        repo_dir = sb.root / "monorepo"
        base_info = generate_realistic_repo(
            repo_dir=repo_dir,
            git_bin=str(bins["git"]),
            env=env,
            source_files=args.source_files,
            cache_mb=args.cache_mb,
            cache_files=args.cache_files,
        )
        log(
            f"[worktrees] Generated in {base_info['setup_elapsed_ms']:.0f} ms "
            f"(cache physical={base_info['cache_physical_bytes'] / MIB:.1f} MiB, "
            f"files={base_info['cache_file_count']})"
        )

        base_inodes = collect_inodes(repo_dir)
        results: Dict[str, Any] = {}

        # 1. Raw `git worktree add` (no cache copy — cold cache)
        log(f"[worktrees] Running git worktree add (raw / cold cache) x{args.agents}...")
        results["git_worktree_raw"] = bench_git_worktrees(
            repo_dir=repo_dir,
            wt_parent=sb.root / "git-raw",
            git_bin=str(bins["git"]),
            env=env,
            agents=args.agents,
            copy_warm_caches=False,
            base_inodes=base_inodes,
            base_info=base_info,
        )

        # 2. `git worktree add` + warm cache copy (warm-cache parity)
        if args.skip_git_warm_copy:
            results["git_worktree_warm_copy"] = {
                "available": True,
                "skipped": True,
                "warm_cache": True,
                "reason": "Skipped by --skip-git-warm-copy",
            }
        else:
            log(f"[worktrees] Running git worktree add + warm cache copy x{args.agents}...")
            results["git_worktree_warm_copy"] = bench_git_worktrees(
                repo_dir=repo_dir,
                wt_parent=sb.root / "git-warm",
                git_bin=str(bins["git"]),
                env=env,
                agents=args.agents,
                copy_warm_caches=True,
                base_inodes=base_inodes,
                base_info=base_info,
            )

        # 3. `oz ws spawn` (warm cache + CoW / FUSE3 / APFS / ProjFS)
        log(f"[worktrees] Running oz ws spawn (warm cache, backend={args.oz_backend}) x{args.agents}...")
        results["oz_ws_spawn_warm"] = bench_oz_workspaces(
            sandbox=sb,
            repo_dir=repo_dir,
            oz_bin=str(bins["oz"]),
            env=env,
            agents=args.agents,
            base_inodes=base_inodes,
            base_info=base_info,
            oz_backend=args.oz_backend,
        )

        # 4. Optional comparison tools (only if installed; NEVER fabricated)
        results["jj_workspace_add"] = bench_optional_jj(
            repo_dir=repo_dir,
            wt_parent=sb.root / "jj-ws",
            jj_bin=bins.get("jj"),
            env=env,
            agents=args.agents,
            base_inodes=base_inodes,
        )
        results["worktrunk"] = bench_optional_worktrunk(
            repo_dir=repo_dir,
            wt_bin=bins.get("worktrunk"),
            env=env,
            agents=args.agents,
            base_inodes=base_inodes,
        )
        results["simgit"] = bench_optional_simgit(
            repo_dir=repo_dir,
            simgit_bin=bins.get("simgit"),
            env=env,
            agents=args.agents,
        )

    payload = {
        "suite": "worktrees",
        "config": {
            "agents": args.agents,
            "cache_mb": args.cache_mb,
            "source_files": args.source_files,
            "cache_files": args.cache_files,
            "skip_git_warm_copy": args.skip_git_warm_copy,
        },
        "system": metadata,
        "fixture": base_info,
        "results": results,
    }
    return payload


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Benchmark N-agent worktree spawn time & physical disk footprint (git vs oz vs optional tools)."
    )
    p.add_argument("--agents", type=int, default=20, help="Number of agent workspaces to spawn (default: 20)")
    p.add_argument(
        "--cache-mb",
        type=int,
        default=512,
        help="Warm cache size in MiB across target/, node_modules/, .venv/ (default: 512; use 5000 for 5 GB gate)",
    )
    p.add_argument("--source-files", type=int, default=200, help="Tracked source file count (default: 200)")
    p.add_argument("--cache-files", type=int, default=2000, help="Warm cache file count (default: 2000)")
    p.add_argument(
        "--skip-git-warm-copy",
        action="store_true",
        help="Skip copying warm caches into each git worktree (useful on small disks)",
    )
    p.add_argument("--oz-bin", default=None, help="Path to `oz` binary (auto-detected if omitted)")
    p.add_argument("--vfsd-bin", default=None, help="Path to `oz-vfsd` binary (auto-detected if omitted)")
    p.add_argument(
        "--oz-backend",
        default="auto",
        help="Ozone workspace backend override (`auto`, `linux_fuse3`, `cas_hardlink`, etc.; default: auto)",
    )
    p.add_argument("--work-dir", default=None, help="Base directory for temporary benchmark sandboxes")
    p.add_argument("--output", default=None, help="Optional JSON output file path")
    p.add_argument("--keep-sandbox", action="store_true", help="Keep sandbox directory after run (for debugging)")
    return p.parse_args(argv)


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parse_args(argv)
    payload = run_benchmark(args)
    if args.output:
        write_json(Path(args.output), payload)
        log(f"[worktrees] Wrote JSON results to {args.output}")
    else:
        print(json.dumps(payload, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
