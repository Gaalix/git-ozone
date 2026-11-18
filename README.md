# Ozone (`oz` / `git-ozone`)

**A pure-Rust, GitHub-compatible Git client and Copy-on-Write workspace engine built for solo developers and parallel AI coding agents.**

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Pure Rust](https://img.shields.io/badge/pure--rust-zero%20C%20bindings-orange.svg)](#tiered-cross-platform-cow-architecture)
[![Version: v1.0.0](https://img.shields.io/badge/version-v1.0.0-brightgreen.svg)](#installation)

Ozone operates **100% colocated inside standard `.git/` repositories** — no repository conversion, no lock-in, and full compatibility with `git`, GitHub, IDEs, `gh`, and CI. It eliminates the four failure modes of canonical C `git` under heavy parallel agent workloads:

1. **Zero-Byte CoW Workspaces with Warm Caches** — Spawn 20 isolated branch workspaces in `../<repo>.oz/<ws>` on macOS (`APFS clonefile`), Linux (`Btrfs`/`XFS`/`bcachefs` `FICLONE` or unprivileged `ext4` FUSE3 via `oz-vfsd`), and Windows (`Dev Drive / ReFS` or `NTFS ProjFS`) with warm `target/`, `node_modules/`, and `.venv/` caches and deterministic `mtime` preservation.
2. **Lock-Free MVCC OpLog & `< 10 ms` Undo** — Concurrent commits across parallel workspaces never collide on `.git/index.lock` or `.git/packed-refs.lock`. Every mutating command and `oz checkpoint` is recorded in an append-only OpLog so `oz undo` / `oz redo` rewind refs and working-tree edits in milliseconds.
3. **Stage-Free Working-Tree-First Commits** — Commit directly from the working directory with built-in secret, conflict-marker, `> 50 MB` binary, and `--scope`/`--deny` guardrails, automatic `Change-Id:` trailers, pure-Rust Ed25519 SSH signing, and one-shot `--pick f1 h2a` / `--exclude` partial commits using deterministic file (`f1`) and hunk (`h1a`) IDs.
4. **Multi-Agent Integration, GitHub Stacks & MCP** — Detect cross-workspace overlaps before merge with `oz radar`, merge atomically in memory with `oz integrate --verify`, manage GitHub stacked PRs with `oz pr stack` / `oz sync` / `oz ci --json`, wire 5 coding agent harnesses with `oz agent setup`, expose 8 structured tools via `oz mcp`, and audit AI vs. human line authorship with `refs/notes/ai` and `oz blame`.

---

## Installation

Release archives (`oz`, `git-ozone`, and `oz-vfsd`) are published as static pure-Rust binaries (zero `libgit2`, `OpenSSL`, or `libfuse` C dependencies) on [GitHub Releases](https://github.com/ozone-vcs/ozone/releases).

### macOS & Linux (Homebrew)

```bash
brew install ozone-vcs/tap/oz
```

### Linux & macOS (Shell Installer)

```bash
curl -fsSL https://raw.githubusercontent.com/ozone-vcs/ozone/main/packaging/install.sh | bash
```

Supports `--version v1.0.0`, `--prefix ~/.local`, and `--dry-run`.

### Debian / Ubuntu (`.deb` / APT)

```bash
# Download the latest release .deb (amd64 or arm64) and install with Recommends: fuse3
curl -fsSLO https://github.com/ozone-vcs/ozone/releases/download/v1.0.0/ozone_1.0.0_amd64.deb
sudo apt install ./ozone_1.0.0_amd64.deb
```

For self-hosted APT repository instructions, see [`packaging/deb/apt-repo-setup.md`](packaging/deb/apt-repo-setup.md).

### Windows (`winget`)

```powershell
winget install OzoneVCS.Ozone
```

### Windows (PowerShell Installer)

```powershell
irm https://raw.githubusercontent.com/ozone-vcs/ozone/main/packaging/install.ps1 | iex
```

Supports `-Version v1.0.0`, `-Prefix "$env:LOCALAPPDATA\Ozone"`, and `-DryRun`. To enable Windows `NTFS` virtualization (`Client-ProjFS`) without rebooting, run `oz setup --enable-projfs`.

### Cargo (`cargo-binstall` or Source)

```bash
# Pre-built release binaries via cargo-binstall
cargo binstall oz-cli

# Or build from source
cargo install --path crates/oz-cli --locked
cargo install --path crates/oz-vfsd --locked
```

### Verify Your Environment

```bash
oz --version
oz doctor
```

`oz doctor` (and `oz doctor --json`) inspects your OS, host filesystem (`statfs` / volume capabilities), `/dev/fuse` + `fusermount3` (Linux), `Client-ProjFS` / `ReFS` Dev Drive (Windows), `APFS clonefile` (macOS), `oz-vfsd`, `git-ozone` alias, SSH signing keys, and GitHub authentication.

---

## 60-Second Quickstart

### Solo Developer

Work directly in any existing Git repository — no migration step required:

```bash
# 1. Inspect indexless status with deterministic file ([f1]) and hunk (h1a) IDs
oz status

# 2. View Histogram unified diff (whole tree or filtered by file/hunk ID)
oz diff f1 h1a

# 3. Stage-free partial commit — zero `git add -p` or `git stash`
oz commit -m "feat(auth): add PKCE verifier" --pick f2 h1a

# 4. Instant < 10 ms rewind (and redo!)
oz undo
oz redo

# 5. Open or update a GitHub PR, inspect failing CI logs, and sync after merge
oz pr create
oz ci --json
oz sync
```

### Parallel AI Agents

Spawn isolated zero-byte CoW workspaces with warm caches and path guardrails, then integrate atomically:

```bash
# 1. Configure agent skills & PostToolUse `oz checkpoint --quiet` hooks
#    (auto-detects Claude Code, Cursor, Codex CLI, Windsurf, Antigravity)
oz agent setup

# 2. Spawn isolated workspaces with warm target/ / node_modules/ / .venv/
oz ws spawn agent-auth    --from main --scope "crates/auth/**"    --deny "Cargo.lock"
oz ws spawn agent-billing --from main --scope "crates/billing/**" --deny "migrations/**"

# 3. Check live cross-workspace hunk overlaps before PR time
oz radar --json

# 4. Atomically merge in memory and run tests inside ephemeral _integrate/ CoW sandbox
oz integrate agent-auth agent-billing --into main --verify "cargo test"

# 5. Audit human vs. AI line provenance stored cleanly in refs/notes/ai
oz blame crates/auth/src/token.rs
```

To expose Ozone directly to MCP-capable agents without shell-quoting overhead, configure `oz mcp` as a stdio MCP server (see [docs/quickstart.md](docs/quickstart.md#connecting-oz-mcp)).

---

## CLI Surface & Compatibility

Ozone follows the **`uv` Playbook**: a clean primary CLI (`oz`) plus drop-in Git compatibility (`git ozone` and `oz git`):

| Invocation | Purpose |
| :--- | :--- |
| `oz <cmd>` | Primary CLI — supports global `--json`, `--agent`, `-C <DIR>`, `-V/--version`. |
| `git ozone <cmd>` | Native Git subcommand alias provided by the `git-ozone` binary. |
| `oz git <cmd>` | Optional compatibility shim (`alias git="oz git"`). Intercepts `worktree`, `status`, `diff`, `add`, `reset`, `commit`, `switch`, `checkout`, `merge`, `rebase`, `clone`, `fetch`, `push` natively on Ozone and wraps delegated mutating `git` commands in OpLog transactions so even `git reset --hard` is reversible via `oz undo`. |

### Command Reference

| Command | Summary |
| :--- | :--- |
| `oz ws <spawn\|list\|rm\|gc>` | Manage isolated CoW/VFS workspaces (`../<repo>.oz/<ws>`) and warm cache LRU GC. |
| `oz status` | Indexless `O(dirty_files)` status emitting deterministic `[f1]` file and `h1a` hunk IDs. |
| `oz diff [TARGETS...]` | Histogram unified diffs filtered by file IDs (`f1`), hunk IDs (`h1a`), or paths. |
| `oz commit -m "..."` | Stage-free commit (`--pick`, `--exclude`, `--amend`, `--no-edit`, `-S/--sign`, guardrails, `Change-Id:`). |
| `oz checkpoint` | Capture `< 10 ms` MVCC working-tree checkpoint (`-q/--quiet` for agent hooks). |
| `oz undo [OP_ID \| --steps N]` / `oz redo` | Sub-10 ms OpLog rewind & re-apply for refs and working-tree edits. |
| `oz radar` | Detect overlapping hunks, shared-file edits, and out-of-scope edits across workspaces. |
| `oz integrate <SOURCES...>` | In-memory 3-way merge into `--into <branch>` with `--verify "<cmd>"` and `_integrate/` sandbox. |
| `oz resolve <ID \| --all> --take <ours\|theirs\|base>` | Resolve structured conflict IDs (`c1`, `c2`) with optional `--continue`. |
| `oz stack <status\|rebase>` | Inspect and in-memory rebase stacked branches while preserving `Change-Id:` trailers. |
| `oz clone` / `oz fetch` / `oz push` | Pure-Rust Git Wire Protocol v2 (`--filter=blob:none`, `--atomic`, `--force-with-lease`). |
| `oz pr <create\|stack>` | Create/update single or cross-linked stacked GitHub Pull Requests. |
| `oz sync` | Fetch, fast-forward base, retarget stacked PRs, rebase downstream, GC merged workspaces, refresh warm baseline. |
| `oz ci` | Inspect GitHub Check Runs & compact failing Actions job log excerpts (`--json` for agent self-healing). |
| `oz agent setup` | Install idempotent skills/rules and `oz checkpoint --quiet` hooks for 5 agent harnesses. |
| `oz mcp` | Launch built-in JSON-RPC 2.0 stdio Model Context Protocol server (8 tools). |
| `oz blame <PATH>` | Line-level human vs. AI (`refs/notes/ai`) authorship attribution. |
| `oz setup` / `oz doctor` | Initialize `.git/ozone/` (`--systemd-user`, `--enable-projfs`) and run system diagnostics. |

---

## Tiered Cross-Platform CoW Architecture

Ozone automatically probes your host filesystem and selects the fastest zero-copy backend:

| Platform & Filesystem | Backend (`BackendKind`) | Untouched Disk Overhead | Daemon |
| :--- | :--- | :--- | :--- |
| **macOS (`APFS`)** | `apfs_clonefile` (`clonefile(2)`) | Metadata only (`0` data blocks) | None |
| **Linux (`Btrfs`, `XFS`, `bcachefs`)** | `linux_ficlone` (`ioctl(FICLONE)`) | Metadata only (`0` data blocks) | None |
| **Linux (`ext4` default)** | `linux_fuse3` (`oz-vfsd` `/dev/fuse` + persistent `upper/`) | `0` bytes for untouched files | Auto-managed user daemon |
| **Windows 11 (`Dev Drive / ReFS`)** | `windows_refs_clone` (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`) | Metadata only (`0` data blocks) | None |
| **Windows 11 (`NTFS`)** | `windows_proj_fs` (`oz-vfsd` `windows-projfs` + persistent `upper/`) | `0` bytes for untouched files | Auto-managed user provider |
| **Container / Universal Fallback** | `cas_hardlink` (immutable `0444`/`0555` deps hardlinked + source copy) | `0` extra blocks for deps | None |

All edits on FUSE3/ProjFS workspaces persist immediately on real host disk in `.git/ozone/ws/<ws>/upper/` — even a `SIGKILL` or power loss drops zero written bytes, and the next `oz` command transparently remounts.

---

## Measured v1.0.0 Release Benchmarks

Measured on **AMD Ryzen 9 7900X** (24 logical cores, `61.39 GiB` RAM), Ubuntu Linux `7.3.0-10-generic` (`ext4` NVMe), comparing release `oz 1.0.0` vs. `git 2.55.0` (commit `3a240f354701`). Because the release runner executes inside a `NoNewPrivs: 1` sandbox (blocking setuid `/usr/bin/fusermount3`), worktree and workspace-status suites ran on Ozone's zero-privilege `cas_hardlink` tier; reproduce the full 5 GB FUSE3 / native CoW run on a standard host shell (`NoNewPrivs: 0`) with `./benchmarks/run_all.sh --full`.

| Benchmark | C `git 2.55.0` | Ozone `oz 1.0.0` (`cas_hardlink`) | Result |
| :--- | :---: | :---: | :--- |
| **20-Agent Warm Worktree Spawn** (`512 MiB` cache) | `5.28 s` (`261.8 ms` p50/ws) | **`941.1 ms` (`43.5 ms` p50/ws)** | **5.6x faster** *(raw cold `git worktree`: `245.4 ms`)* |
| **20-Agent Extra Disk Footprint** (`512 MiB` cache) | `10.16 GiB` | **`529.11 MiB` total** (`720.0 KiB` ws-only) | **19.7x less disk** *(1x `ext4` baseline copy; `< 50 MiB` on FUSE3/CoW)* |
| **250k-File Monorepo `status`** (regular repo, dirty p50) | `139.4 ms` *(clean `137.5 ms`)* | **`117.0 ms`** *(clean **`112.4 ms`**)* | **1.19x–1.22x faster** *(`cas_hardlink` ws walk: `226.5 ms`; FUSE3 hits `O(dirty)`)* |
| **Agent VCBench** (4 canonical tasks, 100% tree parity) | `50` cmds · `4,257` tok~ | **`16` cmds · `1,163` tok~** | **-68.0% commands**, **-72.7% tokens** (`-69.7%` lexical) |
| **10-Agent Concurrent Commits** (distinct / shared `.git/index`) | `0%` distinct / **`66.7%` (`20/30`)** shared | **`0.0%` lock errors (`0/30`)** | **Zero `.git/index.lock` errors** (`git fsck` PASS) |

*Honest trade-offs:* If your repo has no build caches at all, raw cold `git worktree add` (`245.4 ms` / `12.3 ms` per ws) is faster than warm `oz ws spawn` (`941.1 ms` incl. baseline capture), and C `git` has higher raw throughput on tiny distinct-branch commits (`1,051.8 c/s` vs `305.0 c/s` due to MVCC OpLog journaling). See **[docs/benchmarks.md](docs/benchmarks.md)** for full per-suite tables, methodology, and reproduction commands.

---

## Documentation & Resources

* **[Quickstart Guide](docs/quickstart.md)** — Solo developer workflow, `.ozone.toml` configuration, parallel AI agent setups (`oz agent setup`, `oz mcp`), and provenance (`refs/notes/ai`).
* **[Migrating from `git worktree`](docs/migration-from-git-worktree.md)** — Command mapping table, colocated `.git/` safety, `oz git` compatibility shim, and zero-lock-in uninstall steps.
* **[JSON Schema Reference (`oz.v1`)](docs/json-schema-reference.md)** & **[JSON Schema File](docs/schemas/oz.v1.envelope.schema.json)** — Canonical envelope contract, exit codes (`0`/`1`/`2`), deterministic IDs (`f1`, `h1a`, `c1`), command payload schemas, and `oz mcp` tool definitions.
* **[Benchmarks & Methodology](docs/benchmarks.md)** & **[Benchmark Suite](benchmarks/README.md)** — Reproducible scripts comparing `git` vs `oz` (and `jj` / `worktrunk` / `simgit` when installed) on 20-agent spawn time/disk, 250k-file `status` latency, VCBench agent token reduction, and concurrent commit lock contention.
* **[Architecture Specification](ARCHITECTURE.md)** — Deep dive into crate boundaries, MVCC OpLog, warm cache `mtime` preservation, and `oz-cloud-api` open-core seams.
* **[Implementation Roadmap](ROADMAP.md)** — Milestones 0–5 from scaffolding to v1.0 launch and Ozone Cloud.
* **[Launch Site](site/index.html)** — Zero-dependency static site with interactive installer tabs and `git` vs `oz` workflow comparisons.

---

## License

Dual-licensed under either of:

* **MIT License** ([LICENSE-MIT](LICENSE-MIT))
* **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
