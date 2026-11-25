#!/usr/bin/env bash
# Copyright (c) 2026 Ozone Contributors
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Validates Ozone packaging scripts, Homebrew formula, Debian .deb builder,
# Windows winget manifests, PowerShell installer, and GitHub Actions workflows.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

STRICT_FLAG=0
ALLOW_PLACEHOLDERS_FLAG=0
HOMEBREW_CUSTOM=0
WINGET_CUSTOM=0
HOMEBREW_PATH="packaging/homebrew/oz.rb"
WINGET_DIR="packaging/winget"
EXPECTED_VERSION=""
CHECKSUMS_PATH=""

usage() {
  cat <<'EOF'
Usage: validate.sh [OPTIONS]

Validate Ozone packaging scripts, manifests, and GitHub Actions workflows.

Options:
  --strict, --release    Reject all-zero placeholder SHA256 checksums in Homebrew & winget manifests
  --allow-placeholders   Allow all-zero placeholder SHA256 checksums (e.g. pre-build template checks)
  --homebrew <PATH>      Path to Homebrew formula to validate (default: packaging/homebrew/oz.rb)
  --winget-dir <DIR>     Directory containing winget manifests (default: packaging/winget)
  --version <VER>        Expected release version (defaults to Cargo.toml workspace version)
  --checksums <PATH>     Cross-check manifest SHA256 checksums against SHA256SUMS.txt (implies --strict)
  -h, --help             Show this help message

Mode defaults:
  - Strict mode is enabled automatically when --strict/--release, --checksums, custom manifest
    paths, or a tagged GitHub Actions ref (GITHUB_REF_TYPE=tag / refs/tags/v*) is detected,
    unless --allow-placeholders is passed.
EOF
}

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

