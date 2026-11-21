#!/usr/bin/env bash
# Copyright (c) 2026 Ozone Contributors
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Single-command installer for Ozone (oz, git-ozone, oz-vfsd) on Linux and macOS.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Gaalix/git-ozone/main/packaging/install.sh | bash
#   ./packaging/install.sh --version 1.0.0 --prefix "$HOME/.local" --dry-run

set -euo pipefail

DEFAULT_VERSION="1.0.0"
REPO="${OZ_REPO:-Gaalix/git-ozone}"
VERSION="${OZ_VERSION:-${DEFAULT_VERSION}}"
PREFIX="${OZ_INSTALL_PREFIX:-${HOME}/.local}"
DRY_RUN=0
RUN_DOCTOR=1

usage() {
  cat <<'EOF'
Ozone installer (Linux & macOS)

Installs `oz`, `git-ozone`, and `oz-vfsd` from verified GitHub Release tarballs.

Usage:
  install.sh [OPTIONS]

Options:
  --version <VER>   Version to install (e.g. 1.0.0, v1.0.0, or latest; default: 1.0.0)
  --prefix <DIR>    Installation prefix; binaries go into <DIR>/bin (default: ~/.local)
  --repo <O/R>      GitHub repository slug (default: Gaalix/git-ozone)
  --dry-run         Print resolved target, URLs, and install paths without downloading or writing
  --no-doctor       Skip running `oz doctor` after installation
  -h, --help        Show this help message

Environment variables:
  OZ_VERSION         Overrides default version
  OZ_INSTALL_PREFIX  Overrides default installation prefix
  OZ_REPO            Overrides default GitHub repository slug
EOF
}

log() {
  printf '[ozone-install] %s\n' "$*"
}

warn() {
  printf '[ozone-install] warning: %s\n' "$*" >&2
}

die() {
  printf '[ozone-install] error: %s\n' "$*" >&2
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      [[ $# -ge 2 ]] || die "--version requires an argument"
      VERSION="$2"
      shift 2
      ;;
    --prefix)
      [[ $# -ge 2 ]] || die "--prefix requires an argument"
      PREFIX="$2"
      shift 2
      ;;
    --repo)
      [[ $# -ge 2 ]] || die "--repo requires an argument"
      REPO="$2"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --no-doctor)
      RUN_DOCTOR=0
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1 (use --help for usage)"
      ;;
  esac
done

detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux)
      case "$arch" in
        x86_64|amd64) echo "x86_64-unknown-linux-gnu" ;;
        aarch64|arm64) echo "aarch64-unknown-linux-gnu" ;;
        *) die "unsupported Linux architecture: ${arch}" ;;
      esac
      ;;
    Darwin)
      case "$arch" in
        x86_64|amd64) echo "x86_64-apple-darwin" ;;
        arm64|aarch64) echo "aarch64-apple-darwin" ;;
        *) die "unsupported macOS architecture: ${arch}" ;;
      esac
      ;;
    *)
      die "unsupported operating system: ${os} (use packaging/install.ps1 on Windows)"
      ;;
  esac
}

download_file() {
  local url="$1"
  local dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 --retry 3 "$url" -o "$dest"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$dest" "$url"
  else
    die "neither curl nor wget is installed; install one to download release artifacts"
  fi
}

resolve_latest_version() {
  local api_url="https://api.github.com/repos/${REPO}/releases/latest"
  local body=""
  if command -v curl >/dev/null 2>&1; then
    body="$(curl -fsSL --proto '=https' --tlsv1.2 "$api_url")"
  elif command -v wget >/dev/null 2>&1; then
    body="$(wget -qO- "$api_url")"
  else
    die "neither curl nor wget is installed"
  fi

  local tag
  tag="$(printf '%s\n' "$body" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)"
  [[ -n "$tag" ]] || die "failed to resolve latest release tag from ${api_url}"
  printf '%s' "${tag#v}"
}

compute_sha256() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  else
    die "neither sha256sum nor shasum is installed; cannot verify archive checksum"
  fi
}

TARGET="$(detect_target)"

