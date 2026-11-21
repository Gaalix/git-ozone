## Summary

<!-- Briefly describe what this PR changes and why. -->

## Checklist

- [ ] Pure-Rust invariant preserved (`Cargo.lock` contains no `openssl-sys`, `libgit2-sys`, `curl-sys`, `fuse-sys`, or `libfuse`)
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` passes
- [ ] `cargo test --workspace --locked` passes (including new/updated tests)
- [ ] `--json` / `--agent` output conforms to `oz.v1` schema (`docs/schemas/oz.v1.envelope.schema.json`) and disables interactive pagers/prompts
- [ ] `./packaging/validate.sh` passes (if packaging, scripts, or workflows were touched)
