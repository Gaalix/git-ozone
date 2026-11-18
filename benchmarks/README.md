# Ozone Reproducible Public Benchmark Suite

This directory contains the self-contained, zero-dependency (Python 3 stdlib + POSIX bash) public benchmark suite for **Ozone (`oz` / `git-ozone`)**. It is designed to answer one concrete question with honest, reproducible numbers:

> **"Was Ozone worth it compared to `git` (and `jj`, `worktrunk`, `simgit` when installed)?"**

Every script executes real compiled binaries on real filesystems, verifies correctness and Git interoperability (`git rev-parse`, exact working-tree equality, `git fsck --full`), cleans up FUSE3 mounts even on errors or `Ctrl-C`, and **never** mocks or fabricates numbers for tools not installed on `PATH`.

---

## Benchmarks Included

| Suite | Script | What It Measures | ROADMAP Target |
| :--- | :--- | :--- | :--- |
| **1. Multi-Agent Worktrees** | `bench_worktrees.py` | Spawning 20 isolated workspaces on a realistic Rust + Node + Python repo with warm caches (`target/`, `node_modules/`, `.venv/`): wall-clock latency (total, mean, p50, p95, spawn #1 vs steady-state) & physical disk footprint (`st_blocks` + `statvfs Δ`) + edit isolation. Compares `git worktree` (both raw/cold AND with warm cache copy), `oz ws spawn`, and optional `jj`/`worktrunk`/`simgit`. | `< 500 ms` / workspace & `< 50 MiB` total extra disk |
| **2. 250k-File Monorepo `status`** | `bench_status_250k.py` | `status` latency (clean cold/warm p50/p95 & dirty warm p50/p95) on a real 250,000-file Git monorepo across `git status --porcelain`, `oz status --json` (regular repo), `oz status --json` (`oz ws` CoW/FUSE3 workspace), and optional `jj status`. Verifies exact dirty/untracked file parity against `git`. | `O(dirty_files)` fast path inside `oz ws` + 100% `git` parity |
| **3. Agent VCBench (Tokens & Steps)** | `bench_agent_vcbench.py` | Deterministic non-interactive agent execution on 4 canonical workflows (Selective Hunk Commits, 3x Multi-Amends preserving `Change-Id:`, 3-way Split Commits, Multi-Branch Conflict Resolution). Meters commands, UTF-8 bytes, `ceil(bytes/4)` approx LLM tokens, lexical tokens, wall-clock time, bit-for-bit Git tree equality, and `git fsck --full`. | `>= 60%` fewer commands & `>= 50%` fewer tokens vs `git` |
| **4. Concurrent Commit Contention** | `bench_concurrency.py` | 10-agent barrier-synchronized concurrent commits on distinct branches (with concurrent readers and under `git pack-refs --all` pressure), plus shared-worktree `.git/index.lock` contention vs stage-free `oz`. | `0%` lock errors + 100% branch ref & `git fsck` integrity |

---

## Prerequisites

- **Ozone release binaries** (`oz` and `oz-vfsd`):
  ```bash
  cargo build --release --locked --bin oz --bin oz-vfsd
  ```
  Scripts automatically discover `./target/release/oz` (and sibling `oz-vfsd`), or you can pass `--oz-bin /path/to/oz` (or `OZ_BIN=...` / `OZ_VFSD_BIN=...`) to benchmark an installed release binary.
- **Git** (`git` on `PATH`)
- **Python 3.9+** (standard library only — no `pip install` needed)
- **OS virtualization support (for zero-byte CoW workspaces):**
  - **Ubuntu / Linux (`ext4`):** `/dev/fuse` read/write access and `fusermount3` (`sudo apt-get install fuse3`). Linux `Btrfs`/`XFS`/`bcachefs` automatically use native `FICLONE` reflinks.
  - **macOS (`APFS`):** Native `clonefile(2)` — works out of the box.
  - **Windows:** `Client-ProjFS` (`oz setup --enable-projfs`) on `NTFS`, or native reflinks on **Dev Drive (`ReFS`)**.

---

## Optional Comparison Tools

If any of the following tools are installed on `PATH`, the suite benchmarks them automatically and records their versions. If not installed, the suite records `"available": false` and displays `Not installed` in `REPORT.md`.

- **Jujutsu (`jj`):**
  - macOS / Linux (Homebrew): `brew install jj`
  - Cargo: `cargo install --locked jj-cli`
  - Windows (`winget`): `winget install jj-vcs.jj`
- **Worktrunk (`wt` / `worktrunk`):**
  - Cargo / Homebrew (per upstream instructions): `cargo install worktrunk` or `brew install worktrunk`
  - Note: on Windows, the benchmark verifies `wt --version` so Windows Terminal (`wt.exe`) is never misidentified as Worktrunk.
- **simgit (`simgit`):**
  - Install on `PATH` per upstream repository instructions.

---

## Running the Benchmarks

### 1. Quick Smoke Test (~15–30 seconds)
Validates the full pipeline, FUSE3 mount/teardown, JSON schema, and Markdown report generation at reduced scale (`5` agents, `50 MiB` warm cache, `5,000` files):

```bash
./benchmarks/run_all.sh --quick
```

### 2. Standard Reproducible Run (~2–5 minutes)
Runs 20 agents with a `512 MiB` warm cache, `250,000`-file monorepo status, all 4 VCBench tasks, and 10-agent concurrency:

```bash
./benchmarks/run_all.sh
```

### 3. Full v1.0 ROADMAP Release Gate (5 GB Warm Cache + 250k Files)
Runs the exact `ROADMAP.md` v1.0 scale (`20` agents, `5,000 MiB` warm cache across 4,000 files, `250,000`-file monorepo, VCBench, and 10-agent concurrency).
> **Disk space note:** Including `git worktree add` + full warm cache copy for 20 agents requires `~115 GiB` of free disk space on the benchmark volume. If you have less free space, the script automatically detects available disk space and safely skips only the `git_worktree_warm_copy` step (or pass `--skip-git-warm-copy`).

```bash
./benchmarks/run_all.sh --full
```

### 4. Running Individual Suites
Each Python script is standalone and outputs JSON to stdout (or `--output <file>`):

```bash
# Suite 1: 20-agent worktree spawn & disk footprint
python3 benchmarks/bench_worktrees.py --agents 20 --cache-mb 5000 --output benchmarks/results/worktrees.json

# Suite 2: 250k-file monorepo status latency
python3 benchmarks/bench_status_250k.py --files 250000 --runs 7 --output benchmarks/results/status.json

# Suite 3: Agent VCBench (add --verify-cargo-gate to cross-check Rust m3_exit_gates)
python3 benchmarks/bench_agent_vcbench.py --output benchmarks/results/vcbench.json

# Suite 4: 10-agent concurrent commit contention
python3 benchmarks/bench_concurrency.py --agents 10 --rounds 3 --output benchmarks/results/concurrency.json

# Merge and render Markdown report
python3 benchmarks/generate_report.py \
  --worktrees benchmarks/results/worktrees.json \
  --status benchmarks/results/status.json \
  --vcbench benchmarks/results/vcbench.json \
  --concurrency benchmarks/results/concurrency.json \
  --merge-out benchmarks/results/latest.json \
  --report-out benchmarks/results/REPORT.md
```

---

## Methodology & Fairness Notes

### Real-Disk Sandbox Placement (`--work-dir`) & AppArmor / `NoNewPrivs`
- On Linux:
  - `/tmp` is frequently mounted as `tmpfs` (RAM-backed), which distorts disk I/O benchmarks and runs out of space on multi-gigabyte worktree tests.
  - `/var/tmp` is denied by Ubuntu's default AppArmor `/etc/apparmor.d/fusermount3` profile (which allows `@{HOME}/**/` and `/tmp/**/`, not `/var/tmp`).
  - Therefore, `bench_common.py` defaults to `$HOME/.cache/oz-bench` on Linux (when short and on a real persistent filesystem) — satisfying all three requirements: real disk (`ext4`/`btrfs`/`xfs`), AppArmor `fusermount3` allowlist, and short Unix socket path (`< 60` bytes so `.git/ozone/vfsd.sock` stays well under `SUN_LEN` 108 bytes).
- In restricted containers/sandboxes that set `PR_SET_NO_NEW_PRIVS` (`NoNewPrivs: 1` in `/proc/self/status`), setuid `fusermount3` cannot elevate privileges; run the suite in a standard user shell to benchmark FUSE3, or pass `--oz-backend cas_hardlink` to explicitly benchmark the universal CAS hardlink fallback backend.
- Pass `--work-dir /path/on/target/volume` to benchmark a specific volume (for example, a Windows **Dev Drive** `D:\bench` vs standard `C:\bench` NTFS, or a Linux `btrfs` mount).

### Fair Physical Disk Footprint Measurement
Measuring CoW/VFS disk usage naively with `du` across mountpoints is misleading:
- On **Linux `ext4` + FUSE3 (`oz-vfsd`)**, a FUSE mountpoint exposes lower-layer files virtually; walking the mountpoint counts lower-layer bytes `N` times even though zero bytes were duplicated. We measure:
  1. **Backing physical blocks (`st_blocks * 512`)** across `.git/ozone/{ws,baselines,state,oplog}` and `.git/worktrees`, deduplicated by `(st_dev, st_ino)` and excluding original base-repository inodes (which `oz-warm` safely hardlinks for immutable build artifacts).
  2. **Filesystem-level allocated delta (`os.statvfs` / `disk_usage` after `sync()`)** before vs. after spawning all `N` workspaces — which also captures native block sharing on **APFS (`clonefile`)**, **Btrfs/XFS (`FICLONE`)**, and **ReFS**.
  3. **Apparent logical size** visible across all `N` workspaces so the space amplification factor is crystal clear.
- For `git worktree`, we benchmark **both**:
  - `git worktree add` (**raw / cold cache**): fast and small on disk, but every agent starts with a cold `target/` and `node_modules/` and must recompile/reinstall.
  - `git worktree add` (**+ warm cache copy**, using `--reflink=never` on ext4 / standard recursive copy): true warm-cache parity with `oz ws spawn`.

### Fair Monorepo `status` Measurement
- We benchmark `oz status --json` in **both** contexts against `git status --porcelain`:
  1. Inside a **regular Git repo** (no FUSE3 overlay; full working-tree walk).
  2. Inside an **`oz ws` CoW/FUSE3 workspace** (`O(dirty_files)` fast path scanning `.git/ozone/ws/<ws>/upper/`).
- Showing both side-by-side makes clear that Ozone's `O(dirty_files)` status advantage applies inside `oz ws` workspaces, while being completely transparent about regular-repo directory-walk performance.

### Deterministic Non-Interactive VCBench Metering
- Every `git` and `oz` subprocess runs with `GIT_TERMINAL_PROMPT=0`, `GIT_EDITOR=true`, `GIT_PAGER=cat`, and `OZ_AGENT=1`.
- Token overhead is metered over the complete agent I/O transcript (`command + stdin + stdout + stderr`) using **two** independent estimators:
  1. Standard LLM byte approximation: `ceil(utf8_bytes / 4)`
  2. Deterministic lexical token count (alphanumeric/underscore identifier runs + individual punctuation/operator symbols, matching `crates/oz-cli/tests/m3_exit_gates.rs`).
- Every task asserts bit-for-bit identical Git tree OIDs (`HEAD^{tree}` / `HEAD~k^{tree}` / `main^{tree}`), bit-for-bit identical remaining working trees, preserved `Change-Id:` trailers, and clean `git fsck --full`.

### Clean FUSE3 Teardown
Every benchmark uses a `Sandbox` context manager registered with `atexit`, `SIGINT`, and `SIGTERM` handlers. On normal completion or failure, it runs `oz ws rm --force`, terminates any `oz-vfsd` processes bound specifically to that sandbox root, unmounts any remaining FUSE3 mounts via `fusermount3 -u` (fallback `-u -z`), and deletes the temporary sandbox directory cleanly.
