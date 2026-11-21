#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

tag=${1:?usage: homebrew-formula.sh <tag> <SHA256SUMS>}
sums=${2:?usage: homebrew-formula.sh <tag> <SHA256SUMS>}
base=${KOIDRIVE_RELEASE_BASE_URL:-https://github.com/koidrive/koidrive/releases}

[[ $tag =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-(alpha|rc)\.(0|[1-9][0-9]*))?$ ]] || { echo "::error::$tag is not a vX.Y.Z, vX.Y.Z-alpha.N or vX.Y.Z-rc.N tag." >&2; exit 1; }
[ -f "$sums" ] || { echo "::error::$sums does not exist." >&2; exit 1; }

sha() {
    awk -v name="koidrive-$1.tar.gz" '$2 == name { print $1 }' "$sums"
}

missing=
for target in aarch64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    [[ $(sha "$target") =~ ^[0-9a-f]{64}$ ]] || missing="$missing koidrive-$target.tar.gz"
done
if [ -n "$missing" ]; then
    echo "::error::$sums needs one SHA-256 for each tarball the formula installs, and has none or several for:$missing" >&2
    exit 1
fi

release() {
    echo "      url \"$base/download/$tag/koidrive-$1.tar.gz\""
    echo "      sha256 \"$(sha "$1")\""
}

version=
if [[ $tag == *-* ]]; then
    version=$'\n'"  version \"${tag#v}\""
fi

cat <<EOF
class Koidrive < Formula
  desc "File sync you deploy to your own Cloudflare account"
  homepage "https://koidrive.com"$version
  license all_of: ["Apache-2.0", "AGPL-3.0-or-later"]

  on_macos do
    depends_on arch: :arm64

    on_arm do
$(release aarch64-apple-darwin)
    end
  end

  on_linux do
    on_arm do
$(release aarch64-unknown-linux-musl)
    end
    on_intel do
$(release x86_64-unknown-linux-musl)
    end
  end

  def install
    bin.install "koi"
    prefix.install "THIRD-PARTY-NOTICES"
  end

  def caveats
    <<~EOS
      To set up Koidrive on this device, run:
        koi setup
    EOS
  end

  test do
    assert_equal "koi #{version}", shell_output("#{bin}/koi --version").strip
  end
end
EOF