while [[ $# -gt 0 ]]; do
  case "$1" in
    --strict|--release)
      STRICT_FLAG=1
      shift
      ;;
    --allow-placeholders)
      ALLOW_PLACEHOLDERS_FLAG=1
      shift
      ;;
    --homebrew)
      [[ $# -ge 2 ]] || fail "--homebrew requires a path argument"
      HOMEBREW_PATH="$2"
      HOMEBREW_CUSTOM=1
      shift 2
      ;;
    --winget-dir)
      [[ $# -ge 2 ]] || fail "--winget-dir requires a directory argument"
      WINGET_DIR="$2"
      WINGET_CUSTOM=1
      shift 2
      ;;
    --version)
      [[ $# -ge 2 ]] || fail "--version requires a version argument"
      EXPECTED_VERSION="${2#v}"
      shift 2
      ;;
    --checksums)
      [[ $# -ge 2 ]] || fail "--checksums requires a file path argument"
      CHECKSUMS_PATH="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown option: $1 (use --help for usage)"
      ;;
  esac
done

if [[ "$STRICT_FLAG" -eq 1 && "$ALLOW_PLACEHOLDERS_FLAG" -eq 1 ]]; then
  fail "--strict/--release and --allow-placeholders are mutually exclusive"
fi
if [[ -n "$CHECKSUMS_PATH" && "$ALLOW_PLACEHOLDERS_FLAG" -eq 1 ]]; then
  fail "--checksums and --allow-placeholders are mutually exclusive"
fi

# Resolve user-provided relative paths before changing directory to REPO_ROOT.
abs_path() {
  local p="$1"
  if [[ "$p" == /* ]]; then
    printf '%s' "$p"
  else
    printf '%s/%s' "$PWD" "$p"
  fi
}

CUSTOM_MANIFESTS=0
if [[ "$HOMEBREW_CUSTOM" -eq 1 ]]; then
  HOMEBREW_PATH="$(abs_path "$HOMEBREW_PATH")"
  CUSTOM_MANIFESTS=1
fi
if [[ "$WINGET_CUSTOM" -eq 1 ]]; then
  WINGET_DIR="$(abs_path "$WINGET_DIR")"
  CUSTOM_MANIFESTS=1
fi
if [[ -n "$CHECKSUMS_PATH" ]]; then
  CHECKSUMS_PATH="$(abs_path "$CHECKSUMS_PATH")"
fi

cd "${REPO_ROOT}"

# Determine strict vs placeholder-allowing mode.
ALLOW_PLACEHOLDERS=1
if [[ "$STRICT_FLAG" -eq 1 || -n "$CHECKSUMS_PATH" || "$CUSTOM_MANIFESTS" -eq 1 ]]; then
  ALLOW_PLACEHOLDERS=0
elif [[ "${GITHUB_REF_TYPE:-}" == "tag" || "${GITHUB_REF:-}" == refs/tags/v* ]]; then
  ALLOW_PLACEHOLDERS=0
fi
if [[ "$ALLOW_PLACEHOLDERS_FLAG" -eq 1 ]]; then
  ALLOW_PLACEHOLDERS=1
fi

TMP_DIRS=()
cleanup() {
  local d
  for d in "${TMP_DIRS[@]}"; do
    [[ -n "$d" && -d "$d" ]] && rm -rf "$d"
  done
}
trap cleanup EXIT

echo "=== Ozone Packaging Validation Suite ==="
if [[ "$ALLOW_PLACEHOLDERS" -eq 0 ]]; then
  echo "Mode: strict (all-zero SHA256 placeholders rejected)"
else
  echo "Mode: template (all-zero SHA256 placeholders allowed; --strict enforces real checksums)"
fi

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

# 1c. Python syntax check for manifest automation script (without creating __pycache__)
[[ -f packaging/populate-manifests.py ]] || fail "missing script: packaging/populate-manifests.py"
python3 -B -c '
import pathlib, sys
src = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
compile(src, sys.argv[1], "exec")
' packaging/populate-manifests.py
pass "python3 syntax check packaging/populate-manifests.py"

# 2. YAML syntax & cross-file consistency checks (workflows + winget + Homebrew + README)
OZ_ALLOW_PLACEHOLDERS="${ALLOW_PLACEHOLDERS}" \
OZ_HOMEBREW_PATH="${HOMEBREW_PATH}" \
OZ_WINGET_DIR="${WINGET_DIR}" \
OZ_EXPECTED_VERSION="${EXPECTED_VERSION}" \
OZ_CHECKSUMS_PATH="${CHECKSUMS_PATH}" \
python3 -B - <<'PY'
import os
import pathlib
import re
import sys
import tomllib

try:
    import yaml
except ImportError as exc:
    print(f"[FAIL] PyYAML is required for YAML validation: {exc}", file=sys.stderr)
    sys.exit(1)

ZERO_SHA256 = "0" * 64
allow_placeholders = os.environ["OZ_ALLOW_PLACEHOLDERS"] == "1"
homebrew_path = pathlib.Path(os.environ["OZ_HOMEBREW_PATH"])
winget_dir = pathlib.Path(os.environ["OZ_WINGET_DIR"])
expected_version_override = os.environ["OZ_EXPECTED_VERSION"].strip()
checksums_path_str = os.environ["OZ_CHECKSUMS_PATH"].strip()

wg_ver_path = winget_dir / "OzoneVCS.Ozone.yaml"
wg_inst_path = winget_dir / "OzoneVCS.Ozone.installer.yaml"
wg_loc_path = winget_dir / "OzoneVCS.Ozone.locale.en-US.yaml"

yaml_files = [
    pathlib.Path(".github/workflows/ci.yml"),
    pathlib.Path(".github/workflows/release.yml"),
    pathlib.Path(".github/workflows/pages.yml"),
    wg_ver_path,
    wg_inst_path,
    wg_loc_path,
]

docs = {}
for p in yaml_files:
    if not p.is_file():
        print(f"[FAIL] Missing YAML file: {p}", file=sys.stderr)
        sys.exit(1)
    with p.open("r", encoding="utf-8") as f:
        data = yaml.safe_load(f)
    if not isinstance(data, dict):
        print(f"[FAIL] YAML root must be a mapping: {p}", file=sys.stderr)
        sys.exit(1)
    docs[p] = data
    print(f"[PASS] YAML syntax valid: {p}")

# Validate ci.yml requirements
ci_text = pathlib.Path(".github/workflows/ci.yml").read_text(encoding="utf-8")
for required in (
    "cargo fmt --all -- --check",
    "-D clippy::undocumented_unsafe_blocks",
    "m3_exit_gates",
    "m4_exit_gates",
    "packaging/validate.sh",
):
    if required not in ci_text:
        print(f"[FAIL] .github/workflows/ci.yml missing required check: {required}", file=sys.stderr)
        sys.exit(1)
print("[PASS] ci.yml includes rustfmt, undocumented_unsafe_blocks clippy, m3/m4 exit gates, and packaging/validate.sh")

# Validate release.yml requirements
rel_doc = docs[pathlib.Path(".github/workflows/release.yml")]
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
    "packaging/populate-manifests.py",
    "--strict",
    "dist/oz.rb",
    "dist/winget/OzoneVCS.Ozone.installer.yaml",
    "actions/attest-build-provenance",
    "cosign",
    "oz-v${VERSION}-${TARGET}",
    "packaging/deb/build-deb.sh",
):
    if required not in rel_text:
        print(f"[FAIL] release.yml missing expected element: {required}", file=sys.stderr)
        sys.exit(1)
print("[PASS] release.yml matrix, pure-Rust guard, archives, .deb, manifest automation, checksums, and signing verified")

# Load expected version
if expected_version_override:
    pkg_version = expected_version_override
else:
    with pathlib.Path("Cargo.toml").open("rb") as f:
        pkg_version = tomllib.load(f)["workspace"]["package"]["version"]

# Optional SHA256SUMS.txt cross-check map
expected_checksums = {}
if checksums_path_str:
    cs_path = pathlib.Path(checksums_path_str)
    if not cs_path.is_file():
        print(f"[FAIL] Checksums file not found: {cs_path}", file=sys.stderr)
        sys.exit(1)
    for raw_line in cs_path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line:
            continue
        m = re.fullmatch(r"([0-9a-fA-F]{64})\s+\*?(\S+)", line)
        if not m:
            print(f"[FAIL] Malformed line in {cs_path}: {raw_line!r}", file=sys.stderr)
            sys.exit(1)
        expected_checksums[m.group(2)] = m.group(1).lower()

# Validate winget manifests
wg_ver = docs[wg_ver_path]
wg_inst = docs[wg_inst_path]
wg_loc = docs[wg_loc_path]

for name, doc, mtype in (
    ("version", wg_ver, "version"),
    ("installer", wg_inst, "installer"),
    ("locale", wg_loc, "defaultLocale"),
):
    assert doc.get("PackageIdentifier") == "OzoneVCS.Ozone", f"winget {name} PackageIdentifier mismatch"
    assert doc.get("PackageVersion") == pkg_version, f"winget {name} PackageVersion mismatch (expected {pkg_version}, got {doc.get('PackageVersion')})"
    assert doc.get("ManifestType") == mtype, f"winget {name} ManifestType mismatch"
    assert doc.get("ManifestVersion") == "1.6.0", f"winget {name} ManifestVersion mismatch"

expected_notes_url = f"https://github.com/Gaalix/git-ozone/releases/tag/v{pkg_version}"
assert wg_loc.get("ReleaseNotesUrl") == expected_notes_url, (
    f"winget locale ReleaseNotesUrl mismatch: {wg_loc.get('ReleaseNotesUrl')}"
)

inst_entry = wg_inst["Installers"][0]
expected_win_archive = f"oz-v{pkg_version}-x86_64-pc-windows-msvc.zip"
expected_win_url = f"https://github.com/Gaalix/git-ozone/releases/download/v{pkg_version}/{expected_win_archive}"
assert inst_entry["InstallerUrl"] == expected_win_url, f"winget InstallerUrl mismatch: {inst_entry['InstallerUrl']}"

win_sha = inst_entry["InstallerSha256"]
assert re.fullmatch(r"[A-F0-9]{64}", win_sha), "winget InstallerSha256 must be 64 uppercase hex chars"
if win_sha == ZERO_SHA256:
    if not allow_placeholders:
        print(
            f"[FAIL] {wg_inst_path}: InstallerSha256 is an all-zero placeholder (rejected in strict/release mode)",
            file=sys.stderr,
        )
        sys.exit(1)
    print(f"[WARN] {wg_inst_path}: InstallerSha256 uses all-zero template placeholder (allowed outside strict mode)")
elif expected_checksums:
    want_win_sha = expected_checksums.get(expected_win_archive)
    if not want_win_sha or win_sha != want_win_sha.upper():
        print(
            f"[FAIL] {wg_inst_path}: InstallerSha256 {win_sha} does not match {expected_win_archive} ({want_win_sha})",
            file=sys.stderr,
        )
        sys.exit(1)

nested = {item["PortableCommandAlias"]: item["RelativeFilePath"] for item in wg_inst["NestedInstallerFiles"]}
assert nested == {
    "oz": rf"oz-v{pkg_version}-x86_64-pc-windows-msvc\oz.exe",
    "git-ozone": rf"oz-v{pkg_version}-x86_64-pc-windows-msvc\git-ozone.exe",
    "oz-vfsd": rf"oz-v{pkg_version}-x86_64-pc-windows-msvc\oz-vfsd.exe",
}, f"winget NestedInstallerFiles mismatch: {nested}"
print(f"[PASS] winget manifests schema, URL, checksum format, and nested binary layout verified (v{pkg_version})")

# Cross-check Homebrew formula archive URLs, version, SHA256s, and test assertions
if not homebrew_path.is_file():
    print(f"[FAIL] Missing Homebrew formula: {homebrew_path}", file=sys.stderr)
    sys.exit(1)
rb_text = homebrew_path.read_text(encoding="utf-8")

ver_match = re.search(r'(?m)^\s*version\s+"([^"]+)"\s*$', rb_text)
if not ver_match or ver_match.group(1) != pkg_version:
    found_ver = ver_match.group(1) if ver_match else "<missing>"
    print(
        f"[FAIL] {homebrew_path}: version mismatch (expected {pkg_version}, got {found_ver})",
        file=sys.stderr,
    )
    sys.exit(1)

broken_main_formula_url = "https://raw.githubusercontent.com/Gaalix/git-ozone/main/packaging/homebrew/oz.rb"
if broken_main_formula_url in rb_text:
    print(
        f"[FAIL] {homebrew_path}: must not instruct installing the unpopulated main-branch template URL",
        file=sys.stderr,
    )
    sys.exit(1)

for expected_rb in (
    "class Oz < Formula",
    'license any_of: ["MIT", "Apache-2.0"]',
    'bin.install "oz"',
    'bin.install "git-ozone"',
    'bin.install "oz-vfsd"',
    "oz --version --json",
    "git-ozone doctor --json",
):
    if expected_rb not in rb_text:
        print(f"[FAIL] {homebrew_path} missing: {expected_rb}", file=sys.stderr)
        sys.exit(1)

hb_targets = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
)
placeholder_targets = []
for target in hb_targets:
    pat = re.compile(
        r'url\s+"https://github\.com/Gaalix/git-ozone/releases/download/v#\{version\}/'
        rf'oz-v#\{{version\}}-{re.escape(target)}\.tar\.gz"\s*\n\s*sha256\s+"([0-9a-f]{{64}})"'
    )
    matches = pat.findall(rb_text)
    if len(matches) != 1:
        print(
            f"[FAIL] {homebrew_path}: expected 1 url+sha256 block with 64 lowercase hex chars for {target}, found {len(matches)}",
            file=sys.stderr,
        )
        sys.exit(1)
    digest = matches[0]
    if digest == ZERO_SHA256:
        placeholder_targets.append(target)
    elif expected_checksums:
        archive_name = f"oz-v{pkg_version}-{target}.tar.gz"
        want_sha = expected_checksums.get(archive_name)
        if not want_sha or digest != want_sha:
            print(
                f"[FAIL] {homebrew_path}: sha256 for {target} ({digest}) does not match {archive_name} ({want_sha})",
                file=sys.stderr,
            )
            sys.exit(1)

if placeholder_targets:
    if not allow_placeholders:
        print(
            f"[FAIL] {homebrew_path}: all-zero placeholder sha256 found for {', '.join(placeholder_targets)} (rejected in strict/release mode)",
            file=sys.stderr,
        )
        sys.exit(1)
    print(
        f"[WARN] {homebrew_path}: all-zero template sha256 placeholders present for {len(placeholder_targets)} targets (allowed outside strict mode)"
    )

print(f"[PASS] Homebrew formula version, URLs, sha256 entries, binaries, and test block verified ({homebrew_path})")

# Verify README.md points Homebrew users to published release asset, never broken main template
readme_text = pathlib.Path("README.md").read_text(encoding="utf-8")
if broken_main_formula_url in readme_text:
    print(
        "[FAIL] README.md must not instruct users to install the main-branch template oz.rb with placeholder checksums",
        file=sys.stderr,
    )
    sys.exit(1)
if "https://github.com/Gaalix/git-ozone/releases/latest/download/oz.rb" not in readme_text:
    print(
        "[FAIL] README.md missing release asset Homebrew formula URL (releases/latest/download/oz.rb)",
        file=sys.stderr,
    )
    sys.exit(1)
print("[PASS] README.md Homebrew install instructions reference published release asset oz.rb")

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
  ruby -c "${HOMEBREW_PATH}" >/dev/null
  pass "ruby -c ${HOMEBREW_PATH}"
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

if [[ -n "$EXPECTED_VERSION" ]]; then
  PKG_VERSION="$EXPECTED_VERSION"
else
  PKG_VERSION="$(python3 -B -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')"
fi

# 5. Functional test: install.sh --dry-run
dry_run_out="$(./packaging/install.sh --version "$PKG_VERSION" --prefix /tmp/oz-test-prefix --dry-run)"
printf '%s\n' "$dry_run_out" | grep -Fq "oz-v${PKG_VERSION}-" || fail "install.sh --dry-run missing archive name"
printf '%s\n' "$dry_run_out" | grep -Fq "SHA256SUMS.txt" || fail "install.sh --dry-run missing SHA256SUMS.txt"
printf '%s\n' "$dry_run_out" | grep -Fq "/tmp/oz-test-prefix/bin" || fail "install.sh --dry-run missing install dir"
pass "packaging/install.sh --dry-run"

# 6. Functional test: build-deb.sh --dry-run (amd64 + arm64) and real dpkg-deb build test
for deb_arch in amd64 arm64; do
  deb_dry="$(./packaging/deb/build-deb.sh --version "$PKG_VERSION" --arch "$deb_arch" --dry-run)"
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
  TMP_DIRS+=("$deb_tmp")
  mkdir -p "${deb_tmp}/bins" "${deb_tmp}/out"
  for b in oz git-ozone oz-vfsd; do
    printf '#!/bin/sh\necho "%s stub"\n' "$b" > "${deb_tmp}/bins/${b}"
    chmod 0755 "${deb_tmp}/bins/${b}"
  done
  ./packaging/deb/build-deb.sh \
    --version "$PKG_VERSION" \
    --arch amd64 \
    --bin-dir "${deb_tmp}/bins" \
    --out-dir "${deb_tmp}/out" >/dev/null

  deb_file="${deb_tmp}/out/ozone_${PKG_VERSION}_amd64.deb"
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

# 7. Functional self-test of strict placeholder rejection & manifest population automation
#    (runs when validating default template manifests in non-strict mode)
if [[ "$ALLOW_PLACEHOLDERS" -eq 1 && "$CUSTOM_MANIFESTS" -eq 0 && -z "$CHECKSUMS_PATH" ]]; then
  # 7a. Verify --strict rejects checked-in all-zero template placeholders
  if grep -Fq "0000000000000000000000000000000000000000000000000000000000000000" packaging/homebrew/oz.rb packaging/winget/OzoneVCS.Ozone.installer.yaml; then
    if ./packaging/validate.sh --strict >/dev/null 2>&1; then
      fail "validate.sh --strict unexpectedly passed on all-zero placeholder manifests"
    fi
    pass "validate.sh --strict rejects all-zero placeholder manifests"
  fi

  # 7b. Populate manifests from synthetic SHA256SUMS.txt and verify strict validation passes
  pop_tmp="$(mktemp -d)"
  TMP_DIRS+=("$pop_tmp")
  cat > "${pop_tmp}/SHA256SUMS.txt" <<EOF
1111111111111111111111111111111111111111111111111111111111111111  oz-v${PKG_VERSION}-aarch64-apple-darwin.tar.gz
2222222222222222222222222222222222222222222222222222222222222222  oz-v${PKG_VERSION}-x86_64-apple-darwin.tar.gz
3333333333333333333333333333333333333333333333333333333333333333  oz-v${PKG_VERSION}-aarch64-unknown-linux-gnu.tar.gz
4444444444444444444444444444444444444444444444444444444444444444  oz-v${PKG_VERSION}-x86_64-unknown-linux-gnu.tar.gz
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  oz-v${PKG_VERSION}-x86_64-pc-windows-msvc.zip
EOF
  python3 -B ./packaging/populate-manifests.py \
    --version "$PKG_VERSION" \
    --checksums "${pop_tmp}/SHA256SUMS.txt" \
    --out-dir "${pop_tmp}/out" >/dev/null

  ./packaging/validate.sh \
    --strict \
    --version "$PKG_VERSION" \
    --homebrew "${pop_tmp}/out/oz.rb" \
    --winget-dir "${pop_tmp}/out/winget" \
    --checksums "${pop_tmp}/SHA256SUMS.txt" >/dev/null
  pass "packaging/populate-manifests.py end-to-end population & --strict checksum verification"
fi

echo "=== All packaging validations passed ==="
