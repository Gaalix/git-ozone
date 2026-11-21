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

## 3. v1.0.0 Release Benchmark Results

> [!NOTE]
> **Environment & Transparency (`benchmarks/results/latest.json`, commit `4690cbee08b2`, UTC `2026-11-21T21:51:45Z`):**
> - **Hardware / OS:** `AMD Ryzen 9 7900X 12-Core Processor` (24 logical cores, `61.39 GiB` RAM), Ubuntu Linux `7.3.0-10-generic (x86_64)`, `ext4` NVMe (`1520.21 GiB` free).
> - **Binaries:** `oz 1.0.0`, `oz-vfsd 1.0.0`, `git 2.55.0`, `jj 0.47.1`, `worktrunk` (`wt 0.83.0`) (`simgit` not installed — zero fabricated numbers).
> - **Backend (`linux_fuse3`, native host):** `oz doctor` confirmed `/dev/fuse` and `fusermount3 3.18.3` (`recommended_backend = linux_fuse3`), with `20/20` live FUSE3 mounts active across the full 5 GB release gate.
> - **Command executed:**
>   ```bash
>   cargo build --release --workspace
>   ./benchmarks/run_all.sh --full
>   ```

### Summary Scorecard (7/7 ROADMAP Gates PASS)

| Benchmark Metric | C `git 2.55.0` Baseline | Peer Tools (`jj` / `wt`) | Ozone `oz 1.0.0` (`linux_fuse3`) | Target | Result |
| :--- | :---: | :---: | :---: | :---: | :---: |
| **20-Agent Warm Worktree Spawn (`5,000 MiB` cache)** | `22.78 s` / `1.11 s` p50 *(raw cold: `243.9 ms`)* | `jj` `2.61 s` (cold) · `wt` `567.8 ms` (cold) | **`444.5 ms` / `19.8 ms` p50** *(steady p95 `22.2 ms`)* | `< 500 ms` / ws | **PASS** (**51.2x faster** than warm `git`) |
| **20-Agent Extra Disk (`5,000 MiB` warm cache)** | `97.96 GiB` *(raw cold: `17.03 MiB`)* | `jj` `16.72 MiB` · `wt` `20.79 MiB` (cold) | **`1.84 MiB`** (`97.67 GiB` apparent; `960 KiB` ws-only) | `< 50 MiB` total | **PASS** (**~54,404x less disk** than warm `git`) |
| **250k-File Monorepo `status` (`oz ws`, dirty p50)** | `132.1 ms` *(clean `136.8 ms`)* | `jj` `69.6 ms` | **`4.4 ms`** *(clean **`3.3 ms`**, `fast_path=true`)* | `O(dirty_files)` | **PASS** (**30.2x faster** than `git`, **15.8x** vs `jj`) |
| **250k-File Monorepo `status` (regular repo, dirty p50)** | `132.1 ms` *(clean `136.8 ms`)* | `jj` `69.6 ms` | **`118.3 ms`** *(clean **`117.4 ms`**)* | Beat C `git` | **1.12x–1.17x faster** than C `git` |
| **VCBench Agent Commands (4 canonical tasks)** | `50` cmds | — | **`16` cmds (`-68.0%`)** | `>= 60%` reduction | **PASS** |
| **VCBench Agent Tokens (`ceil(bytes/4)` / lexical)** | `4,257` / `4,785` | — | **`1,163` (`-72.7%`) / `1,449` (`-69.7%`)** | `>= 50%` reduction | **PASS** |
| **10-Agent Concurrent Commits (distinct / shared `.git/index`)** | `0%` distinct (`1,131 c/s`) / **`73.3%` (`22/30`)** shared | — | **`0.0%` lock errors (`0/30`)** (`496 c/s`, `git fsck` PASS) | `0%` lock errors | **PASS** |

### Suite 1 — 20-Agent Worktree Spawn & Physical Disk Footprint (5,000 MiB Warm Cache)

Fixture: `201` tracked Rust/TS source files + `4.90 GiB` physical warm caches across `4,065` files (`target/`, `node_modules/`, `.venv/`). All workspaces passed cross-workspace edit isolation and warm cache visibility checks.

