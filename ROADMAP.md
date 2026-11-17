# Ozone (`oz` / `git-ozone`) — Implementation Roadmap

> **Goal:** Deliver a cross-platform (macOS, Ubuntu/Linux, Windows), GitHub-compatible Rust Git client optimized for zero-bloat agent worktrees, lock-free speed, and deterministic agentic UX.
> **Constraint:** Milestone sequencing ensures Mac (`APFS`) and Ubuntu (`ext4` + FUSE3) achieve instant zero-byte CoW workspaces in Milestone 1, followed immediately by Windows `NTFS` ProjFS and multi-agent integration in Milestone 2.

---

## Milestone 0 — Workspace Scaffolding & Cross-OS CI Matrix

**Objective:** Establish clean crate boundaries, licensing, automated cross-OS test runners, and `gix` colocated repository discovery.

### Deliverables
* Initialize Cargo workspace (`crates/oz-cli`, `oz-core`, `oz-cow`, `oz-vfsd`, `oz-warm`, `oz-oplog`, `oz-integrate`, `oz-github`, `oz-agent`, `oz-cloud-api`) with `MIT OR Apache-2.0` dual licensing headers.
* Implement canonical JSON output envelope (`oz.v1`), strict non-TTY / `--agent` prompt suppression, and exit code taxonomy (`0` success, `1` conflict/check fail, `2` usage/guardrail error).
* Implement `gix` colocated repository discovery (`.git/` + `.git/ozone/` layout, `.ozone.toml` parser, and `git-ozone` alias resolution).
* Stand up GitHub Actions CI matrix running integration tests across:
  * `ubuntu-latest` (`ext4` with `/dev/fuse` + `fusermount3`)
  * `macos-latest` (`APFS`)
  * `windows-latest` (`NTFS` + `ReFS` / Dev Drive)

### Acceptance Criteria
* `oz --version --json` and `git ozone doctor --json` build and pass CI cleanly across Linux, macOS, and Windows without C/OpenSSL toolchain dependencies.

---

## Milestone 1 — Core CoW Workspaces, Ubuntu `FUSE3` VFS, Warm Caches & Stage-Free Commits (`v0.1 Alpha`)

**Objective:** Deliver the killer single-machine demo: spawn 20 isolated agent workspaces with warm `target/` / `node_modules/` / `.venv/` caches in under a second with near-zero extra disk usage on macOS and Ubuntu `ext4`, plus stage-free hunk commits and instant `oz undo`.

### Deliverables
* **Tiered `WorkspaceBackend` (`oz-cow` + `oz-vfsd`):**
  * **macOS (`APFS`):** Native `clonefile(2)` backend.
  * **Linux (`Btrfs` / `XFS` / `bcachefs`):** Native `FICLONE` `ioctl` backend.
  * **Windows (`Dev Drive / ReFS`):** Native `FSCTL_DUPLICATE_EXTENTS_TO_FILE` backend.
  * **Ubuntu / Linux (`ext4`):** Unprivileged **`oz-vfsd` FUSE3 CoW daemon** (`fuser` / `fuse3` with `writeback_cache`), persistent real-disk `.git/ozone/ws/<ws>/upper/`, auto-spawn via IPC socket, and `systemd --user` auto-remount via `oz setup`.
  * **Universal Fallback:** Content-addressed hardlink farm (strictly immutable `0444`/`0555` deps only) + parallel source materialization.
* **Workspace Lifecycle (`oz ws`):**
  * Grouped sibling layout `../<repo>.oz/<ws>` with real branch checkout (`HEAD -> refs/heads/<branch>`).
  * `oz ws spawn`, `oz ws list`, `oz ws rm`, `oz ws gc`.
* **Warm Cache Engine (`oz-warm`):**
  * Zero-config auto-detection for Rust (`target/`), Node/TS (`node_modules/`, `.next/`, `.turbo/`), and Python (`.venv/` trampoline rewrites + caches).
  * Quiescent baseline capture (`.git/ozone/baselines/<commit>`) with lock-file check, LRU disk-cap GC, and **deterministic `mtime` preservation** for untouched baseline files.
