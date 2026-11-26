# Changelog

All notable changes to Ozone (`oz` / `git-ozone` / `oz-vfsd`) are documented in this file.

## [1.4.0] — 2026-11-26

### Security & Soundness Hardening
- **Host-Scoped GitHub Token Discovery & Cleartext HTTP Credential Guardrail (`oz-github`):** Scoped `OZ_GITHUB_TOKEN`, `GITHUB_TOKEN`, and `GH_TOKEN` environment token discovery strictly to `github.com`, loopback endpoints, and hosts explicitly configured via `GH_HOST` or `OZ_GITHUB_API_URL`, blocking automatic token discovery over non-loopback `http://`. Enforced fail-closed `OzError::GuardrailViolation` (`insecure_http_credentials`) across `wire::connect_transport` and `GitHubClient` whenever credentials would be sent over non-loopback cleartext `http://`.
- **Drop-Safe Poison Recovery & Zero Production `.expect()`/`.unwrap()` (`oz-core`, `oz-oplog`, `oz-cow`, `oz-warm`, `oz-github`, `oz-agent`, `oz-integrate`):** Replaced lock-poisoning `.expect()` calls in `WorkerFlush::drop` with `PoisonError::into_inner` recovery to prevent double-panic aborts during unwind, eliminated remaining production `.expect()`/`.unwrap()` calls across all 10 workspace crates, and upgraded lossy `OzError::Git` string conversions to causal `OzError::git_source` / `OzError::git_context` error chains.
- **Width-Safe Arithmetic & Baseline Budget Clamping (`oz-integrate`, `oz-warm`):** Replaced unchecked `isize`/`as u32` casts in `oz radar` (`project_pos_onto_base`) with clamped `i64` arithmetic and added overflow-safe upper-bound clamping for warm-cache GC byte budgets (`gc_baselines`).

### Features & Performance
- **`regex-automata` `oz grep` Engine & Parallel Working-Tree Scanning (`oz-core`):** Replaced the custom NFA in `SimpleRegex` with `regex-automata` (`meta::Regex` + `regex-syntax`), adding full Rust regex support including Unicode word boundaries (`\b`, `\B`), bounded quantifiers (`{m,n}`), and allocation-free case-insensitive fixed-string (`-F -i`) matching. Parallelized working-tree `oz grep` via `ignore::WalkBuilder::build_parallel()` with 8 KiB NUL-byte binary sniffing before full-file reads, allocation-free path separator normalization, and deterministic `(path, line_number)` ordering.
- **Lock-Free Rayon + `Cow` CAS Materialization (`oz-cow`):** Updated `link_file_via_cas` to borrow cached BLAKE3 digests via `Cow<'a, str>` (zero per-file `String` allocations on warm workspace spawns) and replaced mutex-contended digest collection in `materialize_cas_workspace` with lock-free Rayon parallel collection.
- **Lock-Split `McpServer`, Bounded OpLog Locks & Linear `fsck` (`oz-agent`, `oz-oplog`):** Split `McpServer` session state locking so `MutexGuard` is dropped before executing blocking `tools/call` handlers (allowing concurrent `tools/list` and fast tools while `oz_ci_logs` waits), refactored `IN_PROCESS_LOCKS` to `Weak<Mutex<()>>` with dead-entry pruning for bounded memory in long-lived MCP servers, and replaced quadratic `oz fsck` head-reachability filtering with a single `O(V + E)` reverse-children graph walk.

### Dependencies, Hygiene & Static Analysis
- **Lean Workspace Dependencies & `clap`-Free `oz-vfsd` (`oz-vfsd`, workspace):** Replaced `clap` in `oz-vfsd` with a lightweight `std::env::args_os()` parser, removed unused `bytes` from workspace dependencies, hoisted `regex-automata` and `regex-syntax` to `[workspace.dependencies]`, and trimmed base workspace `tokio` to `["rt"]` with exact per-crate feature sets.
- **Workspace-Wide Pedantic, Perf & Dependency Lints:** Narrowed crate-internal helpers to `pub(crate)`, removed dead internal methods, ignored `.hallmark/`, and made all 10 workspace crates warning-free under Rust 1.100 `-W clippy::pedantic -W clippy::perf`, `-W unused-crate-dependencies`, and `cargo::unused_workspace_dependencies`.