| Tool / Mode | Warm Cache | Total Time | Mean / ws | p50 / ws | p95 / ws | Extra Disk (`st_blocks`) | Extra Disk (`statvfs Δ`) | Apparent Size |
| :--- | :---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `git worktree add` (raw / cold) | No | `243.9 ms` | `12.2 ms` | `12.1 ms` | `13.1 ms` | `17.03 MiB` | `0 B` | `900.1 KiB` |
| `git worktree add` (+ warm cache copy) | Yes | `22.78 s` | `1,139 ms` (`1.14 s`) | `1,108 ms` (`1.11 s`) | `1,322 ms` (`1.32 s`) | `97.96 GiB` | `98.05 GiB` | `97.66 GiB` |
| `oz ws spawn` (`linux_fuse3`, warm) | Yes | **`444.5 ms`** | **`22.1 ms`** | **`19.8 ms`** | **`25.6 ms`** | **`1.84 MiB`** | `5.12 MiB` | `97.67 GiB` |
| `jj workspace add` (`jj 0.47.1`) | No | `2,606 ms` (`2.61 s`) | `130.3 ms` | `133.9 ms` | `144.8 ms` | `16.72 MiB` | `23.34 MiB` | `657.8 KiB` |
| `worktrunk` (`wt 0.83.0`) | No | `567.8 ms` | `28.4 ms` | `27.2 ms` | `31.3 ms` | — | `20.79 MiB` | — |

*Breakdown:* Spawn #1 took `79.7 ms` (including warm baseline capture and `oz-vfsd` startup); steady-state spawns #2–20 averaged `19.1 ms` (p95 `22.2 ms`) with `20/20` live FUSE3 mounts. Workspace-only backing disk across all 20 workspaces was **`960.0 KiB`** (~48 KiB/workspace) while exposing **`97.67 GiB`** of apparent warm caches.

### Suite 2 — 250,000-File Monorepo `status` Latency

Dirty edit set: `5` modified + `2` untracked files; `7` warm runs per case; 100% clean and dirty parity verified between `git` and `oz`.

| Command / Context | Fast Path | Clean Cold | Clean Warm p50 | Clean Warm p95 | Dirty Warm p50 | Dirty Warm p95 |
| :--- | :---: | ---: | ---: | ---: | ---: | ---: |
| `git status --porcelain` | index | `135.8 ms` | `136.8 ms` | `137.8 ms` | `132.1 ms` | `133.8 ms` |
| `oz status --json` (regular repo) | `false` (parallel walk) | `120.0 ms` | `117.4 ms` | `120.1 ms` | `118.3 ms` | `122.8 ms` |
| `oz status --json` (`oz ws`, `linux_fuse3`) | **`true`** | **`3.8 ms`** | **`3.3 ms`** | **`3.5 ms`** | **`4.4 ms`** | **`4.7 ms`** |
| `jj status` (`jj 0.47.1`) | — | — | — | — | `69.6 ms` | `71.4 ms` |

### Suite 3 — Agent VCBench (4 Canonical Non-Interactive Tasks)

Strict non-interactive execution (`GIT_TERMINAL_PROMPT=0`, `GIT_EDITOR=true`, `OZ_AGENT=1`) with bit-for-bit Git tree OID, working-tree, `Change-Id:`, and `git fsck --full` parity verification.

| Task | `git` cmds | `oz` cmds | Cmd Δ | `git` tok~ | `oz` tok~ | Tok~ Δ | `git` lex | `oz` lex | Lex Δ | `git` ms | `oz` ms |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1. Selective Hunk Commits | 7 | 2 | **-71.4%** | 794 | 119 | **-85.0%** | 876 | 154 | **-82.4%** | 11.2 | 21.2 |
| 2. Multi-Amends (3x) | 17 | 6 | **-64.7%** | 1,442 | 334 | **-76.8%** | 1,683 | 418 | **-75.2%** | 27.0 | 52.8 |
| 3. Split Commits (3-way) | 14 | 4 | **-71.4%** | 1,084 | 237 | **-78.1%** | 1,198 | 269 | **-77.5%** | 23.8 | 43.4 |
| 4. Conflict Resolution | 12 | 4 | **-66.7%** | 937 | 473 | **-49.5%** | 1,028 | 608 | **-40.9%** | 21.5 | 24.8 |
| **AGGREGATE (All 4 Tasks)** | **50** | **16** | **-68.0%** | **4,257** | **1,163** | **-72.7%** | **4,785** | **1,449** | **-69.7%** | **83.6** | **142.1** |

