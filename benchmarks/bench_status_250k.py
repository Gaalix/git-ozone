#!/usr/bin/env python3
# Copyright (c) 2026 Ozone Contributors
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Benchmark Suite 2: Large Monorepo (250k-File) `status` Latency.

Generates a real Git repository with `--files` tracked files (default: 250,000) across
realistic multi-level directory depth (`packages/pkg-NNN/src/mod-NN/file_NNNN.rs`) and
measures `status` latency on both clean and dirty working trees:
  - `git status --porcelain`
  - `oz status --json` in a regular Git working tree (directory walk)
  - `oz status --json` inside an `oz ws` CoW/FUSE3 workspace (`O(dirty_files)` fast path)
  - `jj status` (if `jj` is installed on PATH; never fabricated)

Verifies that `oz status --json` (both regular repo and `oz ws` workspace) reports the
exact same dirty/untracked file set as `git status --porcelain`.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
import sys
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Set, Tuple

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


def _create_package_batch(
    repo_dir: Path,
    pkg_idx: int,
    submods: int,
    files_for_pkg: int,
) -> List[str]:
    """Create files for a single package directory and return created relative paths."""
    created: List[str] = []
    pkg_rel = f"packages/pkg-{pkg_idx:03d}/src"
    per_submod, rem = divmod(files_for_pkg, submods)
    for m in range(submods):
        count = per_submod + (1 if m < rem else 0)
        if count == 0:
            continue
        mod_rel = f"{pkg_rel}/mod-{m:02d}"
        mod_dir = repo_dir / mod_rel
        mod_dir.mkdir(parents=True, exist_ok=True)
        for f in range(count):
            fname = f"file_{f:04d}.rs"
            p = mod_dir / fname
            p.write_text(
                f"// pkg={pkg_idx} mod={m} file={f}\npub const ID_{pkg_idx}_{m}_{f}: u32 = {f};\n",
                encoding="utf-8",
            )
            created.append(f"{mod_rel}/{fname}")
    return created