if [[ "$VERSION" == "latest" ]]; then
  if [[ "$DRY_RUN" -eq 1 ]]; then
    VERSION="${DEFAULT_VERSION}"
    log "dry-run: using default version ${VERSION} instead of querying GitHub API for 'latest'"
  else
    VERSION="$(resolve_latest_version)"
  fi
fi

CLEAN_VERSION="${VERSION#v}"
TAG="v${CLEAN_VERSION}"
PKG_NAME="oz-${TAG}-${TARGET}"
ARCHIVE_NAME="${PKG_NAME}.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"
ARCHIVE_URL="${BASE_URL}/${ARCHIVE_NAME}"
CHECKSUMS_URL="${BASE_URL}/SHA256SUMS.txt"
BIN_DIR="${PREFIX%/}/bin"

if [[ "$DRY_RUN" -eq 1 ]]; then
  log "dry-run mode enabled (no files will be downloaded or written)"
  log "target:       ${TARGET}"
  log "version:      ${CLEAN_VERSION} (${TAG})"
  log "archive url:  ${ARCHIVE_URL}"
  log "checksum url: ${CHECKSUMS_URL}"
  log "install dir:  ${BIN_DIR}"
  log "binaries:     oz, git-ozone, oz-vfsd"
  exit 0
fi

command -v tar >/dev/null 2>&1 || die "tar is required to unpack ${ARCHIVE_NAME}"

TMP_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

ARCHIVE_PATH="${TMP_DIR}/${ARCHIVE_NAME}"
CHECKSUMS_PATH="${TMP_DIR}/SHA256SUMS.txt"

log "Downloading ${ARCHIVE_NAME} (${TAG})..."
download_file "$ARCHIVE_URL" "$ARCHIVE_PATH"

log "Downloading SHA256SUMS.txt..."
download_file "$CHECKSUMS_URL" "$CHECKSUMS_PATH"

EXPECTED_SHA="$(awk -v name="$ARCHIVE_NAME" '$2 == name || $2 == ("*" name) { print $1; exit }' "$CHECKSUMS_PATH")"
[[ -n "$EXPECTED_SHA" ]] || die "checksum entry for ${ARCHIVE_NAME} not found in SHA256SUMS.txt"

ACTUAL_SHA="$(compute_sha256 "$ARCHIVE_PATH")"
if [[ "$ACTUAL_SHA" != "$EXPECTED_SHA" ]]; then
  die "SHA256 checksum mismatch for ${ARCHIVE_NAME} (expected ${EXPECTED_SHA}, got ${ACTUAL_SHA})"
fi
log "Verified SHA256 checksum (${ACTUAL_SHA})"

EXTRACT_DIR="${TMP_DIR}/extract"
mkdir -p "$EXTRACT_DIR"
tar -xzf "$ARCHIVE_PATH" -C "$EXTRACT_DIR"

SRC_DIR="${EXTRACT_DIR}/${PKG_NAME}"
if [[ ! -d "$SRC_DIR" ]]; then
  SRC_DIR="$EXTRACT_DIR"
fi

mkdir -p "$BIN_DIR"
for bin_name in oz git-ozone oz-vfsd; do
  src_bin="${SRC_DIR}/${bin_name}"
  [[ -f "$src_bin" ]] || die "expected binary '${bin_name}' not found inside ${ARCHIVE_NAME}"
  install -m 0755 "$src_bin" "${BIN_DIR}/${bin_name}"
done

log "Installed oz, git-ozone, and oz-vfsd into ${BIN_DIR}"

case ":${PATH}:" in
  *":${BIN_DIR}:"*) ;;
  *)
    warn "${BIN_DIR} is not currently in your PATH."
    printf '  Add it to your shell profile:\n'
    printf '    export PATH="%s:%s"\n' "$BIN_DIR" "\$PATH"
    ;;
esac

if [[ "$RUN_DOCTOR" -eq 1 ]]; then
  log "Running \`oz doctor\`..."
  if ! "${BIN_DIR}/oz" doctor; then
    warn "\`oz doctor\` reported actionable setup items; see output above."
  fi
fi

log "Ozone ${TAG} installation complete."
