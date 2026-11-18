#!/usr/bin/env python3
# Copyright (c) 2026 Ozone Contributors
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Benchmark Suite 4: Multi-Agent Concurrent Commit Contention (`git` vs `oz`).

Tests real concurrent OS-process contention across N agents (default: 10):
  1. Scenario A (`distinct_branch_commits`):
     N isolated workspaces (`git worktree` vs `oz ws`) running R barrier-synchronized
     rounds of simultaneous commits on distinct branches (`feat/agent-1..N`), plus
     2 concurrent read-only observers (`git status`/`git rev-parse --all` vs
     `oz radar --json`/`oz status --json`).
  2. Scenario B (`distinct_branch_commits_under_pack_refs`):
     Same N agents committing simultaneously on distinct branches after `git pack-refs --all`
     while a concurrent ref housekeeping / `pack-refs` operation runs — exercising
     `packed-refs.lock` contention vs Ozone's lock-free CAS ref & OpLog commits.
  3. Scenario C (`shared_worktree_index_contention`):
     Parallel agents staging disjoint files inside the same repository (`git add <file_i>`
     contending on `.git/index.lock` vs Ozone's indexless `oz status --json` / `oz diff --json`).

Reports exact lock error counts, error rate %, successful commits, wall-clock duration,
throughput (commits/sec), and verifies `git fsck --full` and branch ref integrity.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import threading
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple

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

LOCK_ERROR_MARKERS = (
    ".lock",
    "unable to create",
    "cannot lock ref",
    "index.lock",
    "packed-refs.lock",
    "another git process seems to be running",
    "lock file",
)


def is_lock_error(stderr: str, stdout: str = "") -> bool:
    combined = f"{stderr}\n{stdout}".lower()
    return any(marker in combined for marker in LOCK_ERROR_MARKERS)


def _seed_base_repo(repo_dir: Path, git_bin: str, env: Dict[str, str]) -> None:
    init_git_repo(repo_dir, git_bin=git_bin, env=env)
    (repo_dir / "README.md").write_text("# Concurrency Stress Benchmark\n", encoding="utf-8")
    src = repo_dir / "src"
    src.mkdir(parents=True, exist_ok=True)
    (src / "base.rs").write_text("pub fn base() -> u32 { 1 }\n", encoding="utf-8")
    run_cmd([git_bin, "add", "-A"], cwd=repo_dir, env=env)
    run_cmd([git_bin, "commit", "-q", "-m", "chore: seed concurrency base"], cwd=repo_dir, env=env)


def run_distinct_branch_commits_git(
    repo_dir: Path,
    wt_root: Path,
    git_bin: str,
    env: Dict[str, str],
    agents: int,
    rounds: int,
    with_pack_refs_pressure: bool,
) -> Dict[str, Any]:
    wt_root.mkdir(parents=True, exist_ok=True)
    ws_paths: List[Path] = []
    for i in range(1, agents + 1):
        ws_dir = wt_root / f"agent-{i}"
        branch = f"feat/git-agent-{i}"
        run_cmd([git_bin, "worktree", "add", "-q", "-b", branch, str(ws_dir), "main"], cwd=repo_dir, env=env)
        ws_paths.append(ws_dir)

    if with_pack_refs_pressure:
        run_cmd([git_bin, "pack-refs", "--all"], cwd=repo_dir, env=env)

    total_attempts = 0
    succeeded = 0
    lock_errors = 0
    other_errors = 0
    commit_latencies: List[float] = []
    sample_error_msgs: List[str] = []

    t_all_0 = time.perf_counter()
    for r in range(1, rounds + 1):
        for idx, ws_dir in enumerate(ws_paths, start=1):
            p = ws_dir / "src" / f"agent_{idx}_r{r}.rs"
            p.write_text(f"pub fn agent_{idx}_r{r}() -> usize {{ {r} }}\n", encoding="utf-8")

        extra_threads = 2 + (1 if with_pack_refs_pressure else 0)
        barrier = threading.Barrier(agents + extra_threads)

        def worker(i: int, ws: Path) -> Tuple[int, float, str, str]:
            barrier.wait()
            t0 = time.perf_counter()
            rc1, out1, err1, _ = run_cmd([git_bin, "add", f"src/agent_{i}_r{r}.rs"], cwd=ws, env=env, check=False)
            if rc1 != 0:
                return rc1, (time.perf_counter() - t0) * 1000.0, out1, err1
            rc2, out2, err2, _ = run_cmd(
                [git_bin, "commit", "-q", "-m", f"feat(agent-{i}): round {r}"],
                cwd=ws,
                env=env,
                check=False,
            )
            return rc2, (time.perf_counter() - t0) * 1000.0, out2, err2

        def observer_status() -> None:
            barrier.wait()
            run_cmd([git_bin, "status", "--porcelain"], cwd=ws_paths[0], env=env, check=False)

        def observer_refs() -> None:
            barrier.wait()
            run_cmd([git_bin, "for-each-ref"], cwd=repo_dir, env=env, check=False)

        def pack_refs_contender() -> None:
            barrier.wait()
            # Realistic background git maintenance (`git pack-refs --all`) acquiring packed-refs.lock
            run_cmd([git_bin, "pack-refs", "--all"], cwd=repo_dir, env=env, check=False)

        with ThreadPoolExecutor(max_workers=agents + extra_threads) as pool:
            futs = [pool.submit(worker, idx, ws) for idx, ws in enumerate(ws_paths, start=1)]
            obs1 = pool.submit(observer_status)
            obs2 = pool.submit(observer_refs)
            obs3 = pool.submit(pack_refs_contender) if with_pack_refs_pressure else None

            for fut in futs:
                total_attempts += 1
                rc, elapsed_ms, stdout, stderr = fut.result()
                commit_latencies.append(elapsed_ms)
                if rc == 0:
                    succeeded += 1
                else:
                    if is_lock_error(stderr, stdout):
                        lock_errors += 1
                    else:
                        other_errors += 1
                    if len(sample_error_msgs) < 5:
                        sample_error_msgs.append((stderr or stdout).strip().splitlines()[0] if (stderr or stdout).strip() else f"exit {rc}")
            obs1.result()
            obs2.result()
            if obs3 is not None:
                obs3.result()

    wall_total_ms = (time.perf_counter() - t_all_0) * 1000.0
    fsck_rc, _, _, _ = run_cmd([git_bin, "fsck", "--full"], cwd=repo_dir, env=env, check=False)

    return {
        "agents": agents,
        "rounds": rounds,
        "total_commit_attempts": total_attempts,
        "succeeded": succeeded,
        "lock_errors": lock_errors,
        "other_errors": other_errors,
        "lock_error_rate_pct": round(100.0 * lock_errors / total_attempts, 2) if total_attempts else 0.0,
        "total_error_rate_pct": round(100.0 * (lock_errors + other_errors) / total_attempts, 2) if total_attempts else 0.0,
        "wall_total_ms": round(wall_total_ms, 2),
        "throughput_commits_per_sec": round(succeeded / (wall_total_ms / 1000.0), 2) if wall_total_ms > 0 else 0.0,
        "commit_latency_ms": summarize_ms(commit_latencies),
        "git_fsck_clean": fsck_rc == 0,
        "sample_errors": sample_error_msgs,
    }


def run_distinct_branch_commits_oz(
    sb: Sandbox,
    repo_dir: Path,
    oz_bin: str,
    git_bin: str,
    env: Dict[str, str],
    agents: int,
    rounds: int,
    with_pack_refs_pressure: bool,
    oz_backend: str = "auto",
) -> Dict[str, Any]:
    sb.register_oz_repo(repo_dir)
    configure_oz_backend(repo_dir, oz_backend)
    ws_paths: List[Path] = []
    backends: List[str] = []

    for i in range(1, agents + 1):
        ws_name = f"agent-{i}"
        branch = f"feat/oz-agent-{i}"
        rc_sp, stdout, stderr, _ = run_cmd(
            [oz_bin, "ws", "spawn", ws_name, "--branch", branch, "--no-warm", "--json"],
            cwd=repo_dir,
            env=env,
            check=False,
        )
        if rc_sp != 0:
            if detect_no_new_privs() and "fusermount3" in stdout + stderr:
                raise RuntimeError(
                    "oz ws spawn failed because this shell has `NoNewPrivs: 1` (preventing setuid `fusermount3`).\n"
                    "Run in a normal user shell for FUSE3 benchmarks, or pass `--oz-backend cas_hardlink`.\n"
                    f"Raw stdout:\n{stdout}\nRaw stderr:\n{stderr}"
                )
            raise RuntimeError(f"oz ws spawn {ws_name} failed (exit {rc_sp}):\n{stdout}\n{stderr}")
        data = json.loads(stdout)["data"]
        ws_dir = Path(data["worktree_path"])
        sb.register_mount(ws_dir)
        ws_paths.append(ws_dir)
        backends.append(str(data["backend"]))

    if with_pack_refs_pressure:
        run_cmd([git_bin, "pack-refs", "--all"], cwd=repo_dir, env=env)

    total_attempts = 0
    succeeded = 0
    lock_errors = 0
    other_errors = 0
    reader_errors = 0
    commit_latencies: List[float] = []
    sample_error_msgs: List[str] = []

    t_all_0 = time.perf_counter()
    for r in range(1, rounds + 1):
        for idx, ws_dir in enumerate(ws_paths, start=1):
            p = ws_dir / "src" / f"agent_{idx}_r{r}.rs"
            p.write_text(f"pub fn agent_{idx}_r{r}() -> usize {{ {r} }}\n", encoding="utf-8")

        extra_threads = 2 + (1 if with_pack_refs_pressure else 0)
        barrier = threading.Barrier(agents + extra_threads)

        def worker(i: int, ws: Path) -> Tuple[int, float, str, str]:
            barrier.wait()
            rc, out, err, elapsed_ms = run_cmd(
                [oz_bin, "commit", "-m", f"feat(agent-{i}): round {r}", "--json"],
                cwd=ws,
                env=env,
                check=False,
            )
            return rc, elapsed_ms, out, err

        def observer_radar() -> int:
            barrier.wait()
            rc, _, _, _ = run_cmd([oz_bin, "radar", "--json"], cwd=repo_dir, env=env, check=False)
            return rc

        def observer_status() -> int:
            barrier.wait()
            rc, _, _, _ = run_cmd([oz_bin, "status", "--json"], cwd=ws_paths[0], env=env, check=False)
            return rc

        def pack_refs_contender() -> None:
            barrier.wait()
            run_cmd([git_bin, "pack-refs", "--all"], cwd=repo_dir, env=env, check=False)

        with ThreadPoolExecutor(max_workers=agents + extra_threads) as pool:
            futs = [pool.submit(worker, idx, ws) for idx, ws in enumerate(ws_paths, start=1)]
            obs1 = pool.submit(observer_radar)
            obs2 = pool.submit(observer_status)
            obs3 = pool.submit(pack_refs_contender) if with_pack_refs_pressure else None

            for fut in futs:
                total_attempts += 1
                rc, elapsed_ms, stdout, stderr = fut.result()
                commit_latencies.append(elapsed_ms)
                if rc == 0:
                    succeeded += 1
                else:
                    if is_lock_error(stderr, stdout):
                        lock_errors += 1
                    else:
                        other_errors += 1
                    if len(sample_error_msgs) < 5:
                        sample_error_msgs.append((stderr or stdout).strip().splitlines()[0] if (stderr or stdout).strip() else f"exit {rc}")
            if obs1.result() != 0:
                reader_errors += 1
            if obs2.result() != 0:
                reader_errors += 1
            if obs3 is not None:
                obs3.result()

    wall_total_ms = (time.perf_counter() - t_all_0) * 1000.0

    # Verify all branch refs have their final round file committed and git fsck is clean
    refs_verified = True
    for i in range(1, agents + 1):
        rc, out, _, _ = run_cmd(
            [git_bin, "show", f"feat/oz-agent-{i}:src/agent_{i}_r{rounds}.rs"],
            cwd=repo_dir,
            env=env,
            check=False,
        )
        if rc != 0 or f"agent_{i}_r{rounds}" not in out:
            refs_verified = False

    fsck_rc, _, _, _ = run_cmd([git_bin, "fsck", "--full"], cwd=repo_dir, env=env, check=False)
    sb.cleanup_oz_repo(repo_dir)

    return {
        "agents": agents,
        "rounds": rounds,
        "backends": sorted(set(backends)),
        "total_commit_attempts": total_attempts,
        "succeeded": succeeded,
        "lock_errors": lock_errors,
        "other_errors": other_errors,
        "concurrent_reader_errors": reader_errors,
        "lock_error_rate_pct": round(100.0 * lock_errors / total_attempts, 2) if total_attempts else 0.0,
        "total_error_rate_pct": round(100.0 * (lock_errors + other_errors) / total_attempts, 2) if total_attempts else 0.0,
        "wall_total_ms": round(wall_total_ms, 2),
        "throughput_commits_per_sec": round(succeeded / (wall_total_ms / 1000.0), 2) if wall_total_ms > 0 else 0.0,
        "commit_latency_ms": summarize_ms(commit_latencies),
        "branch_refs_verified": refs_verified,
        "git_fsck_clean": fsck_rc == 0,
        "sample_errors": sample_error_msgs,
    }


def run_shared_worktree_index_contention(
    repo_dir_git: Path,
    repo_dir_oz: Path,
    git_bin: str,
    oz_bin: str,
    env: Dict[str, str],
    workers: int,
    rounds: int,
) -> Dict[str, Any]:
    """Compare parallel `git add <disjoint_file>` (`.git/index.lock` contention) vs indexless `oz status --json`."""
    # Pre-create disjoint files in both repos
    for repo in (repo_dir_git, repo_dir_oz):
        for r in range(1, rounds + 1):
            for i in range(1, workers + 1):
                p = repo / "src" / f"shared_{i}_r{r}.rs"
                p.write_text(f"pub const SHARED_{i}_{r}: u32 = {i * 100 + r};\n", encoding="utf-8")

    git_attempts = 0
    git_lock_errors = 0
    git_other_errors = 0
    git_samples: List[str] = []

    for r in range(1, rounds + 1):
        barrier = threading.Barrier(workers)

        def git_stage(i: int) -> Tuple[int, str, str]:
            barrier.wait()
            rc, out, err, _ = run_cmd([git_bin, "add", f"src/shared_{i}_r{r}.rs"], cwd=repo_dir_git, env=env, check=False)
            return rc, out, err

        with ThreadPoolExecutor(max_workers=workers) as pool:
            for fut in [pool.submit(git_stage, i) for i in range(1, workers + 1)]:
                git_attempts += 1
                rc, out, err = fut.result()
                if rc != 0:
                    if is_lock_error(err, out):
                        git_lock_errors += 1
                    else:
                        git_other_errors += 1
                    if len(git_samples) < 5:
                        git_samples.append((err or out).strip().splitlines()[0] if (err or out).strip() else f"exit {rc}")

    # Ozone has NO staging index (stage-free): concurrent `oz status --json` / `oz diff --json` never take index.lock.
    oz_attempts = 0
    oz_lock_errors = 0
    oz_other_errors = 0
    for r in range(1, rounds + 1):
        barrier = threading.Barrier(workers)

        def oz_inspect(i: int) -> Tuple[int, str, str]:
            barrier.wait()
            rc, out, err, _ = run_cmd([oz_bin, "status", "--json"], cwd=repo_dir_oz, env=env, check=False)
            return rc, out, err

        with ThreadPoolExecutor(max_workers=workers) as pool:
            for fut in [pool.submit(oz_inspect, i) for i in range(1, workers + 1)]:
                oz_attempts += 1
                rc, out, err = fut.result()
                if rc != 0:
                    if is_lock_error(err, out):
                        oz_lock_errors += 1
                    else:
                        oz_other_errors += 1

    return {
        "workers": workers,
        "rounds": rounds,
        "git_concurrent_add": {
            "attempts": git_attempts,
            "lock_errors": git_lock_errors,
            "other_errors": git_other_errors,
            "lock_error_rate_pct": round(100.0 * git_lock_errors / git_attempts, 2) if git_attempts else 0.0,
            "sample_errors": git_samples,
        },
        "oz_indexless_concurrent_status": {
            "attempts": oz_attempts,
            "lock_errors": oz_lock_errors,
            "other_errors": oz_other_errors,
            "lock_error_rate_pct": round(100.0 * oz_lock_errors / oz_attempts, 2) if oz_attempts else 0.0,
        },
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

    log(f"[concurrency] Running {args.agents}-agent x {args.rounds}-round contention suite...")
    with Sandbox(prefix="ozb-cc-", base_dir=work_base, bins=bins, keep=args.keep_sandbox) as sb:
        # Scenario A: Clean distinct-branch concurrent commits (symmetric)
        log("[concurrency] Scenario A: distinct-branch concurrent commits (git vs oz)...")
        repo_a_git = sb.root / "sa-git"
        repo_a_oz = sb.root / "sa-oz"
        _seed_base_repo(repo_a_git, str(bins["git"]), env)
        _seed_base_repo(repo_a_oz, str(bins["git"]), env)

        sa_git = run_distinct_branch_commits_git(
            repo_a_git, sb.root / "sa-git-wts", str(bins["git"]), env, args.agents, args.rounds, False
        )
        sa_oz = run_distinct_branch_commits_oz(
            sb, repo_a_oz, str(bins["oz"]), str(bins["git"]), env, args.agents, args.rounds, False, args.oz_backend
        )

        # Scenario B: Distinct-branch concurrent commits under background `git pack-refs --all` pressure
        log("[concurrency] Scenario B: distinct-branch commits under packed-refs contention (git vs oz)...")
        repo_b_git = sb.root / "sb-git"
        repo_b_oz = sb.root / "sb-oz"
        _seed_base_repo(repo_b_git, str(bins["git"]), env)
        _seed_base_repo(repo_b_oz, str(bins["git"]), env)

        sb_git = run_distinct_branch_commits_git(
            repo_b_git, sb.root / "sb-git-wts", str(bins["git"]), env, args.agents, args.rounds, True
        )
        sb_oz = run_distinct_branch_commits_oz(
            sb, repo_b_oz, str(bins["oz"]), str(bins["git"]), env, args.agents, args.rounds, True, args.oz_backend
        )

        # Scenario C: Shared-worktree index contention (`git add` vs indexless `oz`)
        log("[concurrency] Scenario C: shared-worktree `.git/index.lock` contention...")
        repo_c_git = sb.root / "sc-git"
        repo_c_oz = sb.root / "sc-oz"
        _seed_base_repo(repo_c_git, str(bins["git"]), env)
        _seed_base_repo(repo_c_oz, str(bins["git"]), env)
        sc_res = run_shared_worktree_index_contention(
            repo_c_git, repo_c_oz, str(bins["git"]), str(bins["oz"]), env, args.agents, args.rounds
        )

    return {
        "suite": "concurrency",
        "config": {
            "agents": args.agents,
            "rounds": args.rounds,
        },
        "system": metadata,
        "scenarios": {
            "distinct_branch_commits": {
                "git": sa_git,
                "oz": sa_oz,
            },
            "distinct_branch_commits_with_pack_refs": {
                "git": sb_git,
                "oz": sb_oz,
            },
            "shared_worktree_index_contention": sc_res,
        },
        "roadmap_gates": {
            "oz_zero_lock_errors_distinct_branches": (
                sa_oz["lock_errors"] == 0
                and sa_oz["other_errors"] == 0
                and sa_oz["branch_refs_verified"]
                and sa_oz["git_fsck_clean"]
            ),
            "oz_zero_lock_errors_under_pack_refs": (
                sb_oz["lock_errors"] == 0
                and sb_oz["other_errors"] == 0
                and sb_oz["branch_refs_verified"]
                and sb_oz["git_fsck_clean"]
            ),
        },
    }


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Benchmark N-agent concurrent commit contention (git lock errors vs oz lock-free commits)."
    )
    p.add_argument("--agents", type=int, default=10, help="Concurrent agent worker count (default: 10)")
    p.add_argument("--rounds", type=int, default=3, help="Barrier-synchronized commit rounds (default: 3)")
    p.add_argument("--oz-bin", default=None, help="Path to `oz` binary (auto-detected if omitted)")
    p.add_argument("--vfsd-bin", default=None, help="Path to `oz-vfsd` binary (auto-detected if omitted)")
    p.add_argument(
        "--oz-backend",
        default="auto",
        help="Ozone workspace backend override (`auto`, `linux_fuse3`, `cas_hardlink`, etc.; default: auto)",
    )
    p.add_argument("--work-dir", default=None, help="Base directory for temporary benchmark sandboxes")
    p.add_argument("--output", default=None, help="Optional JSON output file path")
    p.add_argument("--keep-sandbox", action="store_true", help="Keep sandbox directory after run")
    return p.parse_args(argv)


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parse_args(argv)
    payload = run_benchmark(args)
    if args.output:
        write_json(Path(args.output), payload)
        log(f"[concurrency] Wrote JSON results to {args.output}")
    else:
        print(json.dumps(payload, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
