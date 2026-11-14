#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
if [ "${BASH_VERSINFO[0]}" -lt 5 ]; then
    printf 'FAIL the cask lifecycle fixture requires Bash 5 or newer; got %s\n' "$BASH_VERSION" >&2
    exit 2
fi
trap 'printf "FAIL %s:%s status=%s pipeline=%s version=%s command=%s\n" "${BASH_SOURCE[0]}" "$LINENO" "$?" "${PIPESTATUS[*]}" "${version:-not-started}" "$BASH_COMMAND" >&2' ERR

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tap=koidrive/tap
token=$tap/koidrive
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_ENV_HINTS=1

if brew tap | grep -qxF "$tap" || [ -e "$(brew --prefix)/bin/koi" ] || [ -n "$(brew list --formula --versions koidrive 2>/dev/null)" ] || [ -n "$(brew list --cask --versions koidrive 2>/dev/null)" ]; then
    echo 'FAIL the cask fixture needs Homebrew without the Koidrive tap, formula or cask installed' >&2
    exit 1
fi

tmp=$(mktemp -d "${HOMEBREW_TEMP:-${TMPDIR:-/tmp}}/koidrive-cask.XXXXXX")
fixture_id=dev.koidrive.KoidriveCaskFixture.$(uuidgen | tr '[:upper:]' '[:lower:]')
cleanup() {
    if [ -n "$(brew list --cask --versions "$token" 2>/dev/null)" ]; then
        brew uninstall --cask "$token" > /dev/null 2>&1 || true
    fi
    brew untap "$tap" > /dev/null 2>&1 || true
    rm -rf "$tmp"
}
trap cleanup EXIT
export XDG_CONFIG_HOME=$tmp/config
mkdir -p "$tmp/cache"
[ ! -d "$(brew --cache)/api" ] || cp -Rp "$(brew --cache)/api" "$tmp/cache/api"
export HOMEBREW_CACHE=$tmp/cache HOMEBREW_LOGS=$tmp/logs
releases=$tmp/github.com/koidrive/koidrive/releases
mkdir -p "$tmp/Applications" "$tmp/image/Koidrive.app/Contents/Helpers"
cat > "$tmp/zap-helper" <<EOF
#!/bin/sh
[ "\$1" = --unregister-login-item ] || exit 1
[ -d "$tmp/Applications/Koidrive.app" ] || exit 1
printf 'unregistered\n' >> "$tmp/zap.log"
EOF
chmod +x "$tmp/zap-helper"
brew tap-new --no-git "$tap"
repository=$(brew --repository "$tap")
mkdir -p "$repository/Casks"
build=100
for version in 0.1.0-alpha.2 0.1.0-alpha.3 0.1.0-rc.1 0.1.0; do
    build=$((build + 1))
    tag=v$version
    release=$releases/download/$tag
    mkdir -p "$release"
    cat > "$tmp/image/Koidrive.app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>$fixture_id</string>
<key>CFBundleName</key><string>Koidrive Cask Fixture</string>
<key>CFBundleExecutable</key><string>Koidrive</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>${version%%-*}</string>
<key>CFBundleVersion</key><string>$build</string>
<key>KoidriveVersion</key><string>$version</string>
</dict></plist>
EOF
    cat > "$tmp/image/Koidrive.app/Contents/Helpers/koi" <<EOF
