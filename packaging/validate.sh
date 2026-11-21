#!/usr/bin/env bash
# Copyright (c) 2026 Ozone Contributors
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Validates Ozone packaging scripts, Homebrew formula, Debian .deb builder,
# Windows winget manifests, PowerShell installer, and GitHub Actions workflows.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

cd "${REPO_ROOT}"

pass() {
  printf '[PASS] %s\n' "$*"
}

warn() {
  printf '[WARN] %s\n' "$*"
}

fail() {
  printf '[FAIL] %s\n' "$*" >&2
  exit 1
}

echo "=== Ozone Packaging Validation Suite ==="

# 1. Bash syntax checks
for script in \
  packaging/install.sh \
  packaging/deb/build-deb.sh \
  packaging/validate.sh
do
  [[ -f "$script" ]] || fail "missing script: $script"
  bash -n "$script"
  pass "bash -n ${script}"
done

# 1b. ShellCheck static analysis (if shellcheck is installed)
if command -v shellcheck >/dev/null 2>&1; then
  shellcheck -x \
    packaging/install.sh \
    packaging/deb/build-deb.sh \
    packaging/validate.sh
  pass "shellcheck packaging/install.sh packaging/deb/build-deb.sh packaging/validate.sh"
else
  warn "shellcheck not installed; skipped ShellCheck lint (bash -n passed)"
fi

# 2. YAML syntax & cross-file consistency checks (workflows + winget manifests)
python3 - <<'PY'
import pathlib
import re
import sys

try:
    import yaml
except ImportError as exc:
    print(f"[FAIL] PyYAML is required for YAML validation: {exc}", file=sys.stderr)
    sys.exit(1)

yaml_files = [
    ".github/workflows/ci.yml",
    ".github/workflows/release.yml",
    "packaging/winget/OzoneVCS.Ozone.yaml",
    "packaging/winget/OzoneVCS.Ozone.installer.yaml",
    "packaging/winget/OzoneVCS.Ozone.locale.en-US.yaml",
]

docs = {}
for rel in yaml_files:
    p = pathlib.Path(rel)
    if not p.is_file():
        print(f"[FAIL] Missing YAML file: {rel}", file=sys.stderr)
        sys.exit(1)
    with p.open("r", encoding="utf-8") as f:
        data = yaml.safe_load(f)
    if not isinstance(data, dict):
        print(f"[FAIL] YAML root must be a mapping: {rel}", file=sys.stderr)
        sys.exit(1)
    docs[rel] = data
    print(f"[PASS] YAML syntax valid: {rel}")

# Validate ci.yml requirements
ci_text = pathlib.Path(".github/workflows/ci.yml").read_text(encoding="utf-8")
for required in ("m3_exit_gates", "m4_exit_gates", "packaging/validate.sh"):
    if required not in ci_text:
        print(f"[FAIL] .github/workflows/ci.yml missing required check: {required}", file=sys.stderr)
        sys.exit(1)
print("[PASS] ci.yml includes m3_exit_gates, m4_exit_gates, and packaging/validate.sh")

# Validate release.yml requirements
rel_doc = docs[".github/workflows/release.yml"]
rel_text = pathlib.Path(".github/workflows/release.yml").read_text(encoding="utf-8")
on_block = rel_doc.get("on", rel_doc.get(True))
if not isinstance(on_block, dict) or "push" not in on_block or "workflow_dispatch" not in on_block:
    print("[FAIL] release.yml must trigger on push tags and workflow_dispatch", file=sys.stderr)
    sys.exit(1)

expected_targets = {
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
}
matrix_entries = rel_doc["jobs"]["build"]["strategy"]["matrix"]["include"]
found_targets = {entry["target"] for entry in matrix_entries}
if found_targets != expected_targets:
    print(f"[FAIL] release.yml targets mismatch: expected {sorted(expected_targets)}, got {sorted(found_targets)}", file=sys.stderr)
    sys.exit(1)

for required in (
    "openssl-sys",
    "libgit2-sys",
    "curl-sys",
    "fuse-sys",
    "libfuse",
    "SHA256SUMS.txt",
    "actions/attest-build-provenance",
    "cosign",
    "oz-v${VERSION}-${TARGET}",
    "packaging/deb/build-deb.sh",
):
    if required not in rel_text:
        print(f"[FAIL] release.yml missing expected element: {required}", file=sys.stderr)
        sys.exit(1)
print("[PASS] release.yml matrix, pure-Rust guard, archives, .deb, checksums, and signing verified")

# Validate winget manifests
wg_ver = docs["packaging/winget/OzoneVCS.Ozone.yaml"]
wg_inst = docs["packaging/winget/OzoneVCS.Ozone.installer.yaml"]
wg_loc = docs["packaging/winget/OzoneVCS.Ozone.locale.en-US.yaml"]