### Suite 4 — 10-Agent Concurrent Commit Contention (3 Rounds = 30 Attempts)

| Scenario | Tool | Succeeded | Lock Errors | Error Rate | Wall Time | Throughput | `git fsck` |
| :--- | :--- | ---: | ---: | ---: | ---: | ---: | :---: |
| Distinct-branch commits + readers | `git` | 30/30 | 0 | 0.0% | `26.5 ms` | `1,131.2 c/s` | PASS |
| Distinct-branch commits + readers | `oz` | 30/30 | **0** | **0.0%** | `60.5 ms` | `496.0 c/s` | PASS |
| Distinct-branch commits + `pack-refs` | `git` | 30/30 | 0 | 0.0% | `28.8 ms` | `1,042.9 c/s` | PASS |
| Distinct-branch commits + `pack-refs` | `oz` | 30/30 | **0** | **0.0%** | `77.1 ms` | `389.1 c/s` | PASS |
| Shared-worktree `.git/index.lock` contention | `git add` | 8/30 | **22** | **73.3%** | — | — | — |
| Shared-worktree `.git/index.lock` contention | `oz status` (indexless) | **30/30** | **0** | **0.0%** | — | — | — |

---

## 4. Honest Assessment — Where Ozone Wins & Where C `git` Is Still Faster

### Where Ozone Clearly Wins
- **Warm Multi-Agent Workspaces (5 GB Cache):** Across 20 workspaces on `ext4` with live `linux_fuse3`, `oz ws spawn` finished in **`444.5 ms`** (`19.8 ms` p50) using **`1.84 MiB`** of physical disk (`97.67 GiB` apparent) — **51.2x faster** and **~54,404x less disk** than `git worktree add` + cache copy (`22.78 s`, `97.96 GiB`), and even faster/smaller than cold `jj workspace add` (`2.61 s`, `16.72 MiB`) and cold `worktrunk` (`567.8 ms`, `20.79 MiB`).
- **250k-File Monorepo `status`:** Inside an `oz ws` FUSE3 workspace, `oz status --json` hit **`4.4 ms`** dirty p50 (`3.3 ms` clean) — **30.2x faster** than `git status --porcelain` (`132.1 ms`) and **15.8x faster** than `jj status` (`69.6 ms`). Even in a regular non-VFS repo, `oz status --json` (`118.3 ms`) still beat C `git` (`132.1 ms`).
- **Autonomous Agent Workflows (VCBench):** **-68.0% fewer tool calls** (`16` vs `50`) and **-72.7% lower token overhead** (`1,163` vs `4,257`) with bit-for-bit identical Git trees and clean `git fsck`.
- **Zero `.git/index.lock` Races:** Shared-worktree concurrent `git add` failed `73.3%` (`22/30`) on `.git/index.lock`; Ozone is completely stage-free (`0%` lock errors).

### Honest Trade-Offs & Where C `git` Remains Faster
- **Cold Worktrees Without Build Caches:** Raw `git worktree add` (skipping `target/` and `node_modules/`) finished in `243.9 ms` (`12.2 ms`/ws) vs `444.5 ms` (`22.1 ms`/ws) for warm `oz ws spawn` (including `79.7 ms` first-spawn baseline capture). If a repository has no build caches at all, raw `git worktree` is already very fast.
- **Tiny Distinct-Branch Commit Throughput & CLI Wall Time:** C `git` achieves higher raw commit throughput on tiny 1-file distinct-branch commits (`1,042.9–1,131.2 c/s` vs `389.1–496.0 c/s` for `oz`) and lower local wall-clock time on VCBench (`83.6 ms` vs `142.1 ms`) because `oz` journals every mutation to the MVCC OpLog and emits structured `oz.v1` JSON envelopes. For AI agents, saving 34 LLM round-trips dwarfs a 58 ms local CLI delta.
- **Platform Virtualization Requirements:** Zero-byte CoW workspaces on Linux `ext4` require `/dev/fuse` + `fusermount3` (unprivileged FUSE3, not blocked by `NoNewPrivs: 1` container sandboxes); on macOS they use native `APFS clonefile`, on Linux `Btrfs`/`XFS`/`bcachefs` native `FICLONE`, and on Windows `Dev Drive (ReFS)` or `Client-ProjFS`. Without kernel CoW or VFS access, Ozone degrades safely to its `cas_hardlink` fallback.
