# Ozone Debian / Ubuntu APT Repository & `.deb` Packaging Guide

Ozone ships architecture-specific `.deb` packages (`amd64` and `arm64`) on every GitHub Release. Because `oz-vfsd` speaks the Linux kernel FUSE3 protocol directly over `/dev/fuse` in pure Rust, the package has **zero `libfuse3-dev` or OpenSSL runtime dependencies** and only declares `Recommends: fuse3` (for `fusermount3`).

---

## 1. Direct `.deb` Installation (No APT Repo Required)

Download the matching `.deb` and `SHA256SUMS.txt` from the [GitHub Releases](https://github.com/ozone-vcs/ozone/releases) page:

```bash
VERSION=1.0.0
ARCH="$(dpkg --print-architecture)" # amd64 or arm64

curl -fsSLO "https://github.com/ozone-vcs/ozone/releases/download/v${VERSION}/ozone_${VERSION}_${ARCH}.deb"
curl -fsSLO "https://github.com/ozone-vcs/ozone/releases/download/v${VERSION}/SHA256SUMS.txt"

grep "ozone_${VERSION}_${ARCH}.deb" SHA256SUMS.txt | sha256sum -c -
sudo apt-get install -y "./ozone_${VERSION}_${ARCH}.deb"
oz doctor
```

---

## 2. Building `.deb` Packages Locally

Use [`build-deb.sh`](./build-deb.sh) after compiling release binaries:

```bash
cargo build --release --locked -p oz-cli -p oz-vfsd --bin oz --bin git-ozone --bin oz-vfsd
./packaging/deb/build-deb.sh \
  --version 1.0.0 \
  --arch amd64 \
  --bin-dir target/release \
  --out-dir dist
```

Installed files:
- `/usr/bin/oz`
- `/usr/bin/git-ozone`
- `/usr/bin/oz-vfsd`
- `/usr/share/doc/ozone/copyright` (`MIT OR Apache-2.0`)
- `/usr/share/doc/ozone/LICENSE-MIT`
- `/usr/share/doc/ozone/LICENSE-APACHE`

---

## 3. Publishing an APT Repository (`apt.ozone-vcs.dev`)

### Prerequisites (Maintainer Host / CI)

```bash
sudo apt-get install -y dpkg-dev apt-utils gnupg
```

### Step A: Generate or Import the Release Signing Key

```bash
gpg --batch --passphrase '' --quick-generate-key \
  "Ozone APT Archive Signing Key <maintainers@ozone-vcs.dev>" ed25519 sign 3y

# Export the ASCII-armored public key for users
gpg --armor --export "maintainers@ozone-vcs.dev" > ozone-archive-keyring.asc
```

Store the private signing key in CI secrets (e.g. `APT_GPG_PRIVATE_KEY`) and publish `ozone-archive-keyring.asc` at `https://apt.ozone-vcs.dev/ozone-archive-keyring.asc`.

### Step B: Assemble Repository Tree & Metadata

Place `ozone_<version>_amd64.deb` and `ozone_<version>_arm64.deb` into `apt-repo/pool/main/o/ozone/`:

```bash
REPO_DIR="./apt-repo"
mkdir -p \
  "${REPO_DIR}/pool/main/o/ozone" \
  "${REPO_DIR}/dists/stable/main/binary-amd64" \
  "${REPO_DIR}/dists/stable/main/binary-arm64"

cp dist/ozone_*_amd64.deb dist/ozone_*_arm64.deb "${REPO_DIR}/pool/main/o/ozone/"

(
  cd "${REPO_DIR}"
  for arch in amd64 arm64; do
    dpkg-scanpackages --arch "${arch}" pool/main > "dists/stable/main/binary-${arch}/Packages"
    gzip -9c "dists/stable/main/binary-${arch}/Packages" > "dists/stable/main/binary-${arch}/Packages.gz"
  done

  apt-ftparchive \
    -o APT::FTPArchive::Release::Origin="Ozone VCS" \
    -o APT::FTPArchive::Release::Label="Ozone APT Repository" \
    -o APT::FTPArchive::Release::Suite="stable" \
    -o APT::FTPArchive::Release::Codename="stable" \
    -o APT::FTPArchive::Release::Architectures="amd64 arm64" \
    -o APT::FTPArchive::Release::Components="main" \
    release dists/stable > dists/stable/Release

  gpg --default-key "maintainers@ozone-vcs.dev" -abs -o dists/stable/Release.gpg dists/stable/Release
  gpg --default-key "maintainers@ozone-vcs.dev" --clearsign -o dists/stable/InRelease dists/stable/Release
)
```

Sync `${REPO_DIR}/` (and `ozone-archive-keyring.asc`) to your static object store / CDN (GitHub Pages, Cloudflare R2, or S3 + CloudFront).

---

## 4. End-User APT Installation

```bash
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://apt.ozone-vcs.dev/ozone-archive-keyring.asc \
  | sudo gpg --dearmor -o /etc/apt/keyrings/ozone-archive-keyring.gpg
sudo chmod a+r /etc/apt/keyrings/ozone-archive-keyring.gpg

echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/ozone-archive-keyring.gpg] https://apt.ozone-vcs.dev stable main" \
  | sudo tee /etc/apt/sources.list.d/ozone.list > /dev/null

sudo apt-get update
sudo apt-get install -y ozone
oz doctor
```
