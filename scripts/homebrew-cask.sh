#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

[ "$#" -eq 2 ] || { echo '::error::usage: homebrew-cask.sh <tag> <SHA256SUMS>' >&2; exit 1; }
tag=$1
sums=$2
base=${KOIDRIVE_RELEASE_BASE_URL:-https://github.com/koidrive/koidrive/releases}

[[ $tag =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-(alpha|rc)\.(0|[1-9][0-9]*))?$ ]] || { echo "::error::$tag is not a vX.Y.Z, vX.Y.Z-alpha.N or vX.Y.Z-rc.N tag." >&2; exit 1; }
[ -f "$sums" ] || { echo "::error::$sums does not exist." >&2; exit 1; }

asset=Koidrive-${tag#v}-arm64.dmg
sha=$(awk -v name="$asset" '$2 == name { if (NF != 2) exit 1; print $1 }' "$sums") \
    && [[ $sha =~ ^[0-9a-f]{64}$ ]] || { echo "::error::$sums needs exactly one SHA-256 for $asset." >&2; exit 1; }

cat <<EOF
cask "koidrive" do
  version "${tag#v}"
  sha256 "$sha"

  url "$base/download/$tag/Koidrive-#{version}-arm64.dmg"
  name "Koidrive"
  desc "File sync you deploy to your own Cloudflare account"
  homepage "https://koidrive.com/"

  auto_updates true
  depends_on arch: :arm64
  depends_on macos: :tahoe

  app "Koidrive.app"
  binary "#{appdir}/Koidrive.app/Contents/Helpers/koi"

  uninstall quit: "dev.koidrive.Koidrive"

  zap script: {
        executable: "#{appdir}/Koidrive.app/Contents/MacOS/Koidrive",
        args:       ["--unregister-login-item"],
        sudo:       false,
      },
      trash:  [
        "~/Library/Application Support/Koidrive",
        "~/Library/Preferences/dev.koidrive.Koidrive.plist",
      ]

  caveats <<~EOS
    To remove Koidrive with its background service, settings and sync state, run:
      koi uninstall
  EOS
end
EOF
