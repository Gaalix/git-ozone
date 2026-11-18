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
> **Environment & Backend Transparency (`benchmarks/results/latest.json`, commit `3a240f354701`, UTC `2026-11-18T22:36:15Z`):**
> - **Hardware / OS:** `AMD Ryzen 9 7900X 12-Core Processor` (24 logical cores, `61.39 GiB` RAM), Ubuntu Linux `7.3.0-10-generic (x86_64)`, `ext4` NVMe (`1329.6 GiB` free).
> - **Binaries:** `oz 1.0.0`, `oz-vfsd 1.0.0`, `git 2.55.0` (`jj`, `worktrunk`, `simgit` not installed — zero fabricated numbers).
> - **Backend (`cas_hardlink` under `NoNewPrivs: 1`):** `oz doctor` confirmed `/dev/fuse` and `fusermount3 3.18.3` are installed (`recommended_backend = linux_fuse3`), but this release run executed inside a sandboxed shell with Linux `NoNewPrivs: 1`, which blocks setuid `/usr/bin/fusermount3` from mounting live FUSE3 overlays. Worktree and workspace-status suites therefore ran with `--oz-backend cas_hardlink` (Ozone's zero-privilege container fallback tier).
> - **Command executed:**
>   ```bash
>   ./benchmarks/run_all.sh --agents 20 --cache-mb 512 --files 250000 --oz-backend cas_hardlink --oz-bin ./target/release/oz --vfsd-bin ./target/release/oz-vfsd
>   ```
> - **Reproduce full 5 GB FUSE3 / native CoW run on a standard host shell (`NoNewPrivs: 0`):**
>   ```bash
>   cargo build --release --workspace
>   ./benchmarks/run_all.sh --full
>   ```

### Summary Scorecard

| Benchmark Metric | C `git` 2.55.0 Baseline | Ozone `oz` v1.0.0 (`cas_hardlink`) | Target | Result |
| :--- | :---: | :---: | :---: | :---: |
| **20-Agent Warm Worktree Spawn (total / p50 per ws)** | `5.28 s` / `261.8 ms` *(raw cold: `245.4 ms` / `12.0 ms`)* | **`941.1 ms` / `43.5 ms`** *(steady p95 `46.8 ms`)* | `< 500 ms` / ws | **PASS** (**5.6x faster** than warm `git`) |
| **20-Agent Extra Disk (512 MiB warm cache)** | `10.16 GiB` *(warm copy)* | **`529.11 MiB`** total (`720.0 KiB` ws-only) | `< 50 MiB` *(CoW/FUSE3)* | **19.7x less disk** *(1x `ext4` baseline copy under `cas_hardlink`)* |
| **250k-File Monorepo `status` (regular repo, dirty p50)** | `139.4 ms` *(clean `137.5 ms`)* | **`117.0 ms`** *(clean **`112.4 ms`**)* | Beat C `git` | **1.19x–1.22x faster** than C `git` |
| **250k-File Monorepo `status` (`oz ws`, dirty p50)** | `139.4 ms` | `226.5 ms` *(`cas_hardlink`, `fast_path=false`)* | `O(dirty_files)` *(FUSE3/ProjFS)* | Full walk under `NoNewPrivs: 1` *(host FUSE3 enables fast path)* |
| **VCBench Agent Commands (4 canonical tasks)** | `50` cmds | **`16` cmds (`-68.0%`)** | `>= 60%` reduction | **PASS** |
| **VCBench Agent Tokens (`ceil(bytes/4)` / lexical)** | `4,257` / `4,785` | **`1,163` (`-72.7%`) / `1,449` (`-69.7%`)** | `>= 50%` reduction | **PASS** |
| **10-Agent Concurrent Commits (distinct branches, 30 ops)** | `0%` lock errors (`1,051.8 c/s`) | **`0%` lock errors** (`305.0 c/s`, `git fsck` PASS) | `0%` lock errors | **PASS** |
| **Shared-Worktree `.git/index.lock` Contention (30 ops)** | **`66.7%` lock errors (`20/30`)** | **`0.0%` lock errors (`0/30`, indexless)** | `0%` lock errors | **PASS** |

### Suite 1 — 20-Agent Worktree Spawn & Physical Disk Footprint (512 MiB Warm Cache)

Fixture: `201` tracked Rust/TS source files + `519.13 MiB` physical warm caches across `2,019` files (`target/`, `node_modules/`, `.venv/`). All workspaces passed cross-workspace edit isolation and warm cache visibility checks.

| Tool / Mode | Warm Cache | Total Time | Mean / ws | p50 / ws | p95 / ws | Extra Disk (`st_blocks`) | Extra Disk (`statvfs Δ`) | Apparent Size |
| :--- | :---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `git worktree add` (raw / cold) | No | `245.4 ms` | `12.3 ms` | `12.0 ms` | `13.5 ms` | `17.03 MiB` | `21.42 MiB` | `900.1 KiB` |
| `git worktree add` (+ warm cache copy) | Yes | `5,277.0 ms` (`5.28 s`) | `263.8 ms` | `261.8 ms` | `276.5 ms` | `10.16 GiB` | `10.14 GiB` | `10.00 GiB` |
| `oz ws spawn` (`cas_hardlink`, warm) | Yes | **`941.1 ms`** | **`47.0 ms`** | **`43.5 ms`** | **`50.7 ms`** | **`529.11 MiB`** | `580.02 MiB` | `10.02 GiB` |

*Breakdown:* Spawn #1 took `110.6 ms` (capturing the shared warm baseline into `.git/ozone/warm/`); steady-state spawns #2–20 averaged `43.6 ms` (p95 `46.8 ms`). Across all 20 workspaces, workspace-only backing storage (`extra_physical_bytes_ws_only`) was **`720.0 KiB`** (~36 KiB/workspace); the remaining `~519 MiB` is the single one-time immutable CAS baseline copy required on `ext4` without reflink or FUSE3.

### Suite 2 — 250,000-File Monorepo `status` Latency

Dirty edit set: `5` modified + `2` untracked files; `7` warm runs per case; 100% clean and dirty parity verified between `git` and `oz`.

| Command / Context | Fast Path | Clean Cold | Clean Warm p50 | Clean Warm p95 | Dirty Warm p50 | Dirty Warm p95 |
| :--- | :---: | ---: | ---: | ---: | ---: | ---: |
| `git status --porcelain` | index | `129.2 ms` | `137.5 ms` | `139.7 ms` | `139.4 ms` | `150.1 ms` |
| `oz status --json` (regular repo) | `false` (parallel walk) | **`108.1 ms`** | **`112.4 ms`** | **`115.7 ms`** | **`117.0 ms`** | **`121.7 ms`** |
| `oz status --json` (`oz ws`, `cas_hardlink`) | `false` | `238.6 ms` | `219.8 ms` | `223.4 ms` | `226.5 ms` | `233.1 ms` |

### Suite 3 — Agent VCBench (4 Canonical Non-Interactive Tasks)

Strict non-interactive execution (`GIT_TERMINAL_PROMPT=0`, `GIT_EDITOR=true`, `OZ_AGENT=1`) with bit-for-bit Git tree OID, working-tree, `Change-Id:`, and `git fsck --full` parity verification.

| Task | `git` cmds | `oz` cmds | Cmd Δ | `git` tok~ | `oz` tok~ | Tok~ Δ | `git` lex | `oz` lex | Lex Δ | `git` ms | `oz` ms |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1. Selective Hunk Commits | 7 | 2 | **-71.4%** | 794 | 119 | **-85.0%** | 876 | 154 | **-82.4%** | 11.4 | 18.9 |
| 2. Multi-Amends (3x) | 17 | 6 | **-64.7%** | 1,442 | 334 | **-76.8%** | 1,683 | 418 | **-75.2%** | 27.8 | 51.8 |
| 3. Split Commits (3-way) | 14 | 4 | **-71.4%** | 1,084 | 237 | **-78.1%** | 1,198 | 269 | **-77.5%** | 23.5 | 43.6 |
| 4. Conflict Resolution | 12 | 4 | **-66.7%** | 937 | 473 | **-49.5%** | 1,028 | 608 | **-40.9%** | 23.0 | 22.0 |
| **AGGREGATE (All 4 Tasks)** | **50** | **16** | **-68.0%** | **4,257** | **1,163** | **-72.7%** | **4,785** | **1,449** | **-69.7%** | **85.8** | **136.3** |

### Suite 4 — 10-Agent Concurrent Commit Contention (3 Rounds = 30 Attempts)

| Scenario | Tool | Succeeded | Lock Errors | Error Rate | Wall Time | Throughput | `git fsck` |
| :--- | :--- | ---: | ---: | ---: | ---: | ---: | :---: |
| Distinct-branch commits + readers | `git` | 30/30 | 0 | 0.0% | `28.5 ms` | `1,051.8 c/s` | PASS |
| Distinct-branch commits + readers | `oz` | 30/30 | **0** | **0.0%** | `98.3 ms` | `305.0 c/s` | PASS |
| Distinct-branch commits + `pack-refs` | `git` | 30/30 | 0 | 0.0% | `23.7 ms` | `1,267.9 c/s` | PASS |
| Distinct-branch commits + `pack-refs` | `oz` | 30/30 | **0** | **0.0%** | `93.3 ms` | `321.5 c/s` | PASS |
| Shared-worktree `.git/index.lock` contention | `git add` | 10/30 | **20** | **66.7%** | — | — | — |
| Shared-worktree `.git/index.lock` contention | `oz status` (indexless) | **30/30** | **0** | **0.0%** | — | — | — |

---

## 4. Honest Assessment — Where Ozone Wins & Where C `git` Is Still Faster

### Where Ozone Clearly Wins
- **Warm Multi-Agent Workspaces:** Across 20 workspaces with a 512 MiB warm cache, `oz ws spawn` (`cas_hardlink`) was **5.6x faster** (`941.1 ms` vs `5.28 s`) and used **19.7x less disk** (`529.11 MiB` total / `720.0 KiB` workspace-only vs `10.16 GiB`) than `git worktree add` + cache copy.
- **250k-File Regular-Repo `status`:** In a standard 250,000-file `.git/` repository, `oz status --json` beat C `git status --porcelain` by **19–22%** (`112.4 ms` clean / `117.0 ms` dirty vs `137.5 ms` / `139.4 ms`) while emitting deterministic `[f1]`/`h1a` IDs.
- **Autonomous Agent Workflows (VCBench):** **-68.0% fewer tool calls** (`16` vs `50`) and **-72.7% lower token overhead** (`1,163` vs `4,257`) with bit-for-bit identical Git trees and clean `git fsck`.
- **Zero `.git/index.lock` Races:** Shared-worktree concurrent `git add` failed `66.7%` (`20/30`) on `.git/index.lock`; Ozone is completely stage-free (`0%` lock errors).

### Honest Trade-Offs & Where C `git` Remains Faster
- **Cold Worktrees Without Build Caches:** Raw `git worktree add` (skipping `target/` and `node_modules/`) finished in `245.4 ms` (`12.3 ms`/ws) vs `941.1 ms` (`47.0 ms`/ws) for warm `oz ws spawn`. If a repository has no build caches at all, raw `git worktree` is already very fast.
- **Tiny Distinct-Branch Commit Throughput & CLI Wall Time:** C `git` achieves higher raw commit throughput on tiny 1-file distinct-branch commits (`1,051.8–1,267.9 c/s` vs `305.0–321.5 c/s` for `oz`) and lower local wall-clock time on VCBench (`85.8 ms` vs `136.3 ms`) because `oz` journals every mutation to the MVCC OpLog and emits structured `oz.v1` JSON envelopes. For AI agents, saving 34 LLM round-trips dwarfs a 50 ms local CLI delta.
- **`cas_hardlink` Fallback Under `NoNewPrivs: 1` (`ext4`):** Without `FICLONE` or live FUSE3 mounts (blocked here by `NoNewPrivs: 1`), `cas_hardlink` incurs a single one-time copy of the warm baseline (`529.11 MiB` total extra) and falls back to a full 250k-file walk inside `oz ws` (`226.5 ms` vs `139.4 ms` `git`) because no FUSE3 `upper/` dirty-set or `.git/index` exists. Running `./benchmarks/run_all.sh --full` in a standard host shell (`NoNewPrivs: 0`) enables `linux_fuse3` (or `APFS clonefile` / `Btrfs`/`XFS` `FICLONE` / Windows `ReFS`/`ProjFS`) for `< 50 MiB` extra disk and `O(dirty_files)` workspace status.