## [1.3.0] — 2026-11-26

### Features & CLI UX
- **OpLog Inspection (`oz oplog` & `oz undo --list`) (`oz-oplog`, `oz-cli`):** Added `oz oplog` (with `--limit`, `--all`, `--json`, and `ol` alias) and `oz undo --list` to inspect the MVCC operation journal, display operation IDs, kinds, timestamps, undo/redo pointers, and target snapshots before rolling back.
- **Grouped 6-Domain `oz --help` & Updated Command Summaries (`oz-cli`):** Reorganized top-level `oz --help` into 6 clear task domains (Daily VCS, Inspect & History, Workspaces & Integration, Undo & Recovery, GitHub & Stacked PRs, Setup & Agent Tooling) with refreshed `pr`, `issue`, and `ci` subcommand summaries.
- **Progressive-Disclosure Agent Harness Setup (`oz-agent`):** Split `oz agent setup` templates into compact always-on instruction rules plus full on-demand `SKILL.md` workflow guides across supported coding-agent harnesses, keeping default context footprint minimal while preserving deep workflow references.

### Bug Fixes & Compatibility
- **Cargo 1.100 Build Layout & Authenticated Windows Mock TCP IPC (`oz-warm`, `oz-cow`, `oz-integrate`):** Added support for Cargo 1.100's new build directory layout (`build-dir-new-layout`) in warm cache classification and updated Windows mock TCP IPC tests to use per-instance authenticated endpoints.
- **GitHub Pages Deployment Workflow (`.github/workflows`):** Split GitHub Pages build and deploy jobs and bumped Pages actions to Node 24.

### Dependencies & Release Automation
- Bumped `bytes` from `1.12.1` to `1.12.2` and updated GitHub Actions (`actions/upload-artifact` v7, `actions/download-artifact` v8, `actions/attest-build-provenance` v4, `softprops/action-gh-release` v3).

## [1.2.0] — 2026-11-25

### Features & Workflow Ergonomics
- **User-Wide Hardened `oz-vfsd` Daemon & Writable Warm Caches (`oz-vfsd`, `oz-cow`, `oz-warm`):** Added user-wide hardened `oz-vfsd` systemd service support and writable `pnpm` monorepo warm-cache materialization (`node_modules/.modules.yaml`, `.pnpm-workspace-state-v1.json`, `node_modules/.bin/*`, and nested workspace `node_modules`) under `cas_hardlink`.
- **Full GitHub PR & Issue Lifecycle (`oz-github`, `oz-cli`):**
  - Added `oz pr list`, `oz pr edit`, `oz pr comments <n> [--unresolved]`, `oz pr view <n> [--comments] [--brief|--no-body|--no-files]`, `oz pr reply` (`t1` / `#id` + `--resolve`), `oz pr resolve`, `oz pr status --pr ...`, and `--body-file` support.
  - Added `oz issue view <n|url> [--comments]`, `oz issue list`, and `oz issue comment` using REST-only endpoints (avoiding deprecated classic `projectCards` GraphQL queries).
- **Multi-PR / Workflow CI & `watch_superseded` Race Fix (`oz-github`, `oz-cli`):** Fixed the `watch_superseded` race condition in `oz ci --watch`, added multi-PR snapshots (`oz ci --pr 1,2`) and workflow filtering (`oz ci --workflow <file|name> [--watch]`), added cross-workflow `oz ci --runs`, and made `oz ci --json` compact passing checks by default (`--verbose` for full list) with null omission and accurate top-level `branch`/`head_oid` envelopes.
- **Local VCS & Multi-Branch Ergonomics (`oz-core`, `oz-integrate`, `oz-cli`):**
  - Added `oz merge <rev>` (`--no-ff`, `--ff-only`, linked worktree sync, and OpLog `oz undo` reversibility).
  - Added rev-scoped tree `oz grep <pattern> [<rev>] [-- <paths...>]` with `-n`, `--exclude`, `:!` negative pathspecs, `--max-line-len`, and BRE `\|` dialect hints.
  - Added `oz branch -a` / `-r` (local + remote-tracking branch SHAs), multi-refspec `oz push origin b1 b2 b3`, enhanced `oz log` (multi-rev, `A...B --left-right`, `-i`, `--no-notes`, `--format fuller`), and `oz show <missing-hex-sha>` recovery hints pointing to `oz fetch`.