* **Stage-Free Working-Tree-First VCS (`oz-core` + `oz-oplog`):**
  * Indexless tree-delta worktree state tracking for `O(dirty_files)` `oz status` and `oz diff` (via `gix-imara-diff`) emitting deterministic `[f1]` file IDs and `h1a` hunk IDs.
  * `oz commit -m "..."` (with secret / conflict-marker / `>50 MB` binary guardrails), one-shot `--pick` / `--exclude` partial commit synthesis via `gix` tree builders, and automatic `Change-Id` trailers.
  * Append-only MVCC OpLog (`.git/ozone/oplog/`), lock-free ref transactions, manual/hook `oz checkpoint`, and `< 10 ms` `oz undo` / `oz redo`.

### Exit Gate & Benchmarks
* **Space & Speed Gate:** Spawning 10 worktrees of a Rust + Node repository with a 5 GB `target/` + `node_modules/` takes **< 500 ms per workspace** and consumes **< 50 MB total additional disk space** on both macOS `APFS` and Ubuntu `ext4`.
* **Reliability Gate:** Killing `oz-vfsd` with `SIGKILL` mid-edit on Ubuntu `ext4`, re-running `oz status`, transparently remounts with **zero lost edits**.

---

## Milestone 2 — Windows `ProjFS`, In-Memory Multi-Agent Integration & Collision Radar (`v0.2 Beta`)

**Objective:** Achieve full zero-byte CoW parity on standard Windows `NTFS`, add live multi-agent collision prevention and atomic in-memory merging, and ship the `oz git` compatibility shim.

### Deliverables
* **Windows `NTFS` Virtualization (`oz-vfsd`):**
  * Implement `ProjFsVfsBackend` using `windows-projfs` backed by the same persistent `.git/ozone/ws/<ws>/upper/` model and one-time non-reboot `oz setup` (`Enable-WindowsOptionalFeature Client-ProjFS`).
* **Guardrails & Collision Radar (`oz-integrate`):**
  * `--scope` and `--deny` path enforcement during `oz ws spawn`, `oz status`, `oz commit`, and `oz integrate`.
  * Live `oz radar [--json]` scanning active `upperdir` / dirty sets and diff hunk ranges across all open workspaces.
* **Atomic In-Memory Merge & Ephemeral CoW Sandbox:**
  * `oz integrate` and `oz stack rebase` executed 100% in memory via `gix-merge`.
  * Conflict materialization exclusively inside ephemeral `../<repo>.oz/_integrate/` CoW sandbox with structured JSON conflict IDs (`c1`, `c2`), `oz resolve <id> --take ours|theirs`, and `--verify "<test-cmd>"` gating before advancing target branches.
* **Compatibility Shim (`oz git`):**
  * Intercepts `worktree`, `status`, `diff`, `checkout`, `switch`, `commit`, `add` (virtual `pinned` set), `merge`, `rebase`, `clone`, `fetch`, `push`.
  * Delegates remaining legacy commands to system `git` wrapped in pre/post OpLog snapshots so `oz undo` reverses legacy operations safely.

### Exit Gate & Benchmarks
* **Windows Gate:** Spawning 10 worktrees of a 5 GB repo on standard Windows 11 `NTFS` takes **< 500 ms/workspace** with **< 50 MB additional disk**.
* **Concurrency Stress Test:** 10 concurrent worker processes executing simultaneous `oz commit` operations on distinct branches achieve **0% lock errors** (compared to `.git/index.lock` / `packed-refs.lock` failures under C `git`).

---

## Milestone 3 — Native GitHub Stacked PRs, CI Self-Healing & Agent Ecosystem (`v0.3 RC`)

**Objective:** Close the loop from local multi-agent edits to merged GitHub PRs and full AI provenance attribution.