def generate_monorepo(
    repo_dir: Path,
    file_count: int,
    git_bin: str,
    env: Dict[str, str],
) -> Tuple[List[str], Dict[str, Any]]:
    """Generate a real committed Git repository with `file_count` files across 4-level depth."""
    t0 = time.perf_counter()
    init_git_repo(repo_dir, git_bin=git_bin, env=env)
    (repo_dir / ".gitignore").write_text("/target\n/node_modules\n", encoding="utf-8")

    # Realistic 4-level layout: packages/pkg-NNN/src/mod-NN/file_NNNN.rs
    num_pkgs = min(50, max(5, file_count // 500))
    submods_per_pkg = min(20, max(4, file_count // (num_pkgs * 25) or 4))

    base_per_pkg, rem = divmod(file_count, num_pkgs)
    sample_paths: List[str] = []

    t_gen = time.perf_counter()
    workers = min(os.cpu_count() or 4, 8, num_pkgs)
    with ThreadPoolExecutor(max_workers=workers) as pool:
        futures = [
            pool.submit(
                _create_package_batch,
                repo_dir,
                pkg_idx,
                submods_per_pkg,
                base_per_pkg + (1 if pkg_idx < rem else 0),
            )
            for pkg_idx in range(num_pkgs)
        ]
        for fut in futures:
            batch = fut.result()
            if batch:
                sample_paths.append(batch[0])
                if len(batch) > 1:
                    sample_paths.append(batch[-1])
    gen_ms = (time.perf_counter() - t_gen) * 1000.0

    t_git = time.perf_counter()
    run_cmd([git_bin, "add", "-A"], cwd=repo_dir, env=env)
    run_cmd([git_bin, "commit", "-q", "-m", f"chore: seed {file_count}-file monorepo"], cwd=repo_dir, env=env)
    commit_ms = (time.perf_counter() - t_git) * 1000.0
    # Wait past the 1-second racy-git stat granularity window so `git status` warm runs
    # get Git's true best-case index stat cache without racy-git rehashing.
    time.sleep(1.05)

    # Ensure deterministic sorted sample paths for dirty modifications
    sample_paths.sort()
    return sample_paths, {
        "file_count": file_count,
        "packages": num_pkgs,
        "submodules_per_package": submods_per_pkg,
        "file_generation_ms": round(gen_ms, 2),
        "git_initial_commit_ms": round(commit_ms, 2),
        "total_setup_ms": round((time.perf_counter() - t0) * 1000.0, 2),
    }


def parse_git_porcelain_paths(stdout: str) -> Set[str]:
    paths: Set[str] = set()
    for line in stdout.splitlines():
        if len(line) < 4:
            continue
        raw_path = line[3:].strip()
        if " -> " in raw_path:
            raw_path = raw_path.split(" -> ", 1)[1]
        raw_path = raw_path.strip('"')
        if raw_path:
            paths.add(raw_path.replace("\\", "/"))
    return paths


def parse_oz_status_json(stdout: str) -> Tuple[Set[str], bool]:
    parsed = json.loads(stdout)
    if not parsed.get("ok"):
        raise RuntimeError(f"oz status --json returned ok=false: {stdout}")
    data = parsed["data"]
    paths = {str(ch["path"]).replace("\\", "/") for ch in data.get("changes", [])}
    used_fast = bool(data.get("used_fuse_fast_path", False))
    return paths, used_fast


def measure_repeated_status(
    cmd: Sequence[str],
    cwd: Path,
    env: Dict[str, str],
    runs: int,
    measure_cold: bool = True,
) -> Tuple[Dict[str, Any], str]:
    """Run status command (optional cold run + `runs` warm runs) and return stats + last stdout."""
    cold_ms: Optional[float] = None
    last_stdout = ""
    if measure_cold:
        _, last_stdout, _, cold_ms = run_cmd(cmd, cwd=cwd, env=env)

    warm_samples: List[float] = []
    for _ in range(runs):
        _, last_stdout, _, elapsed_ms = run_cmd(cmd, cwd=cwd, env=env)
        warm_samples.append(elapsed_ms)

    result: Dict[str, Any] = {
        "warm_ms": summarize_ms(warm_samples),
    }
    if cold_ms is not None:
        result["cold_ms"] = round(cold_ms, 3)
    return result, last_stdout


def apply_dirty_edits(root: Path, candidate_paths: Sequence[str], n_mod: int, n_untracked: int) -> Set[str]:
    """Apply identical modified + untracked edits and return the expected dirty path set."""
    expected: Set[str] = set()
    step = max(1, len(candidate_paths) // max(1, n_mod))
    chosen = [candidate_paths[min(i * step, len(candidate_paths) - 1)] for i in range(n_mod)]
    # Deduplicate if candidate list is tiny
    chosen = list(dict.fromkeys(chosen))

    for idx, rel in enumerate(chosen):
        p = root / rel
        with p.open("a", encoding="utf-8") as fh:
            fh.write(f"// dirty benchmark edit #{idx}\n")
        expected.add(rel.replace("\\", "/"))

    for u in range(n_untracked):
        rel = f"packages/pkg-000/src/mod-00/untracked_bench_{u}.rs"
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(f"pub const UNTRACKED_{u}: u32 = {u};\n", encoding="utf-8")
        expected.add(rel)

    return expected


def run_benchmark(args: argparse.Namespace) -> Dict[str, Any]:
    bins = discover_binaries(args.oz_bin, args.vfsd_bin)
    if not bins["oz"]:
        raise SystemExit("ERROR: `oz` binary not found. Build with `cargo build --release` or pass --oz-bin.")
    if not bins["git"]:
        raise SystemExit("ERROR: `git` binary not found on PATH.")

    work_base = choose_work_dir_base(args.work_dir)
    env = build_base_env(bins)
    metadata = collect_system_metadata(work_base, bins)

    log(f"[status] Sandbox base: {work_base} (fs={metadata['work_dir_filesystem']})")
    log(f"[status] Generating real Git monorepo with {args.files:,} tracked files...")

    with Sandbox(prefix="ozb-st-", base_dir=work_base, bins=bins, keep=args.keep_sandbox) as sb:
        repo_dir = sb.root / "monorepo"
        sample_paths, fixture_info = generate_monorepo(
            repo_dir=repo_dir,
            file_count=args.files,
            git_bin=str(bins["git"]),
            env=env,
        )
        sb.register_oz_repo(repo_dir)
        log(
            f"[status] Repo ready in {fixture_info['total_setup_ms'] / 1000.0:.2f}s "
            f"(gen={fixture_info['file_generation_ms'] / 1000.0:.2f}s, "
            f"git_commit={fixture_info['git_initial_commit_ms'] / 1000.0:.2f}s)"
        )

        # Spawn an `oz ws` CoW/FUSE3 workspace (`--no-warm` because there are no build caches here)
        configure_oz_backend(repo_dir, args.oz_backend)
        log(f"[status] Spawning `oz ws` workspace (backend={args.oz_backend})...")
        rc_sp, spawn_out, spawn_err, ws_spawn_ms = run_cmd(
            [str(bins["oz"]), "ws", "spawn", "bench-status", "--branch", "feat/bench-status", "--no-warm", "--json"],
            cwd=repo_dir,
            env=env,
            check=False,
        )
        if rc_sp != 0:
            if detect_no_new_privs() and "fusermount3" in spawn_out + spawn_err:
                raise RuntimeError(
                    "oz ws spawn failed because this shell has `NoNewPrivs: 1` (preventing setuid `fusermount3`).\n"
                    "Run in a normal user shell for FUSE3 benchmarks, or pass `--oz-backend cas_hardlink`.\n"
                    f"Raw stdout:\n{spawn_out}\nRaw stderr:\n{spawn_err}"
                )
            raise RuntimeError(f"oz ws spawn failed (exit {rc_sp}):\n{spawn_out}\n{spawn_err}")
        spawn_json = json.loads(spawn_out)
        ws_dir = Path(spawn_json["data"]["worktree_path"])
        ws_backend = str(spawn_json["data"]["backend"])
        ws_mounted = bool(spawn_json["data"].get("mounted", False))
        sb.register_mount(ws_dir)
        log(f"[status] Workspace ready ({ws_backend}, mounted={ws_mounted}) in {ws_spawn_ms:.1f} ms")

        # --- CLEAN TREE BENCHMARKS ---
        log(f"[status] Measuring CLEAN tree latency ({args.runs} warm runs each)...")
        git_clean, git_clean_out = measure_repeated_status(
            [str(bins["git"]), "status", "--porcelain"],
            cwd=repo_dir,
            env=env,
            runs=args.runs,
        )
        oz_regular_clean, oz_reg_clean_out = measure_repeated_status(
            [str(bins["oz"]), "status", "--json"],
            cwd=repo_dir,
            env=env,
            runs=args.runs,
        )
        oz_ws_clean, oz_ws_clean_out = measure_repeated_status(
            [str(bins["oz"]), "status", "--json"],
            cwd=ws_dir,
            env=env,
            runs=args.runs,
        )

        git_clean_set = parse_git_porcelain_paths(git_clean_out)
        oz_reg_clean_set, oz_reg_fast_clean = parse_oz_status_json(oz_reg_clean_out)
        oz_ws_clean_set, oz_ws_fast_clean = parse_oz_status_json(oz_ws_clean_out)
        clean_parity_ok = (len(git_clean_set) == 0) and (len(oz_reg_clean_set) == 0) and (len(oz_ws_clean_set) == 0)
        if not clean_parity_ok:
            raise RuntimeError(
                f"Clean status parity mismatch: git={git_clean_set}, oz_reg={oz_reg_clean_set}, oz_ws={oz_ws_clean_set}"
            )

        # --- DIRTY TREE BENCHMARKS ---
        log(
            f"[status] Applying {args.dirty_modified} modified + {args.dirty_untracked} untracked edits "
            f"and measuring DIRTY tree latency..."
        )
        expected_dirty = apply_dirty_edits(repo_dir, sample_paths, args.dirty_modified, args.dirty_untracked)
        expected_dirty_ws = apply_dirty_edits(ws_dir, sample_paths, args.dirty_modified, args.dirty_untracked)
        assert expected_dirty == expected_dirty_ws

        git_dirty, git_dirty_out = measure_repeated_status(
            [str(bins["git"]), "status", "--porcelain"],
            cwd=repo_dir,
            env=env,
            runs=args.runs,
            measure_cold=False,
        )
        oz_regular_dirty, oz_reg_dirty_out = measure_repeated_status(
            [str(bins["oz"]), "status", "--json"],
            cwd=repo_dir,
            env=env,
            runs=args.runs,
            measure_cold=False,
        )
        oz_ws_dirty, oz_ws_dirty_out = measure_repeated_status(
            [str(bins["oz"]), "status", "--json"],
            cwd=ws_dir,
            env=env,
            runs=args.runs,
            measure_cold=False,
        )

        git_dirty_set = parse_git_porcelain_paths(git_dirty_out)
        oz_reg_dirty_set, oz_reg_fast_dirty = parse_oz_status_json(oz_reg_dirty_out)
        oz_ws_dirty_set, oz_ws_fast_dirty = parse_oz_status_json(oz_ws_dirty_out)

        dirty_parity_ok = (
            git_dirty_set == expected_dirty
            and oz_reg_dirty_set == git_dirty_set
            and oz_ws_dirty_set == git_dirty_set
        )
        if not dirty_parity_ok:
            raise RuntimeError(
                f"Dirty status parity mismatch!\n"
                f"  expected ({len(expected_dirty)}): {sorted(expected_dirty)}\n"
                f"  git      ({len(git_dirty_set)}): {sorted(git_dirty_set)}\n"
                f"  oz_reg   ({len(oz_reg_dirty_set)}): {sorted(oz_reg_dirty_set)}\n"
                f"  oz_ws    ({len(oz_ws_dirty_set)}): {sorted(oz_ws_dirty_set)}"
            )

        # Optional `jj status` (only if installed on PATH)
        jj_result: Dict[str, Any]
        if bins.get("jj"):
            log("[status] Running optional `jj status` comparison...")
            # Clean up dirty files in repo_dir before jj init, or run jj on a clean check then dirty check
            # To avoid modifying repo_dir while oz_ws is mounted, tear down oz_ws first.
            sb.cleanup_oz_repo(repo_dir)
            rc, _, stderr, _ = run_cmd([str(bins["jj"]), "git", "init", "--colocate"], cwd=repo_dir, env=env, check=False)
            if rc == 0:
                jj_dirty, _ = measure_repeated_status(
                    [str(bins["jj"]), "status"],
                    cwd=repo_dir,
                    env=env,
                    runs=args.runs,
                    measure_cold=True,
                )
                jj_result = {
                    "available": True,
                    "dirty": jj_dirty,
                }
            else:
                jj_result = {"available": True, "skipped": True, "reason": f"jj init failed: {stderr.strip()}"}
        else:
            jj_result = {"available": False, "reason": "jj not found on PATH"}

        sb.cleanup_oz_repo(repo_dir)

    git_clean_med = git_clean["warm_ms"]["median_ms"]
    oz_ws_clean_med = oz_ws_clean["warm_ms"]["median_ms"]
    git_dirty_med = git_dirty["warm_ms"]["median_ms"]
    oz_ws_dirty_med = oz_ws_dirty["warm_ms"]["median_ms"]

    return {
        "suite": "status_monorepo",
        "config": {
            "files": args.files,
            "runs": args.runs,
            "dirty_modified": args.dirty_modified,
            "dirty_untracked": args.dirty_untracked,
        },
        "system": metadata,
        "fixture": fixture_info,
        "parity_verified": {
            "clean": clean_parity_ok,
            "dirty": dirty_parity_ok,
            "dirty_file_count": len(expected_dirty),
        },
        "results": {
            "git_status_porcelain": {
                "available": True,
                "clean": git_clean,
                "dirty": git_dirty,
            },
            "oz_status_regular_repo": {
                "available": True,
                "used_fuse_fast_path": oz_reg_fast_clean or oz_reg_fast_dirty,
                "clean": oz_regular_clean,
                "dirty": oz_regular_dirty,
            },
            "oz_status_ws_cow": {
                "available": True,
                "backend": ws_backend,
                "mounted": ws_mounted,
                "ws_spawn_ms": round(ws_spawn_ms, 3),
                "used_fuse_fast_path": oz_ws_fast_clean and oz_ws_fast_dirty,
                "clean": oz_ws_clean,
                "dirty": oz_ws_dirty,
                "speedup_vs_git_clean_x": round(git_clean_med / oz_ws_clean_med, 2) if oz_ws_clean_med > 0 else None,
                "speedup_vs_git_dirty_x": round(git_dirty_med / oz_ws_dirty_med, 2) if oz_ws_dirty_med > 0 else None,
            },
            "jj_status": jj_result,
        },
    }


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Benchmark `status` latency on a large (250k-file) monorepo: git vs oz (regular & CoW/FUSE3) vs jj."
    )
    p.add_argument("--files", type=int, default=250000, help="Number of tracked files to generate (default: 250000)")
    p.add_argument("--runs", type=int, default=7, help="Number of warm iterations per measurement (default: 7)")
    p.add_argument("--dirty-modified", type=int, default=5, help="Number of modified tracked files for dirty test (default: 5)")
    p.add_argument("--dirty-untracked", type=int, default=2, help="Number of untracked files for dirty test (default: 2)")
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
        log(f"[status] Wrote JSON results to {args.output}")
    else:
        print(json.dumps(payload, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
