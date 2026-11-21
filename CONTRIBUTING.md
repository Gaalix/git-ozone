# Contributing to Ozone (`oz`)

Thank you for contributing to Ozone! All contributions are dual-licensed under **MIT OR Apache-2.0**.

## Prerequisites

- **Rust toolchain:** 1.85+ (edition 2024) with `clippy` and `rustfmt`
- **Git & test tools:** `git`, `git-lfs` (for LFS integration tests), `python3` + `PyYAML` (for packaging validation), and optionally `cargo-nextest`, `shellcheck`, `ruby`, `delta`, and `fuse3` (`fusermount3` on Linux)

## Core Invariants

1. **Pure-Rust supply chain:** Never add C-backed native dependencies (`openssl-sys`, `libgit2-sys`, `curl-sys`, `fuse-sys`, `libfuse`, `libssh2-sys`). CI enforces this on `Cargo.lock` across every PR.
2. **Colocated `.git/` compatibility:** Repositories remain 100% standard `.git/` directories interoperable with C `git`, IDEs, `gh`, and CI. Never break `git fsck --full`.
3. **Structured `oz.v1` JSON contract:** Every CLI command supporting `--json` must emit the canonical [`oz.v1` envelope](docs/schemas/oz.v1.envelope.schema.json) (`schema`, `ok`, `exit_code`, `data` / `error`) and keep stdout clean in `--json`, `--agent`, `OZ_AGENT=1`, and non-TTY modes (no interactive prompts or pagers).
4. **Workspace isolation:** Check all workspace crate boundaries in [ARCHITECTURE.md](ARCHITECTURE.md) before introducing cross-crate dependencies.

## Development Workflow

```bash
# 1. Build and lint
cargo build --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings

# 2. Run the workspace test suite
cargo test --workspace --locked
# (Or with nextest; in restricted NoNewPrivs containers, exclude live /dev/fuse tests:)
# cargo nextest run --workspace --locked -E 'not (binary(fuse_live) | binary(m1_exit_gates) | binary(m2_exit_gates))'

# 3. Validate packaging scripts, shellcheck, winget manifests, and workflows
./packaging/validate.sh
```

## Opening a Pull Request

1. Fork [Gaalix/git-ozone](https://github.com/Gaalix/git-ozone) and create a focused branch from `main`.
2. Add or update unit/integration tests for your change (`proptest` suites live in `oz-core`, `oz-integrate`, and `oz-oplog`).
3. Ensure `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`, and `./packaging/validate.sh` pass.
4. Fill out the pull request checklist.
