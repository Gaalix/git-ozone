# Security Policy

## Supported Versions

| Version | Supported |
| :--- | :---: |
| `1.4.x` / `main` | Yes |
| `< 1.4.0` | No |

## Reporting a Vulnerability

Please **do not** open public GitHub Issues for suspected security vulnerabilities (including secret-guardrail bypasses, FUSE/ProjFS path-traversal issues, SSH signing bugs, or Wire Protocol vulnerabilities).

Instead, report vulnerabilities privately through [GitHub Security Advisories](https://github.com/Gaalix/git-ozone/security/advisories/new). Please include:

- Steps to reproduce or a minimal proof of concept
- Affected `oz --version --json` and `oz doctor --json` output
- Potential security impact

We aim to acknowledge reports within 48 hours and coordinate a fix before public disclosure.

## Verifying Release Artifacts

Every tagged GitHub Release publishes:
- `SHA256SUMS.txt`
- `SHA256SUMS.txt.sigstore.json` (keyless Cosign OIDC bundle)
- GitHub SLSA Build Provenance attestations (`actions/attest-build-provenance`)

You can verify downloaded release artifacts with:

```bash
sha256sum -c SHA256SUMS.txt
cosign verify-blob --bundle SHA256SUMS.txt.sigstore.json SHA256SUMS.txt
gh attestation verify oz-v1.4.5-x86_64-unknown-linux-gnu.tar.gz -R Gaalix/git-ozone
```

## Pure-Rust Supply-Chain Guarantee

Ozone compiles strictly without C FFI bindings to `libgit2`, `OpenSSL`, `libcurl`, `libssh2`, or `libfuse`, avoiding whole classes of C memory-safety CVEs. Every push and release runs an automated `Cargo.lock` guard to enforce this invariant.