### Security & Soundness Hardening
- **Constant-Time SSH Signing & Secret Zeroization (`oz-core`):** Replaced hand-rolled variable-time Ed25519 and SHA-2 implementations with audited pure-Rust constant-time crates (`ed25519-dalek`, `sha2`, `zeroize`), zeroizing secret key material on drop and removing `Ed25519SigningKey::seed()` from the public API.
- **Filter Command Injection & Pipe Deadlock Fixes (`oz-core`):** Eliminated Windows `cmd.exe /C` shell interpolation for `.gitattributes` `%f` filters and switched filter stdin writes to concurrent scoped threads so `> 64 KiB` blobs never deadlock.
- **Hardened `oz-vfsd` IPC & Mount Validation (`oz-vfsd`):** Added per-instance random auth tokens for Windows loopback TCP IPC (with `WRITE_DAC` permissions on endpoint files), fail-closed Unix `peer_cred()` enforcement, owner-verified `0o700` socket parent directories, bounded IPC frame reads (`1 MiB`) and concurrency limits, canonicalized non-symlink + UID-ownership validation for `upper_dir`/`meta_dir`/`baseline_dir`, and `tokio::signal` shutdown handling.
- **Windows ProjFS FFI, Workspace Unsafe Lint & Bounded MCP Framing (`oz-vfsd`, `oz-agent`):** Enabled workspace-wide `undocumented_unsafe_blocks = "deny"`, added compile-time `WinState: Send + Sync` assertions, size-checked `transmute`, buffer bounds checks before `copy_nonoverlapping`, explicit `// SAFETY:` invariants across all `unsafe` blocks, and bounded sync/async MCP header/line/payload framing limits (`4 MiB` line / `16 MiB` message / 32 headers).

### Performance & Concurrency
- **Lock-Split FUSE3 Overlay (`oz-vfsd`):** Decoupled blocking disk copy-up and directory materialization from short-lived metadata/inode locks, serialized `lookup` and `getattr` via shared path locks to prevent inode races, and replaced lock-poisoning `.expect()` calls with graceful recovery.
- **Async MCP & Pooled GitHub HTTP Client (`oz-agent`, `oz-github`):** Offloaded blocking MCP tool dispatch in `serve_async` via `tokio::task::spawn_blocking` and pooled `reqwest::blocking::Client` in `GitHubClient` with async-runtime-safe off-thread drop handling.
- **Diff & Hot-Path Optimizations (`oz-core`, `oz-integrate`, `oz-agent`, `oz-oplog`):** Unified diffing onto `gix::diff::blob::imara_diff` to remove duplicate dependency trees, avoided unnecessary allocations in `status.rs`/`daily.rs`/`projfs.rs`, and replaced Unix `kill -0` subprocesses in `oz fsck` with direct `libc::kill(pid, 0)`.

