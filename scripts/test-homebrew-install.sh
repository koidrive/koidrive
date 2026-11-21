#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

binary=${1:?usage: test-homebrew-install.sh <koi binary> <target>}
target=${2:?usage: test-homebrew-install.sh <koi binary> <target>}
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tap=koidrive/local
formula=$tap/koidrive
tmp=$(mktemp -d)
cleanup() {
    brew uninstall --formula --force "$formula" > /dev/null 2>&1 || true
    brew untap "$tap" > /dev/null 2>&1 || true
    rm -rf "$tmp"
}
trap cleanup EXIT
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_ENV_HINTS=1

if command -v sha256sum > /dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
reported=$("$binary" --version)
version=${reported#koi }
tag=v$version
upgrade=v999.0.0
releases=$tmp/github.com/koidrive/koidrive/releases
mkdir -p "$tmp/package" "$tmp/other"
notices=$(dirname "$binary")/THIRD-PARTY-NOTICES
[ -f "$notices" ] || { echo "FAIL $notices is missing; the release archive must carry THIRD-PARTY-NOTICES next to koi" >&2; exit 1; }
cp "$binary" "$tmp/package/koi"
printf '#!/bin/sh\nexit 1\n' > "$tmp/other/koi"
cp "$notices" "$tmp/package/THIRD-PARTY-NOTICES"
cp "$notices" "$tmp/other/THIRD-PARTY-NOTICES"
for each in "$tag" "$upgrade"; do
    release=$releases/download/$each
    mkdir -p "$release"
    for platform in aarch64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
        if [ "$platform" = "$target" ]; then source=$tmp/package; else source=$tmp/other; fi
        tar -czf "$release/koidrive-$platform.tar.gz" -C "$source" koi THIRD-PARTY-NOTICES
    done
    (cd "$release" && "${sha256[@]}" koidrive-*.tar.gz > SHA256SUMS)
done
grep -q "  koidrive-$target.tar.gz$" "$releases/download/$tag/SHA256SUMS" || { echo "FAIL $target is not one of the targets the formula installs" >&2; exit 1; }

render() {
    KOIDRIVE_RELEASE_BASE_URL="file://$releases" bash "$root/scripts/homebrew-formula.sh" "$1" "$releases/download/${2:-$1}/SHA256SUMS" \
        > "$(brew --repository "$tap")/Formula/koidrive.rb"
}

brew tap-new --no-git "$tap"
render "$tag"
brew style "$formula"
brew install "$formula"
brew test "$formula"
installed=$("$(brew --prefix)/bin/koi" --version)
[ "$installed" = "koi $version" ] || { echo "FAIL the installed koi --version prints '$installed', expected 'koi $version'" >&2; exit 1; }
cmp -s "$notices" "$(brew --prefix koidrive)/THIRD-PARTY-NOTICES" || { echo "FAIL brew install koidrive did not install THIRD-PARTY-NOTICES into $(brew --prefix koidrive)" >&2; exit 1; }
for each in v1.2.3 v1.2.3-alpha.4 "$tag"; do
    render "$each" "$tag"
    brew style "$formula"
    brew audit --strict --formula "$formula"
done
render "$upgrade"
brew upgrade koidrive
brew cleanup koidrive
[ "$(brew list --versions koidrive)" = "koidrive ${upgrade#v}" ] || { echo "FAIL brew upgrade koidrive did not install ${upgrade#v}: $(brew list --versions koidrive)" >&2; exit 1; }
brew uninstall koidrive
[ ! -e "$(brew --prefix)/bin/koi" ] || { echo "FAIL koi is still in $(brew --prefix)/bin after brew uninstall" >&2; exit 1; }
echo "Installed, tested, upgraded and uninstalled koidrive $version for $target from a local tap."
