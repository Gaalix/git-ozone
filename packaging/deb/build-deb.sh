#!/usr/bin/env bash
# Copyright (c) 2026 Ozone Contributors
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Build a Debian / Ubuntu (.deb) package for Ozone (oz, git-ozone, oz-vfsd).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEFAULT_REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

VERSION=""
ARCH=""
BIN_DIR=""
OUT_DIR="./dist"
REPO_ROOT="${DEFAULT_REPO_ROOT}"
DRY_RUN=0

usage() {
  cat <<'EOF'
Usage: build-deb.sh [OPTIONS]

Build an Ubuntu/Debian .deb package containing oz, git-ozone, and oz-vfsd.

Options:
  --version <VER>     Package version (e.g. 1.1.0; defaults to Cargo.toml version)
  --arch <ARCH>       Debian architecture: amd64 or arm64 (defaults to host arch)
  --bin-dir <DIR>     Directory containing built oz, git-ozone, and oz-vfsd binaries
  --out-dir <DIR>     Output directory for the .deb archive (default: ./dist)
  --repo-root <DIR>   Repository root containing Cargo.toml and LICENSE-* files
  --dry-run           Stage package metadata and verify layout without requiring binaries
  -h, --help          Show this help message
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

detect_host_deb_arch() {
  case "$(uname -m)" in
    x86_64|amd64) echo "amd64" ;;
    aarch64|arm64) echo "arm64" ;;
    *) die "unsupported host architecture '$(uname -m)'; specify --arch amd64|arm64" ;;
  esac
}

