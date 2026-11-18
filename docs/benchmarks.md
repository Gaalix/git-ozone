# Ozone Benchmarks & Methodology

Ozone ships a reproducible, self-contained benchmark suite under [`benchmarks/`](../benchmarks/README.md) designed to answer one question honestly: **Is Ozone measurably faster, leaner, and more reliable than canonical C `git` (and optional peer tools like `jj`, `worktrunk`, and `simgit`) on real multi-agent workloads?**

---

## 1. Benchmark Suites & Methodology

All benchmarks invoke real compiled release binaries (`oz`, `oz-vfsd`, `git`, and optional comparison tools when present on `PATH` — never fabricating numbers when a comparison tool is absent) and record full host hardware, OS, kernel, filesystem type, and binary versions.

| Suite | Script | What It Measures | Milestone 4 Target |
| :--- | :--- | :--- | :--- |
| **1. 20-Agent Worktree Spawn & Disk Footprint** | [`benchmarks/bench_worktrees.py`](../benchmarks/bench_worktrees.py) | Spawning 20 isolated workspaces of a realistic Rust + Node repository with large `target/` + `node_modules/` caches (configurable `--cache-mb`, up to 5 GB). Compares `oz ws spawn` (CoW/FUSE3/APFS/ProjFS) vs. `git worktree add` + cache copy (warm parity) AND raw `git worktree add` (cold baseline). Measures wall-clock total & per-workspace p50/p95 latency, real allocated disk blocks (`stat.st_blocks` / filesystem delta), and cross-workspace edit isolation. | **< 500 ms / workspace** and **< 50 MB total extra disk** across 20 workspaces |
| **2. 250k-File Monorepo `status` Latency** | [`benchmarks/bench_status_250k.py`](../benchmarks/bench_status_250k.py) | `git status --porcelain` vs. `oz status --json` (both in a regular repo and inside an `oz ws` CoW/FUSE3 workspace) vs. `jj status` (if installed) across a 250,000-file repository for clean (cold & warm) and dirty (5 modified + 2 untracked) trees, asserting identical dirty-file parity. | **`O(dirty_files)`** sub-10 ms status on VFS/FUSE3 workspaces |
| **3. Agent Token & Step Count (VCBench)** | [`benchmarks/bench_agent_vcbench.py`](../benchmarks/bench_agent_vcbench.py) | Deterministic 4-task agent evaluation (Selective Hunk Commit, Multi-Amend, Split Commits, Conflict Resolution) running real `oz` vs. `git` CLI flows. Reports command count, UTF-8 byte/4 token overhead, lexical token count, and wall-clock time. | **>= 60% fewer commands** and **>= 50% lower token overhead** |
| **4. 10-Agent Concurrent Commit Contention** | [`benchmarks/bench_concurrency.py`](../benchmarks/bench_concurrency.py) | 10 concurrent worker processes committing simultaneously on distinct branches, comparing `.git/index.lock` / `packed-refs.lock` error rate and throughput between C `git` and `oz commit`. | **0% lock errors** under `oz commit` |

---

## 2. How to Reproduce

Build the release binaries and run the full orchestrator:

```bash
cargo build --release --workspace
./benchmarks/run_all.sh
```

The orchestrator writes raw JSON telemetry to `benchmarks/results/latest.json` and renders the formatted Markdown report at [`benchmarks/results/REPORT.md`](../benchmarks/results/REPORT.md).

For a quick local smoke run (smaller scale):

```bash
python3 benchmarks/bench_worktrees.py --agents 5 --cache-mb 50
python3 benchmarks/bench_status_250k.py --files 5000
python3 benchmarks/bench_agent_vcbench.py
python3 benchmarks/bench_concurrency.py
```

See [`benchmarks/README.md`](../benchmarks/README.md) for all CLI flags, optional tool installation (`jj`, `worktrunk`, `simgit`), and hardware/filesystem interpretation guidelines.

---

## 3. v1.0 Release Benchmark Results

> [!IMPORTANT]
> **Status:** Pending final v1.0.0 integrated release benchmark run.
> Once all v1.0.0 release branches are merged into `main`, `./benchmarks/run_all.sh` will run on the release build and populate the live measured report at **[`benchmarks/results/REPORT.md`](../benchmarks/results/REPORT.md)**. No synthetic or estimated numbers are listed here.

| Benchmark Metric | C `git` Baseline | Ozone `oz` v1.0.0 | Target | Source |
| :--- | :---: | :---: | :---: | :--- |
| 20-Agent Worktree Spawn (ms / ws, p50) | *Pending run* | *Pending run* | `< 500 ms` | [`REPORT.md`](../benchmarks/results/REPORT.md) |
| 20-Agent Extra Disk Footprint (5 GB cache) | *Pending run* | *Pending run* | `< 50 MB` | [`REPORT.md`](../benchmarks/results/REPORT.md) |
| 250k-File Monorepo `status` (dirty, warm) | *Pending run* | *Pending run* | `O(dirty_files)` | [`REPORT.md`](../benchmarks/results/REPORT.md) |
| VCBench Command Reduction (`oz` vs `git`) | — | *Pending run* | `>= 60%` | [`REPORT.md`](../benchmarks/results/REPORT.md) |
| VCBench Token Overhead Reduction | — | *Pending run* | `>= 50%` | [`REPORT.md`](../benchmarks/results/REPORT.md) |
| 10-Agent Concurrent Commit Lock Errors | *Pending run* | *Pending run* | `0%` | [`REPORT.md`](../benchmarks/results/REPORT.md) |