### Bug Fixes
- **`oz undo` Index Stat-Cache Fix (`oz-oplog`):** Fixed `sync_git_index_diff` unconditionally stamping `Stat::from_fs` onto patched `.git/index` entries when restoring to a target snapshot with dirty files (such as `oz undo` after `oz commit`, `--amend`, `--pick`, `switch -f`, or `reset --hard`). Dirty target paths now receive zeroed `Stat::default()` (and risky same-OID cached stats are invalidated) so working-tree edits are never hidden from `oz status`, `git status`, or subsequent OpLog scans while `oz redo` restores cleanly; hardened `snapshot_from_standard_workdir`'s index stat shortcut to reject zeroed stats, symlinks, and Unix executable-bit mismatches.
- **Writable Cargo `target/` in `_integrate` & `cas_hardlink` Sandboxes (`oz-warm`, `oz-cow`, `oz-integrate`):** Fixed `oz integrate --verify "cargo test"` (and `oz ws spawn --backend cas_hardlink`) failing with `Permission denied` writing into `target/`. Added `CargoTargetClassifier` (`SkipTransientLock`, `MutableWritable`, `ImmutableCas`) so Cargo fingerprints, `.rustc_info.json`, dep-info `*.d`, `build/**`, `incremental/**`, and local crate outputs are materialized writable while immutable external crate dependency artifacts (including external `build.rs` `OUT_DIR` outputs) remain `0444` CAS-hardlinked; added baseline mtime restoration for integrated trees in `_integrate`.
- **FUSE3 Test Lifecycle & Teardown (`oz-cli`, `oz-vfsd`):** Added RAII FUSE3 mount teardown across integration test suites to prevent leaked `fuse.ozone` mounts and gated live FUSE3 mount tests cleanly under `NoNewPrivs=1`.

### Release Engineering, CI & Packaging
- **Automated Homebrew & `winget` Release Manifests:** Added `packaging/populate-manifests.py` and wired `.github/workflows/release.yml` to generate, strictly validate against `SHA256SUMS.txt`, and publish populated `oz.rb` and `OzoneVCS.Ozone*.yaml` release assets on every tagged release.
- **Release Profile, Publish Metadata & CI Hardening:** Added optimized `[profile.release]` (`lto = "thin"`, `codegen-units = 1`, `strip = "symbols"`, `panic = "unwind"`), workspace publish metadata, strict `--strict` / `--allow-placeholders` / `--checksums` checks in `packaging/validate.sh`, `cargo fmt --all -- --check` and `-D clippy::undocumented_unsafe_blocks` in `.github/workflows/ci.yml`, pruned unused dependencies, and narrowed internal `oz-cli` exports.

## [1.1.0] — 2026-11-24

### Agent Ergonomics & PR Review Release
- **Line-Numbered Diffs (`old_line` / `new_line`):** Every `DiffLine` now carries exact 1-based `old_line` / `new_line` coordinates in `--json` and MCP output, with `oz diff --line-numbers` rendering explicit side-by-side line columns.
- **Safe `oz diff` Excludes & Git Negative Pathspecs:** Added `--exclude <pattern|glob|f1|h1a>` on `oz diff` (matching `oz commit --exclude`) and native Git negative pathspecs (`:!pnpm-lock.yaml`, `:^...`, `:(exclude)...`). Exclusion-only filters now subtract from all changed files instead of returning a silent `0 file(s) changed`, and unsupported `:(...)` pathspec magic fails loudly.
- **Compact `oz status` Hunk Lists:** Files with more than 5 hunks now collapse inline to `(N hunks: h1a..h1al)` to avoid blowing context windows on lockfiles; pass `oz status --hunks` (or `--json`) for full enumeration.
- **Core Local VCS Read Parity:**
  - `oz log [REV] [-- <PATHS>...]` adds Merkle-tree path filtering (with glob fallback) plus `--pickaxe`, `--grep-diff`, and `--rev` CLI aliases matching MCP parameter names.
  - `oz show <rev>:<path>` reads blobs at any revision without mutating the working tree, with `--lines <START..END>`, `--line-numbers`, binary detection, and `--json`.
  - `oz rev-parse` resolves revisions (`--short[=<LEN>]`, `--verify`, `--abbrev-ref`) and repository paths (`--show-toplevel`, `--git-dir`, `--git-common-dir`, `--is-inside-work-tree`).