detect_cargo_version() {
  local cargo_toml="${REPO_ROOT}/Cargo.toml"
  [[ -f "$cargo_toml" ]] || die "Cargo.toml not found at ${cargo_toml}; specify --version"
  awk '
    /^\[workspace\.package\]/ { in_ws = 1; next }
    /^\[/ && in_ws { in_ws = 0 }
    in_ws && /^version[[:space:]]*=/ {
      gsub(/^version[[:space:]]*=[[:space:]]*"/, "")
      gsub(/".*$/, "")
      print
      exit
    }
  ' "$cargo_toml"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      [[ $# -ge 2 ]] || die "--version requires an argument"
      VERSION="${2#v}"
      shift 2
      ;;
    --arch)
      [[ $# -ge 2 ]] || die "--arch requires an argument"
      ARCH="$2"
      shift 2
      ;;
    --bin-dir)
      [[ $# -ge 2 ]] || die "--bin-dir requires an argument"
      BIN_DIR="$2"
      shift 2
      ;;
    --out-dir)
      [[ $# -ge 2 ]] || die "--out-dir requires an argument"
      OUT_DIR="$2"
      shift 2
      ;;
    --repo-root)
      [[ $# -ge 2 ]] || die "--repo-root requires an argument"
      REPO_ROOT="$2"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=1
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

if [[ -z "$VERSION" ]]; then
  VERSION="$(detect_cargo_version)"
fi
[[ -n "$VERSION" ]] || die "could not determine version; pass --version"
if ! [[ "$VERSION" =~ ^[0-9][0-9A-Za-z.+~-]*$ ]]; then
  die "invalid Debian version '${VERSION}'"
fi

if [[ -z "$ARCH" ]]; then
  ARCH="$(detect_host_deb_arch)"
fi
case "$ARCH" in
  amd64|arm64) ;;
  *) die "unsupported Debian architecture '${ARCH}' (expected amd64 or arm64)" ;;
esac

if [[ "$DRY_RUN" -eq 0 && -z "$BIN_DIR" ]]; then
  die "--bin-dir is required unless --dry-run is set"
fi

STAGE_ROOT="$(mktemp -d)"
trap 'rm -rf "$STAGE_ROOT"' EXIT

PKG_ROOT="${STAGE_ROOT}/ozone_${VERSION}_${ARCH}"
mkdir -p \
  "${PKG_ROOT}/DEBIAN" \
  "${PKG_ROOT}/usr/bin" \
  "${PKG_ROOT}/usr/share/doc/ozone"

BINARIES=(oz git-ozone oz-vfsd)

if [[ "$DRY_RUN" -eq 1 && -z "$BIN_DIR" ]]; then
  for bin_name in "${BINARIES[@]}"; do
    printf '#!/bin/sh\nexit 0\n' > "${PKG_ROOT}/usr/bin/${bin_name}"
    chmod 0755 "${PKG_ROOT}/usr/bin/${bin_name}"
  done
else
  [[ -d "$BIN_DIR" ]] || die "binary directory does not exist: ${BIN_DIR}"
  for bin_name in "${BINARIES[@]}"; do
    src="${BIN_DIR}/${bin_name}"
    [[ -f "$src" ]] || die "required binary missing: ${src}"
    install -m 0755 "$src" "${PKG_ROOT}/usr/bin/${bin_name}"
  done
fi

cat > "${PKG_ROOT}/usr/share/doc/ozone/copyright" <<'EOF'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: ozone
Upstream-Contact: Ozone Contributors <maintainers@ozone-vcs.dev>
Source: https://github.com/Gaalix/git-ozone

Files: *
Copyright: 2026 Ozone Contributors
License: MIT or Apache-2.0

License: MIT
 Permission is hereby granted, free of charge, to any person obtaining a copy
 of this software and associated documentation files (the "Software"), to deal
 in the Software without restriction, including without limitation the rights
 to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 copies of the Software, and to permit persons to whom the Software is
 furnished to do so, subject to the following conditions:
 .
 The above copyright notice and this permission notice shall be included in all
 copies or substantial portions of the Software.
 .
 THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 SOFTWARE.

License: Apache-2.0
 Licensed under the Apache License, Version 2.0 (the "License");
 you may not use this file except in compliance with the License.
 You may obtain a copy of the License at
 .
     https://www.apache.org/licenses/LICENSE-2.0
 .
 Unless required by applicable law or agreed to in writing, software
 distributed under the License is distributed on an "AS IS" BASIS,
 WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 See the License for the specific language governing permissions and
 limitations under the License.
 .
 On Debian systems, the complete text of the Apache License, Version 2.0
 can be found in `/usr/share/common-licenses/Apache-2.0'.
EOF
chmod 0644 "${PKG_ROOT}/usr/share/doc/ozone/copyright"

for lic_file in LICENSE-MIT LICENSE-APACHE; do
  if [[ -f "${REPO_ROOT}/${lic_file}" ]]; then
    install -m 0644 "${REPO_ROOT}/${lic_file}" "${PKG_ROOT}/usr/share/doc/ozone/${lic_file}"
  fi
done

INSTALLED_SIZE="$(du -sk "${PKG_ROOT}/usr" | awk '{print $1}')"

cat > "${PKG_ROOT}/DEBIAN/control" <<EOF
Package: ozone
Version: ${VERSION}
Architecture: ${ARCH}
Maintainer: Ozone Contributors <maintainers@ozone-vcs.dev>
Installed-Size: ${INSTALLED_SIZE}
Section: vcs
Priority: optional
Homepage: https://github.com/Gaalix/git-ozone
Recommends: fuse3
Suggests: git
Description: Pure-Rust CoW Git client and multi-agent workspace engine
 Ozone (oz / git-ozone / oz-vfsd) delivers instant copy-on-write workspaces on
 APFS, Btrfs, XFS, ReFS, ext4 (via unprivileged pure-Rust FUSE3 oz-vfsd), and
 Windows NTFS (via ProjFS), with warm build caches, stage-free hunk commits,
 sub-10ms MVCC undo, collision radar, and native GitHub stacked PRs.
EOF
chmod 0644 "${PKG_ROOT}/DEBIAN/control"

DEB_NAME="ozone_${VERSION}_${ARCH}.deb"
DEB_PATH="${OUT_DIR}/${DEB_NAME}"

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "[dry-run] Validated Debian staging tree for ${DEB_NAME}"
  echo "[dry-run] Output target: ${DEB_PATH}"
  echo "--- DEBIAN/control ---"
  cat "${PKG_ROOT}/DEBIAN/control"
  echo "--- staged files ---"
  (cd "${PKG_ROOT}" && find . -type f | LC_ALL=C sort)
  exit 0
fi

command -v dpkg-deb >/dev/null 2>&1 || die "dpkg-deb is required to build .deb archives"
mkdir -p "${OUT_DIR}"

if dpkg-deb --help 2>&1 | grep -Fq -- '--root-owner-group'; then
  dpkg-deb --build --root-owner-group "${PKG_ROOT}" "${DEB_PATH}"
else
  dpkg-deb --build "${PKG_ROOT}" "${DEB_PATH}"
fi

echo "Built Debian package: ${DEB_PATH}"
dpkg-deb --info "${DEB_PATH}"
dpkg-deb --contents "${DEB_PATH}"