#!/bin/sh
printf 'koi %s\n' '$version'
EOF
    hdiutil create -volname Koidrive -srcfolder "$tmp/image" -format UDZO "$release/Koidrive-$version-arm64.dmg"
    (cd "$release" && shasum -a 256 ./*.dmg | sed 's#  ./#  #' > SHA256SUMS)
    KOIDRIVE_RELEASE_BASE_URL="file://$releases" bash "$root/scripts/homebrew-cask.sh" "$tag" "$release/SHA256SUMS" \
        > "$repository/Casks/koidrive.rb"
    if [ "$build" -eq 101 ]; then
        brew trust --cask "$token"
        brew style --cask "$token"
        brew audit --cask "$token"
    fi
    python3 - "$repository/Casks/koidrive.rb" "$tmp" "$fixture_id" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
source = path.read_text()
replacements = {
    '"#{appdir}/Koidrive.app/Contents/MacOS/Koidrive"': f'"{sys.argv[2]}/zap-helper"',
    '"~/Library/Application Support/Koidrive"': f'"{sys.argv[2]}/app-state"',
    '"~/Library/Preferences/dev.koidrive.Koidrive.plist"': f'"{sys.argv[2]}/preferences.plist"',
}
for original, replacement in replacements.items():
    if source.count(original) != 1:
        raise RuntimeError(f'The fixture must isolate {original}')
    source = source.replace(original, replacement)
source = source.replace('dev.koidrive.Koidrive', sys.argv[3])
path.write_text(source)
PY
    if [ "$build" -eq 101 ]; then
        brew install --cask --appdir="$tmp/Applications" "$token"
    else
        /usr/libexec/PlistBuddy -c "Set CFBundleShortVersionString ${version%%-*}" -c "Set CFBundleVersion $build" -c "Set KoidriveVersion $version" \
            "$tmp/Applications/Koidrive.app/Contents/Info.plist"
        plain=$(brew outdated --cask "$token") || true
        if grep -q koidrive <<< "$plain"; then
            echo "FAIL Homebrew replaces a self-updating app whose bundle already has the cask's version" >&2
            exit 1
        fi
        status=0
        outdated=$(brew outdated --cask --greedy "$token") || status=$?
        if [ "$status" -ne 1 ] || ! grep -q koidrive <<< "$outdated"; then
            echo "FAIL Homebrew does not consider $version an app upgrade" >&2
            exit 1
        fi
        brew upgrade --cask --appdir="$tmp/Applications" "$token"
    fi
    plist=$tmp/Applications/Koidrive.app/Contents/Info.plist
    [ "$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$plist")" = "${version%%-*}" ] || { echo 'FAIL the installed short version differs from release metadata' >&2; exit 1; }
    [ "$(/usr/libexec/PlistBuddy -c 'Print CFBundleVersion' "$plist")" = "$build" ] || { echo 'FAIL the app did not upgrade to the new build' >&2; exit 1; }
    [ "$(/usr/libexec/PlistBuddy -c 'Print KoidriveVersion' "$plist")" = "$version" ] || { echo 'FAIL the installed app version differs from the cask' >&2; exit 1; }
    [ "$(/bin/sh "$(brew --prefix)/bin/koi" --version)" = "koi $version" ] || { echo 'FAIL the linked CLI version differs from the app' >&2; exit 1; }
    [ ! -e "$tmp/zap.log" ] || { echo 'FAIL install or upgrade ran login-item cleanup' >&2; exit 1; }
    [ -z "$(brew list --formula --versions koidrive 2>/dev/null)" ] || { echo 'FAIL the cask installed a separate CLI formula' >&2; exit 1; }
    echo "PASS cask installs app and bundled CLI $version without a formula dependency, plain outdated skips it once the app updated itself, and --greedy upgrades it"
done
brew reinstall --cask --appdir="$tmp/Applications" "$token"
[ ! -e "$tmp/zap.log" ] || { echo 'FAIL reinstall ran login-item cleanup' >&2; exit 1; }
[ "$(/bin/sh "$(brew --prefix)/bin/koi" --version)" = "koi $version" ] || { echo 'FAIL reinstall lost the bundled CLI link' >&2; exit 1; }
echo 'PASS reinstall leaves login-item cleanup alone and retains the bundled CLI link'
brew uninstall --cask "$token"
[ ! -e "$tmp/Applications/Koidrive.app" ] || { echo 'FAIL the app remains after cask uninstall' >&2; exit 1; }
[ ! -L "$(brew --prefix)/bin/koi" ] || { echo 'FAIL the bundled CLI link remains after cask uninstall' >&2; exit 1; }
[ ! -e "$tmp/zap.log" ] || { echo 'FAIL plain uninstall ran login-item cleanup' >&2; exit 1; }
echo 'PASS plain uninstall removes the app and CLI link without login-item cleanup'
brew install --cask --appdir="$tmp/Applications" "$token"
brew uninstall --cask --zap "$token"
[ "$(grep -cx unregistered "$tmp/zap.log")" = 1 ] || { echo 'FAIL zap did not unregister once before removing the app' >&2; exit 1; }
[[ ! -e "$tmp/Applications/Koidrive.app" && ! -L "$(brew --prefix)/bin/koi" ]] || { echo 'FAIL zap left the app or CLI link' >&2; exit 1; }
echo 'PASS zap unregisters before app removal'
echo 'Passed 7 Homebrew cask install tests'
