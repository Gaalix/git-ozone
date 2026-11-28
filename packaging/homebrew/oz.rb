# typed: false
# frozen_string_literal: true

# Template Homebrew formula for Ozone (oz / git-ozone / oz-vfsd).
#
# Real SHA256 checksums are populated automatically from `SHA256SUMS.txt` by
# `.github/workflows/release.yml` (`packaging/populate-manifests.py`) and
# published as the `oz.rb` release asset on every tagged GitHub Release.
#
# Install from the latest published GitHub Release:
#   curl -fsSLO https://github.com/Gaalix/git-ozone/releases/latest/download/oz.rb
#   brew install --formula ./oz.rb
class Oz < Formula
  desc "Pure-Rust CoW Git client and multi-agent workspace engine"
  homepage "https://github.com/Gaalix/git-ozone"
  version "1.4.5"
  license any_of: ["MIT", "Apache-2.0"]

  # Template checksums are populated from SHA256SUMS.txt by .github/workflows/release.yml.
  on_macos do
    on_arm do
      url "https://github.com/Gaalix/git-ozone/releases/download/v#{version}/oz-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_intel do
      url "https://github.com/Gaalix/git-ozone/releases/download/v#{version}/oz-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Gaalix/git-ozone/releases/download/v#{version}/oz-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_intel do
      url "https://github.com/Gaalix/git-ozone/releases/download/v#{version}/oz-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "oz"
    bin.install "git-ozone"
    bin.install "oz-vfsd"
  end

  def caveats
    <<~EOS
      Ozone installs three binaries:
        - oz         (primary CLI)
        - git-ozone  (enables `git ozone ...` subcommand alias)
        - oz-vfsd    (unprivileged FUSE3 / VFS CoW daemon)

      Run `oz doctor` to verify filesystem CoW support, SSH signing keys, and GitHub auth.
      On Linux ext4 filesystems, install `fuse3` so `oz-vfsd` can mount zero-copy workspaces.
    EOS
  end

  test do
    require "json"

    version_out = shell_output("#{bin}/oz --version --json")
    version_json = JSON.parse(version_out)
    assert_equal "oz.v1", version_json["schema"]
    assert_equal true, version_json["ok"]
    assert_equal version.to_s, version_json.dig("data", "version")

    doctor_out = shell_output("#{bin}/git-ozone doctor --json")
    doctor_json = JSON.parse(doctor_out)
    assert_equal "oz.v1", doctor_json["schema"]
    assert_equal true, doctor_json["ok"]
  end
end