### Deliverables
* **Pure-Rust Wire Protocol v2 & SSH Signing (`oz-github`):**
  * `oz clone` (with `--filter=blob:none` partial clone support), `oz fetch`, and `oz push` (`--atomic`, `--force-with-lease`) over HTTPS & SSH using `gix-protocol` / `gix-transport`.
  * Native pure-Rust Ed25519 SSH commit signing (`ssh-key`) producing GitHub "Verified" signatures without GPG agent dependencies.
* **GitHub Stacked PRs & CI Loops:**
  * Token discovery reusing `gh` CLI credentials / OS keychains / `GITHUB_TOKEN`.
  * `oz pr create` and `oz pr stack` (auto-base management and PR stack cross-links).
  * `oz sync` (fetch `origin`, fast-forward `main`, auto-retarget downstream PR bases, auto-delete & `gc` merged CoW workspaces, refresh warm baseline).
  * `oz ci --json` (fetch failing GitHub Actions run logs and compact error traces for agent self-healing).
* **Agent Ecosystem & Provenance (`oz-agent`):**
  * `oz agent setup` configuring `SKILL.md` / rules and `PostToolUse` `oz checkpoint` hooks for Claude Code, Cursor, Codex CLI, Windsurf, and Antigravity.
  * Built-in `oz mcp` stdio server exposing workspace, hunk-commit, radar, undo, and CI tools.
  * Automatic `refs/notes/ai` recording and `oz blame` reporting human vs. agent line attribution while keeping GitHub commit messages clean.

### Exit Gate & Benchmarks
* **VCBench-Style Agent Eval:** Coding agents using `oz` complete selective commits, multi-amends, split commits, and conflict resolution in **>= 60% fewer commands and >= 50% lower token overhead** than baseline C `git`.

---

## Milestone 4 — `v1.0` Public Launch, Benchmarks & Packaging

**Objective:** Harden edge cases, publish reproducible benchmarks, and ship single-command installers across all three OSes.

### Deliverables
* **Reproducible Public Benchmark Suite:** Automated scripts comparing `git` vs `worktrunk` vs `simgit` vs `jj` vs `oz` on:
  * 20-agent worktree spawn time & total disk footprint on macOS (`APFS`), Ubuntu (`ext4`), and Windows (`NTFS` & `Dev Drive`).
  * `status` latency on a 250k-file monorepo.
  * Agent token/step count on multi-commit & conflict scenarios.
* **Distribution & Packaging:**
  * Signed static release binaries distributed via Homebrew (`brew install ozone-vcs/tap/oz`), Debian/Ubuntu APT & `.deb`, Windows `winget`, `cargo-binstall`, and shell/PowerShell installers.
  * Built-in `oz doctor` diagnostics (checking `/dev/fuse`, `fusermount3`, `Client-ProjFS`, Dev Drive detection, SSH signing keys, and GitHub auth).
* **Documentation & Launch Site:** Quickstart for solo devs + parallel AI agent setups, migration guide from `git worktree`, and complete JSON schema reference.

---

## Milestone 5 — Ozone Cloud Commercial Tiers (Post-`v1.0`)

**Objective:** Layer monetizable team & enterprise capabilities cleanly onto the `oz-cloud-api` trait seams without gating any local OSS feature.

### Deliverables
* **Ozone Pro / Team (SaaS):**
  * `RemoteSnapshotStore`: CI publishes compressed, deduplicated warm baseline + `target/`/`node_modules/` chunks; `oz clone --warm` streams pre-warmed workspaces to teammates and ephemeral cloud sandboxes in seconds.
  * `RadarSyncClient`: Real-time multiplayer collision radar alerting developers when another teammate's local agent edits overlapping files/APIs *before* PRs conflict.
* **Ozone Enterprise (SaaS / Self-Hosted):**
  * `AuditSink` & `PolicyProvider`: Org-wide `refs/notes/ai` provenance explorer, enforced agent path policies (`--deny` rules), SAML/SSO, and SOC2 audit log exports.