for name, doc, mtype in (
    ("version", wg_ver, "version"),
    ("installer", wg_inst, "installer"),
    ("locale", wg_loc, "defaultLocale"),
):
    assert doc.get("PackageIdentifier") == "OzoneVCS.Ozone", f"winget {name} PackageIdentifier mismatch"
    assert doc.get("PackageVersion") == "1.0.0", f"winget {name} PackageVersion mismatch"
    assert doc.get("ManifestType") == mtype, f"winget {name} ManifestType mismatch"
    assert doc.get("ManifestVersion") == "1.6.0", f"winget {name} ManifestVersion mismatch"

inst_entry = wg_inst["Installers"][0]
expected_win_url = "https://github.com/ozone-vcs/ozone/releases/download/v1.0.0/oz-v1.0.0-x86_64-pc-windows-msvc.zip"
assert inst_entry["InstallerUrl"] == expected_win_url, f"winget InstallerUrl mismatch: {inst_entry['InstallerUrl']}"
assert re.fullmatch(r"[A-F0-9]{64}", inst_entry["InstallerSha256"]), "winget InstallerSha256 must be 64 uppercase hex chars"

nested = {item["PortableCommandAlias"]: item["RelativeFilePath"] for item in wg_inst["NestedInstallerFiles"]}
assert nested == {
    "oz": r"oz-v1.0.0-x86_64-pc-windows-msvc\oz.exe",
    "git-ozone": r"oz-v1.0.0-x86_64-pc-windows-msvc\git-ozone.exe",
    "oz-vfsd": r"oz-v1.0.0-x86_64-pc-windows-msvc\oz-vfsd.exe",
}, f"winget NestedInstallerFiles mismatch: {nested}"
print("[PASS] winget manifests schema, URL, and nested binary layout verified")

# Cross-check Homebrew formula archive URLs and test assertions
rb_text = pathlib.Path("packaging/homebrew/oz.rb").read_text(encoding="utf-8")
for expected_rb in (
    "class Oz < Formula",
    'license any_of: ["MIT", "Apache-2.0"]',
    "oz-v#{version}-aarch64-apple-darwin.tar.gz",
    "oz-v#{version}-x86_64-apple-darwin.tar.gz",
    "oz-v#{version}-aarch64-unknown-linux-gnu.tar.gz",
    "oz-v#{version}-x86_64-unknown-linux-gnu.tar.gz",
    'bin.install "oz"',
    'bin.install "git-ozone"',
    'bin.install "oz-vfsd"',
    "oz --version --json",
    "git-ozone doctor --json",
):
    if expected_rb not in rb_text:
        print(f"[FAIL] packaging/homebrew/oz.rb missing: {expected_rb}", file=sys.stderr)
        sys.exit(1)
print("[PASS] Homebrew formula URLs, binaries, and test block verified")

# Cross-check PowerShell installer key invariants
ps1_text = pathlib.Path("packaging/install.ps1").read_text(encoding="utf-8")
for expected_ps1 in (
    "x86_64-pc-windows-msvc",
    "oz-$Tag-$Target",
    "SHA256SUMS.txt",
    "Get-FileHash",
    "oz.exe",
    "git-ozone.exe",
    "oz-vfsd.exe",
    "Client-ProjFS",
    "oz setup",
    "[switch]$DryRun",
):
    if expected_ps1 not in ps1_text:
        print(f"[FAIL] packaging/install.ps1 missing: {expected_ps1}", file=sys.stderr)
        sys.exit(1)
print("[PASS] PowerShell installer archive naming, checksum, PATH, and ProjFS checks verified")

# If [package.metadata.binstall] exists in crates/oz-cli/Cargo.toml, verify consistency
cli_toml_path = pathlib.Path("crates/oz-cli/Cargo.toml")
if cli_toml_path.is_file():
    import tomllib
    with cli_toml_path.open("rb") as f:
        cli_data = tomllib.load(f)
    binstall = cli_data.get("package", {}).get("metadata", {}).get("binstall")
    if isinstance(binstall, dict):
        pkg_url = binstall.get("pkg-url", "")
        bin_dir = binstall.get("bin-dir", "")
        if "oz-v{ version }-{ target }" not in pkg_url:
            print(f"[FAIL] binstall pkg-url does not match release archive naming: {pkg_url}", file=sys.stderr)
            sys.exit(1)
        if "oz-v{ version }-{ target }" not in bin_dir:
            print(f"[FAIL] binstall bin-dir does not match release archive layout: {bin_dir}", file=sys.stderr)
            sys.exit(1)
        print("[PASS] crates/oz-cli/Cargo.toml [package.metadata.binstall] matches release archive naming")
PY

