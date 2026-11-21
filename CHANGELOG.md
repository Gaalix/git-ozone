# Changelog

All notable changes to Ozone (`oz` / `git-ozone` / `oz-vfsd`) are documented in this file.

## [1.0.0] — 2026-11-22

### Highlights
- **Pure-Rust, 100% Colocated Git Engine:** Built on `gix`, `imara-diff`, `reqwest`/`rustls`, and pure-Rust FUSE3/ProjFS — zero `libgit2`, `OpenSSL`, `libcurl`, `libssh2`, or `libfuse` C dependencies.
- **Zero-Byte CoW Workspaces (`oz ws spawn|list|rm|gc`):** Tiered native backend selection across macOS (`APFS clonefile`), Linux (`Btrfs`/`XFS`/`bcachefs` `FICLONE` and unprivileged `/dev/fuse` `oz-vfsd`), Windows (`Dev Drive ReFS` and `NTFS ProjFS`), and `cas_hardlink` fallback.
- **Warm Build Cache Engine (`oz-warm`):** Preserves `target/`, `node_modules/`, and `.venv/` with nanosecond `mtime` fidelity, Cargo 1.99 lockfile exclusion (`.cargo-build-lock`, `.cargo-artifact-lock`), `.cargo/config.toml` / `CARGO_TARGET_DIR` detection, and portable Python `.venv` trampoline/shebang/symlink isolation.
- **Lock-Free MVCC OpLog & Sub-10ms Undo (`oz checkpoint`, `oz undo`, `oz redo`):** Atomic journaled operations without `.git/index.lock` races across concurrent agents, plus `oz gc --oplog-days` compaction and `oz fsck --repair`.
- **Stage-Free Working-Tree Commits & Guardrails:** Deterministic `[f1]` / `h1a` IDs (`oz status`, `oz diff`, `oz commit --pick/--exclude`), automatic secret/conflict-marker/large-blob/scope guardrails, `Change-Id:` trailers, and pure-Rust Ed25519 SSH commit/tag signing.
- **Day-3 Git Compatibility:** `.git/hooks/` and `core.hooksPath` hooks (`--no-verify`), `.gitattributes` (`text`, `eol=lf`/`crlf`, `binary`/`-diff`, clean/smudge/`filter=lfs` safety), `160000` gitlink submodules, layered Git config (`include`/`includeIf`, `url.<base>.insteadOf`), `~/.ssh/config` support, and audited `oz git` shim compatibility.
- **Everyday Human CLI Ergonomics:** `oz log`, `oz show`, `oz stash` (backed by OpLog undo/redo), `oz cherry-pick`, `oz revert`, `oz tag`, `oz branch`, automatic TTY paging (`$OZ_PAGER` / `$GIT_PAGER` / `delta` / `less -FRX`), `oz completions`, and `.ozone.toml` short aliases (`st`, `ci`, `co`, `lg`, `br`).
- **Multi-Agent Coordination, GitHub Stacks & MCP:** `oz radar`, in-memory `oz integrate --verify` + `_integrate/` sandbox (`oz resolve`), `oz stack`, `oz pr create|stack`, `oz sync`, `oz ci`, `oz agent setup` (5 harnesses), `oz mcp` (8 tools), and `refs/notes/ai` `oz blame`.
- **Safety & Cross-Platform Packaging:** `proptest` safety suites verifying reference `git` tree-OID parity and `git fsck --full`, static release tarballs/zips, `.deb` packaging, Homebrew formula, winget manifests, and Cosign/SLSA release provenance.
