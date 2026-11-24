# Ozone (`oz` / `git-ozone`)

**A pure-Rust, GitHub-compatible Git client and Copy-on-Write workspace engine built for solo developers and parallel AI coding agents.**

[![CI](https://github.com/Gaalix/git-ozone/actions/workflows/ci.yml/badge.svg)](https://github.com/Gaalix/git-ozone/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Gaalix/git-ozone)](https://github.com/Gaalix/git-ozone/releases)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Pure Rust](https://img.shields.io/badge/pure--rust-zero%20C%20bindings-orange.svg)](#tiered-cross-platform-cow-architecture)
[![Website](https://img.shields.io/badge/site-gaalix.github.io%2Fgit--ozone-brightgreen.svg)](https://gaalix.github.io/git-ozone/)

Ozone operates **100% colocated inside standard `.git/` repositories** — no repository conversion, no lock-in, and full compatibility with `git`, GitHub, IDEs, `gh`, and CI. It eliminates the four failure modes of canonical C `git` under heavy parallel agent workloads:

1. **Zero-Byte CoW Workspaces with Warm Caches** — Spawn 20 isolated branch workspaces in `../<repo>.oz/<ws>` on macOS (`APFS clonefile`), Linux (`Btrfs`/`XFS`/`bcachefs` `FICLONE` or unprivileged `ext4` FUSE3 via `oz-vfsd`), and Windows (`Dev Drive / ReFS` or `NTFS ProjFS`) with warm `target/`, `node_modules/`, and `.venv/` caches and deterministic `mtime` preservation.
2. **Lock-Free MVCC OpLog & `< 10 ms` Undo** — Concurrent commits across parallel workspaces never collide on `.git/index.lock` or `.git/packed-refs.lock`. Every mutating command and `oz checkpoint` is recorded in an append-only OpLog so `oz undo` / `oz redo` rewind refs and working-tree edits in milliseconds.
3. **Stage-Free Working-Tree-First Commits** — Commit directly from the working directory with built-in secret, conflict-marker, `> 50 MB` binary, and `--scope`/`--deny` guardrails, automatic `Change-Id:` trailers, pure-Rust Ed25519 SSH signing, and one-shot `--pick f1 h2a` / `--exclude` partial commits using deterministic file (`f1`) and hunk (`h1a`) IDs.
4. **Multi-Agent Integration, GitHub Stacks & MCP** — Detect cross-workspace overlaps before merge with `oz radar`, merge atomically in memory with `oz integrate --verify`, manage GitHub stacked PRs with `oz pr stack` / `oz sync` / `oz ci --json`, wire 5 coding agent harnesses with `oz agent setup`, expose 11 structured tools via `oz mcp`, and audit AI vs. human line authorship with `refs/notes/ai` and `oz blame`.

---

## Installation

Release archives (`oz`, `git-ozone`, and `oz-vfsd`) are published as static pure-Rust binaries (zero `libgit2`, `OpenSSL`, or `libfuse` C dependencies) on [GitHub Releases](https://github.com/Gaalix/git-ozone/releases), or you can install straight from source with Cargo.

### Linux & macOS (Shell Installer)

```bash
curl -fsSL https://raw.githubusercontent.com/Gaalix/git-ozone/main/packaging/install.sh | bash
```

Supports `--version v1.1.0`, `--prefix ~/.local`, and `--dry-run`.

### Windows (PowerShell Installer)

```powershell
irm https://raw.githubusercontent.com/Gaalix/git-ozone/main/packaging/install.ps1 | iex
```

Supports `-Version v1.1.0`, `-Prefix "$env:LOCALAPPDATA\Ozone"`, and `-DryRun`. To enable Windows `NTFS` virtualization (`Client-ProjFS`) without rebooting, run `oz setup --enable-projfs`.

### Cargo (From GitHub, `cargo-binstall`, or Local Checkout)

```bash
# Install directly from GitHub source (works immediately with Rust 1.85+ / edition 2024)
cargo install --git https://github.com/Gaalix/git-ozone --locked oz-cli oz-vfsd

# Or download pre-built GitHub Release binaries via cargo-binstall
cargo binstall --git https://github.com/Gaalix/git-ozone oz-cli

# Or build from a local clone
cargo install --path crates/oz-cli --locked
cargo install --path crates/oz-vfsd --locked
```

### Debian / Ubuntu (`.deb` / APT)

```bash
# Download the latest release .deb (amd64 or arm64) and install with Recommends: fuse3
curl -fsSLO https://github.com/Gaalix/git-ozone/releases/download/v1.1.0/ozone_1.1.0_amd64.deb
sudo apt install ./ozone_1.1.0_amd64.deb
```

For self-hosted APT repository instructions, see [`packaging/deb/apt-repo-setup.md`](packaging/deb/apt-repo-setup.md).

### macOS & Linux (Homebrew Formula)

```bash
brew install --formula https://raw.githubusercontent.com/Gaalix/git-ozone/main/packaging/homebrew/oz.rb
```

### Windows (`winget`)

```powershell
winget install OzoneVCS.Ozone
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
| `oz <cmd>` | Primary CLI — supports global `--json`, `--agent`, `-C`/`--directory <DIR>`, `-V/--version`. |
| `git ozone <cmd>` | Native Git subcommand alias provided by the `git-ozone` binary. |
| `oz git <cmd>` | Optional compatibility shim (`alias git="oz git"`). Intercepts `worktree`, `status`, `diff`, `add`, `reset`, `commit`, `switch`, `checkout`, `merge`, `rebase`, `clone`, `fetch`, `push` natively on Ozone and wraps delegated mutating `git` commands in OpLog transactions so even `git reset --hard` is reversible via `oz undo`. |

### Command Reference

| Command | Summary |
| :--- | :--- |
| `oz ws <spawn\|list\|rm\|gc>` | Manage isolated CoW/VFS workspaces (`--from`, `--branch`, `--backend`, warm cache LRU GC). |
| `oz status [--hunks]` | Indexless `O(dirty_files)` status emitting deterministic `[f1]` file and collapsed/full `h1a` hunk IDs. |
| `oz diff [--base/--rev \| A..B] [--exclude PAT] [--line-numbers] [TARGETS...]` | Histogram unified diffs (working tree or revision ranges) with explicit `old_line`/`new_line`, `--exclude` / `:!<path>`, file/hunk IDs, or `--stat`. |
| `oz commit -m "..."` | Stage-free commit (`--pick`, `--exclude`, `--amend`, `--no-edit`, `-S/--sign`, `--no-verify`, hooks, guardrails, `Change-Id:`). |
| `oz log` / `oz show` / `oz rev-parse` | Commit graph (`--oneline`, `--author`, `--grep`, `-S`/`--pickaxe`, `-G`/`--grep-diff`, `--since`, `-- <paths...>`), commit & `<rev>:<path> --lines` blob inspection, and native `rev-parse`. |
| `oz stash` | `push` / `pop` / `list` / `drop` backed by MVCC OpLog snapshots so `oz undo` reverses stash operations too. |
| `oz cherry-pick` / `oz revert` | In-memory cherry-pick & revert with `--no-commit`, `--json`, and OpLog undo support. |
| `oz branch` / `oz switch` / `oz tag` | Branch list/create/rename/delete, safe branch switching (`-c`/`-d`/`-f`), and tag management with OpLog undo. |
| `oz restore` | Selectively restore working-tree paths or file IDs (`f1`) from `HEAD` or `--source` (recorded in OpLog for `oz undo`). |
| `oz checkpoint` | Capture `< 10 ms` MVCC working-tree checkpoint (`-q/--quiet` for agent hooks). |
| `oz undo [OP_ID \| --steps N]` / `oz redo` | Sub-10 ms OpLog rewind & re-apply for refs and working-tree edits. |
| `oz radar` | Detect overlapping hunks, shared-file edits, and out-of-scope edits across workspaces. |
| `oz integrate <SOURCES...>` | In-memory 3-way merge into `--into <branch>` with `--verify "<cmd>"` and `_integrate/` sandbox. |
| `oz resolve <ID \| --all> --take <ours\|theirs\|base>` | Resolve structured conflict IDs (`c1`, `c2`) with optional `--continue`. |
| `oz stack <status\|rebase>` | Inspect and in-memory rebase stacked branches while preserving `Change-Id:` trailers. |
| `oz clone` / `oz fetch` / `oz pull` / `oz push` | Pure-Rust Git Wire Protocol v2 (`--filter=blob:none`, `--ff-only`, `--rebase`, `--atomic`, `--force-with-lease`, `--watch`, `--no-verify` `pre-push` hooks, `~/.ssh/config`). |
| `oz pr <view\|diff\|read\|review\|create\|stack>` | Inspect PRs, view line-numbered PR diffs, read files at PR head, submit pre-validated inline reviews, and create/update single or stacked PRs. |
| `oz sync` | Fetch, fast-forward base, retarget stacked PRs, rebase downstream, GC merged workspaces, refresh warm baseline. |
| `oz ci` | Inspect GitHub Check Runs, watch CI (`--watch --json`), and drill into failing Actions job logs (`--job <id>`). |
| `oz agent setup` | Install idempotent skills/rules and `oz checkpoint --quiet` hooks for 5 agent harnesses. |
| `oz mcp` | Launch built-in JSON-RPC 2.0 stdio Model Context Protocol server (11 tools). |
| `oz blame <PATH>` | Line-level human vs. AI (`refs/notes/ai`) authorship attribution. |
| `oz gc` / `oz fsck` | Bounded `.git/ozone/oplog/` compaction (`--oplog-days`), integrity checks, and safe `--repair` for interrupted transactions. |
| `oz completions <SHELL>` | Generate `bash`, `zsh`, `fish`, or `powershell` completions (`clap_complete`). |
| `oz setup` / `oz doctor` | Initialize `.git/ozone/` (`--systemd-user`, `--enable-projfs`) and run system diagnostics. |

Ozone also ships full **Day-3 Git compatibility** (`pre-commit`/`commit-msg`/`post-commit`/`pre-push` hooks & `core.hooksPath`, `.gitattributes` `text`/`eol`/`binary` & `git-lfs` filter safety, `160000` gitlink submodules, `includeIf` + `insteadOf` + `~/.ssh/config`), automatic TTY paging (`$OZ_PAGER` / `$GIT_PAGER` / `delta` / `less -FRX`, strictly disabled under `--json`, `--agent`, or non-TTY pipes), and configurable short command aliases (`st`, `ci`, `co`, `lg`, `br`) via `.ozone.toml`.

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

## Measured v1.0.0 Release Benchmarks (7/7 ROADMAP Gates PASS)

Measured via `./benchmarks/run_all.sh --full` on **AMD Ryzen 9 7900X** (24 logical cores, `61.39 GiB` RAM), Ubuntu Linux `7.3.0-10-generic` (`ext4` NVMe, live unprivileged `linux_fuse3` via `fusermount3 3.18.3`), comparing release `oz 1.0.0` vs. `git 2.55.0`, `jj 0.47.1`, and `worktrunk 0.83.0` (commit `4690cbee08b2`).

| Benchmark | C `git 2.55.0` | `jj 0.47.1` / `wt 0.83.0` | Ozone `oz 1.0.0` (`linux_fuse3`) | Result |
| :--- | :---: | :---: | :---: | :--- |
| **20-Agent Warm Worktree Spawn** (`5,000 MiB` cache) | `22.78 s` (`1.11 s` p50) *(cold: `243.9 ms`)* | `jj` `2.61 s` · `wt` `567.8 ms` *(cold)* | **`444.5 ms` (`19.8 ms` p50/ws)** | **51.2x faster** than warm `git`; faster than cold `jj` & `wt` |
| **20-Agent Extra Disk Footprint** (`5,000 MiB` cache) | `97.96 GiB` *(cold: `17.03 MiB`)* | `jj` `16.72 MiB` · `wt` `20.79 MiB` *(cold)* | **`1.84 MiB`** (`97.67 GiB` apparent) | **~54,404x less disk** than warm `git` (`9.3x` less than cold `git`) |
| **250k-File Monorepo `status`** (dirty warm p50) | `132.1 ms` *(clean `136.8 ms`)* | `jj` `69.6 ms` | **`4.4 ms`** (`oz ws`) · `118.3 ms` (regular) | **30.2x faster** than `git`, **15.8x faster** than `jj` |
| **Agent VCBench** (4 canonical tasks, 100% tree parity) | `50` cmds · `4,257` tok~ | — | **`16` cmds · `1,163` tok~** | **-68.0% commands**, **-72.7% tokens** (`-69.7%` lexical) |
| **10-Agent Concurrent Commits** (distinct / shared `.git/index`) | `0%` distinct / **`73.3%` (`22/30`)** shared | — | **`0.0%` lock errors (`0/30`)** | **Zero `.git/index.lock` errors** (`git fsck` PASS) |

*Honest trade-offs:* If your repo has no build caches at all, raw cold `git worktree add` (`243.9 ms` / `12.2 ms` per ws) is faster than warm `oz ws spawn` (`444.5 ms` incl. `79.7 ms` baseline capture), and C `git` has higher raw throughput on tiny 1-file distinct-branch commits (`1,131.2 c/s` vs `496.0 c/s` due to MVCC OpLog journaling). See **[docs/benchmarks.md](docs/benchmarks.md)** for full per-suite tables, methodology, and reproduction commands.

---

## Documentation & Resources

* **[Website & Interactive Demo](https://gaalix.github.io/git-ozone/)** — Interactive install commands, `git` vs `oz` comparisons, and live v1.0.0 benchmarks (`site/index.html`).
* **[Quickstart Guide](docs/quickstart.md)** — Solo developer workflow, `.ozone.toml` configuration, parallel AI agent setups (`oz agent setup`, `oz mcp`), and provenance (`refs/notes/ai`).
* **[Migrating from `git worktree`](docs/migration-from-git-worktree.md)** — Command mapping table, colocated `.git/` safety, `oz git` shim, and zero-lock-in uninstall steps.
* **[JSON Schema Reference (`oz.v1`)](docs/json-schema-reference.md)** & **[JSON Schema File](docs/schemas/oz.v1.envelope.schema.json)** — Canonical envelope contract, exit codes (`0`/`1`/`2`), deterministic IDs (`f1`, `h1a`, `c1`), command payload schemas, and `oz mcp` tool definitions.
* **[Benchmarks & Methodology](docs/benchmarks.md)** & **[Benchmark Suite](benchmarks/README.md)** — Reproducible scripts comparing `git` vs `oz` (and `jj` / `worktrunk` / `simgit` when installed) on 20-agent spawn time/disk, 250k-file `status` latency, VCBench agent token reduction, warm incremental builds, and concurrent commit lock contention.
* **[Architecture Specification](ARCHITECTURE.md)** — Deep dive into crate boundaries, MVCC OpLog, warm cache `mtime` preservation, and `oz-cloud-api` open-core seams.
* **[Implementation Roadmap](ROADMAP.md)** & **[Changelog](CHANGELOG.md)** — Milestones 0–5 and release history.
* **[Contributing](CONTRIBUTING.md)** · **[Security Policy](SECURITY.md)** · **[Code of Conduct](CODE_OF_CONDUCT.md)** — How to contribute, verify pure-Rust supply chain guarantees, and report security issues.

---

## License

Dual-licensed under either of:

* **MIT License** ([LICENSE-MIT](LICENSE-MIT))
* **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