# 3. Ruby syntax check (if ruby is installed)
if command -v ruby >/dev/null 2>&1; then
  ruby -c packaging/homebrew/oz.rb >/dev/null
  pass "ruby -c packaging/homebrew/oz.rb"
else
  warn "ruby not installed; skipped ruby -c (static formula checks passed)"
fi

# 4. PowerShell AST syntax check (if pwsh is installed)
if command -v pwsh >/dev/null 2>&1; then
  # PowerShell variables ($tokens, $errors) are intentionally single-quoted for pwsh.
  # shellcheck disable=SC2016
  pwsh -NoLogo -NoProfile -Command '
    $tokens = $null
    $errors = $null
    [void][System.Management.Automation.Language.Parser]::ParseFile("packaging/install.ps1", [ref]$tokens, [ref]$errors)
    if ($errors.Count -gt 0) {
      $errors | ForEach-Object { Write-Error $_.ToString() }
      exit 1
    }
  '
  pass "pwsh AST syntax check packaging/install.ps1"
else
  warn "pwsh not installed; skipped PowerShell AST check (static checks passed)"
fi

# 5. Functional test: install.sh --dry-run
dry_run_out="$(./packaging/install.sh --version 1.0.0 --prefix /tmp/oz-test-prefix --dry-run)"
printf '%s\n' "$dry_run_out" | grep -Fq "oz-v1.0.0-" || fail "install.sh --dry-run missing archive name"
printf '%s\n' "$dry_run_out" | grep -Fq "SHA256SUMS.txt" || fail "install.sh --dry-run missing SHA256SUMS.txt"
printf '%s\n' "$dry_run_out" | grep -Fq "/tmp/oz-test-prefix/bin" || fail "install.sh --dry-run missing install dir"
pass "packaging/install.sh --dry-run"

# 6. Functional test: build-deb.sh --dry-run (amd64 + arm64) and real dpkg-deb build test
for deb_arch in amd64 arm64; do
  deb_dry="$(./packaging/deb/build-deb.sh --version 1.0.0 --arch "$deb_arch" --dry-run)"
  printf '%s\n' "$deb_dry" | grep -Fq "Package: ozone" || fail "build-deb.sh (${deb_arch}) missing Package field"
  printf '%s\n' "$deb_dry" | grep -Fq "Architecture: ${deb_arch}" || fail "build-deb.sh (${deb_arch}) missing Architecture field"
  printf '%s\n' "$deb_dry" | grep -Fq "Recommends: fuse3" || fail "build-deb.sh (${deb_arch}) missing Recommends: fuse3"
  printf '%s\n' "$deb_dry" | grep -Fq "./usr/bin/oz" || fail "build-deb.sh (${deb_arch}) missing /usr/bin/oz"
  printf '%s\n' "$deb_dry" | grep -Fq "./usr/bin/git-ozone" || fail "build-deb.sh (${deb_arch}) missing /usr/bin/git-ozone"
  printf '%s\n' "$deb_dry" | grep -Fq "./usr/bin/oz-vfsd" || fail "build-deb.sh (${deb_arch}) missing /usr/bin/oz-vfsd"
  pass "packaging/deb/build-deb.sh --dry-run (${deb_arch})"
done

if command -v dpkg-deb >/dev/null 2>&1; then
  deb_tmp="$(mktemp -d)"
  trap 'rm -rf "$deb_tmp"' EXIT
  mkdir -p "${deb_tmp}/bins" "${deb_tmp}/out"
  for b in oz git-ozone oz-vfsd; do
    printf '#!/bin/sh\necho "%s stub"\n' "$b" > "${deb_tmp}/bins/${b}"
    chmod 0755 "${deb_tmp}/bins/${b}"
  done
  ./packaging/deb/build-deb.sh \
    --version 1.0.0 \
    --arch amd64 \
    --bin-dir "${deb_tmp}/bins" \
    --out-dir "${deb_tmp}/out" >/dev/null

  deb_file="${deb_tmp}/out/ozone_1.0.0_amd64.deb"
  [[ -f "$deb_file" ]] || fail "expected .deb not created: ${deb_file}"
  dpkg-deb --info "$deb_file" | grep -Fq "Recommends: fuse3" || fail ".deb metadata missing Recommends: fuse3"
  deb_contents="$(dpkg-deb --contents "$deb_file")"
  for expected_path in "./usr/bin/oz" "./usr/bin/git-ozone" "./usr/bin/oz-vfsd" "./usr/share/doc/ozone/copyright"; do
    printf '%s\n' "$deb_contents" | grep -Fq "$expected_path" || fail ".deb missing ${expected_path}"
  done
  pass "packaging/deb/build-deb.sh real dpkg-deb build & inspection"
else
  warn "dpkg-deb not installed; skipped real .deb build test"
fi

echo "=== All packaging validations passed ==="