- **Workspace Spawn Polish:** Wired `--backend <BACKEND>` (`auto`, `cas_hardlink`, `linux_fuse3`, etc.) on CLI `oz ws spawn` and added explicit `clean @ <short-oid>` cleanliness confirmation in spawn output (`clean: true` in JSON).
- **Native GitHub PR Review Loop (`oz pr view|diff|read|review`):**
  - `oz pr view <n>` shows compact PR metadata, full head/base SHAs, changed files, and review states.
  - `oz pr diff <n>` parses PR diffs into deterministic `[f1]`/`[h1a]` IDs with exact `old_line`/`new_line` columns enabled by default (`--stat`, `--exclude`, `--no-line-numbers`).
  - `oz pr read <n> <path> [--lines <START..END>]` reads full file context at the exact PR head commit (local `gix` fast path with automatic GitHub Contents API fallback).
  - `oz pr review <n>` submits `APPROVE` / `REQUEST_CHANGES` / `COMMENT` reviews with auto-filled `commit_id` and local pre-flight validation of inline comment paths/lines/sides against PR diff hunks (`--inline`, `--comments-file`, `--dry-run`).
- **Streamlined Agent Harness Templates:** Updated `oz agent setup` workflow templates to reflect exact v1.1.0 CLI syntax and clarify key Git behavioral differences.

## [1.0.0] — 2026-11-22

### Highlights
- **Pure-Rust, 100% Colocated Git Engine:** Built on `gix`, `imara-diff`, `reqwest`/`rustls`, and pure-Rust FUSE3/ProjFS — zero `libgit2`, `OpenSSL`, `libcurl`, `libssh2`, or `libfuse` C dependencies.
- **Zero-Byte CoW Workspaces (`oz ws spawn|list|rm|gc`):** Tiered native backend selection across macOS (`APFS clonefile`), Linux (`Btrfs`/`XFS`/`bcachefs` `FICLONE` and unprivileged `/dev/fuse` `oz-vfsd`), Windows (`Dev Drive ReFS` and `NTFS ProjFS`), and `cas_hardlink` fallback.
- **Warm Build Cache Engine (`oz-warm`):** Preserves `target/`, `node_modules/`, and `.venv/` with nanosecond `mtime` fidelity, Cargo 1.99 lockfile exclusion (`.cargo-build-lock`, `.cargo-artifact-lock`), `.cargo/config.toml` / `CARGO_TARGET_DIR` detection, and portable Python `.venv` trampoline/shebang/symlink isolation.
- **Lock-Free MVCC OpLog & Sub-10ms Undo (`oz checkpoint`, `oz undo`, `oz redo`):** Atomic journaled operations without `.git/index.lock` races across concurrent agents, plus `oz gc --oplog-days` compaction and `oz fsck --repair`.
- **Stage-Free Working-Tree Commits & Guardrails:** Deterministic `[f1]` / `h1a` IDs (`oz status`, `oz diff`, `oz commit --pick/--exclude`), automatic secret/conflict-marker/large-blob/scope guardrails, `Change-Id:` trailers, and pure-Rust Ed25519 SSH commit/tag signing.
- **Day-3 Git Compatibility:** `.git/hooks/` and `core.hooksPath` hooks (`--no-verify`), `.gitattributes` (`text`, `eol=lf`/`crlf`, `binary`/`-diff`, clean/smudge/`filter=lfs` safety), `160000` gitlink submodules, layered Git config (`include`/`includeIf`, `url.<base>.insteadOf`), `~/.ssh/config` support, and audited `oz git` shim compatibility.
- **Everyday Human CLI Ergonomics:** `oz log`, `oz show`, `oz stash` (backed by OpLog undo/redo), `oz cherry-pick`, `oz revert`, `oz tag`, `oz branch`, automatic TTY paging (`$OZ_PAGER` / `$GIT_PAGER` / `delta` / `less -FRX`), `oz completions`, and `.ozone.toml` short aliases (`st`, `ci`, `co`, `lg`, `br`).
- **Multi-Agent Coordination, GitHub Stacks & MCP:** `oz radar`, in-memory `oz integrate --verify` + `_integrate/` sandbox (`oz resolve`), `oz stack`, `oz pr create|stack`, `oz sync`, `oz ci`, `oz agent setup` (5 harnesses), `oz mcp` (11 tools), and `refs/notes/ai` `oz blame`.
- **Safety & Cross-Platform Packaging:** `proptest` safety suites verifying reference `git` tree-OID parity and `git fsck --full`, static release tarballs/zips, `.deb` packaging, Homebrew formula, winget manifests, and Cosign/SLSA release provenance.
