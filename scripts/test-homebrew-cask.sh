#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin"
cat > "$tmp/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[ -z "${GH_STUB_FAIL_LIST:-}" ] || { echo 'gh: Bad Gateway (HTTP 502)' >&2; exit 1; }
[ "$1 $2 $3 $4" = "api --paginate repos/$GH_REPO/releases --jq" ] || { echo "stub gh: unexpected $*" >&2; exit 2; }
for release in $GH_STUB_RELEASES; do
    jq -n --arg tag "${release%:*}" --argjson draft "$([ "${release#*:}" = draft ] && echo true || echo false)" \
        '{tag_name: $tag, draft: $draft, prerelease: false}'
done | jq -s . | jq -r "$5"
EOF
chmod +x "$tmp/bin/gh"
export PATH="$tmp/bin:$PATH" GH_REPO=koidrive/koidrive GH_STUB_RELEASES=
unset KOIDRIVE_RELEASE_BASE_URL
passed=0

pass() {
    echo "PASS $1"
    passed=$((passed + 1))
}
fail() {
    echo "FAIL $1" >&2
    exit 1
}
sums() {
    printf '%s  %s\n' \
        1111111111111111111111111111111111111111111111111111111111111111 koidrive-aarch64-apple-darwin.tar.gz \
        2222222222222222222222222222222222222222222222222222222222222222 Koidrive-0.1.0-alpha.3-arm64.dmg \
        3333333333333333333333333333333333333333333333333333333333333333 Koidrive-0.1.0-rc.1-arm64.dmg \
        4444444444444444444444444444444444444444444444444444444444444444 Koidrive-0.1.0-arm64.dmg
}
render() {
    bash "$root/scripts/homebrew-cask.sh" "$@"
}
expect_refused() {
    local message=$1 description=$2 output
    shift 2
    if output=$(render "$@" 2> "$tmp/error"); then
        fail "$description: the cask rendered"
    fi
    [ -z "$output" ] || fail "$description: a rejected cask wrote to stdout"
    grep '^::error::' "$tmp/error" | grep -qF "$message" || fail "$description: expected an error with '$message'"
    pass "$description"
}

sums > "$tmp/SHA256SUMS"
cat > "$tmp/expected.rb" <<'EOF'
cask "koidrive" do
  version "0.1.0-alpha.3"
  sha256 "2222222222222222222222222222222222222222222222222222222222222222"

  url "https://github.com/koidrive/koidrive/releases/download/v0.1.0-alpha.3/Koidrive-#{version}-arm64.dmg"
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
render v0.1.0-alpha.3 "$tmp/SHA256SUMS" > "$tmp/actual.rb"
diff -u "$tmp/expected.rb" "$tmp/actual.rb" || fail 'the alpha cask differs from the expected cask'
pass 'an alpha renders the exact cask with only app state in zap'
for version in 0.1.0-rc.1 0.1.0; do
    sums > "$tmp/SHA256SUMS"
    render "v$version" "$tmp/SHA256SUMS" > "$tmp/actual.rb"
    grep -qxF "  version \"$version\"" "$tmp/actual.rb" || fail "$version has no explicit version"
    grep -qxF "  url \"https://github.com/koidrive/koidrive/releases/download/v$version/Koidrive-#{version}-arm64.dmg\"" "$tmp/actual.rb" \
        || fail "$version does not name the exact DMG"
    sha=$(awk -v name="Koidrive-$version-arm64.dmg" '$2 == name { print $1 }' "$tmp/SHA256SUMS")
    grep -qxF "  sha256 \"$sha\"" "$tmp/actual.rb" || fail "$version uses another asset's checksum"
    pass "$version renders its explicit version and exact asset checksum"
done
KOIDRIVE_RELEASE_BASE_URL=file:///srv/releases render v0.1.0 "$tmp/SHA256SUMS" \
    | grep -qxF '  url "file:///srv/releases/download/v0.1.0/Koidrive-#{version}-arm64.dmg"' \
    || fail 'KOIDRIVE_RELEASE_BASE_URL does not replace the release URL'
pass 'KOIDRIVE_RELEASE_BASE_URL replaces the release URL'
render v0.1.0 "$tmp/SHA256SUMS" > "$tmp/first.rb"
render v0.1.0 "$tmp/SHA256SUMS" > "$tmp/second.rb"
cmp "$tmp/first.rb" "$tmp/second.rb" || fail 'rendering is not deterministic'
pass 'rendering the same release is deterministic'

for tag in v01.0.0 v0.01.0 v0.1.00 v0.1.0-alpha.01 0.1.0 v0.1 v0.1.0-beta.1 v0.1.0-alpha v0.1.0-alpha.1.2 v0.1.0+build v0.1.0/extra 'v0.1.0"' ''; do
    expect_refused 'is not a vX.Y.Z' "invalid tag '$tag' is refused" "$tag" "$tmp/SHA256SUMS"
done
expect_refused 'usage:' 'no arguments are refused'
expect_refused 'usage:' 'a missing checksum argument is refused' v0.1.0
expect_refused 'usage:' 'extra arguments are refused' v0.1.0 "$tmp/SHA256SUMS" extra
expect_refused 'does not exist' 'a missing checksum file is refused' v0.1.0 "$tmp/missing"
expect_refused 'does not exist' 'a directory is not a checksum file' v0.1.0 "$tmp"

asset=Koidrive-0.1.0-alpha.3-arm64.dmg
for substitute in Koidrive-0.1.0-alpha.2-arm64.dmg Koidrive-0.1.0-alpha.3-x86_64.dmg "$asset.zip" "./$asset"; do
    sums | sed "s|$asset|$substitute|" > "$tmp/SHA256SUMS"
    expect_refused "exactly one SHA-256 for $asset" "a checksum for $substitute cannot substitute for the exact DMG" v0.1.0-alpha.3 "$tmp/SHA256SUMS"
done
: > "$tmp/SHA256SUMS"
expect_refused "exactly one SHA-256 for $asset" 'an empty manifest is refused' v0.1.0-alpha.3 "$tmp/SHA256SUMS"
sums | grep -v "$asset" > "$tmp/SHA256SUMS"
expect_refused "exactly one SHA-256 for $asset" 'a missing DMG checksum is refused' v0.1.0-alpha.3 "$tmp/SHA256SUMS"
{ sums; sums | grep "$asset"; } > "$tmp/SHA256SUMS"
expect_refused "exactly one SHA-256 for $asset" 'duplicate identical DMG checksums are refused' v0.1.0-alpha.3 "$tmp/SHA256SUMS"
{ sums; printf '%064d  %s\n' 0 "$asset"; } > "$tmp/SHA256SUMS"
expect_refused "exactly one SHA-256 for $asset" 'conflicting DMG checksums are refused' v0.1.0-alpha.3 "$tmp/SHA256SUMS"
for sha in invalid '' 222222222222222222222222222222222222222222222222222222222222222 \
    22222222222222222222222222222222222222222222222222222222222222222 \
    gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg; do
    printf '%s  %s\n' "$sha" "$asset" > "$tmp/SHA256SUMS"
    expect_refused "exactly one SHA-256 for $asset" 'a malformed DMG checksum is refused' v0.1.0-alpha.3 "$tmp/SHA256SUMS"
done
sums | sed "/$asset/s/$/ extra/" > "$tmp/SHA256SUMS"
expect_refused "exactly one SHA-256 for $asset" 'a DMG entry with extra fields is refused' v0.1.0-alpha.3 "$tmp/SHA256SUMS"

cask_at() {
    case $1 in
        '') rm -f "$tmp/koidrive.rb" ;;
        latest) printf '  url "https://example.com/download/latest/Koidrive-latest-arm64.dmg"\n' > "$tmp/koidrive.rb" ;;
        mismatch) printf '  url "https://example.com/download/v0.1.0/Koidrive-0.2.0-arm64.dmg"\n' > "$tmp/koidrive.rb" ;;
        *)
            printf '%064d  Koidrive-%s-arm64.dmg\n' 0 "$1" > "$tmp/SHA256SUMS"
            render "v$1" "$tmp/SHA256SUMS" > "$tmp/koidrive.rb"
            ;;
    esac
}
expect_track() {
    local releases=$1 current=$2 tag=$3 expected=$4 description=$5 actual
    cask_at "$current"
    actual=$(GH_STUB_RELEASES=$releases bash "$root/scripts/homebrew-track.sh" "$tag" "$tmp/koidrive.rb" 2>/dev/null) \
        || fail "$description: homebrew-track.sh failed"
    [ "$actual" = "$expected" ] || fail "$description: got $actual, expected $expected"
    pass "$description"
}
expect_track_error() {
    local description=$1 output
    if output=$(bash "$root/scripts/homebrew-track.sh" v0.1.1 "$tmp/koidrive.rb" 2>&1); then
        fail "$description: homebrew-track.sh succeeded"
    fi
    grep -qF 'names the release' <<< "$output" || fail "$description: expected a release parse error in: $output"
    pass "$description"
}

expect_track 'v0.1.0-alpha.1' '' v0.1.0-alpha.1 bump 'the first alpha creates the cask'
expect_track 'v0.1.0-alpha.1 v0.1.0-alpha.2' 0.1.0-alpha.1 v0.1.0-alpha.2 bump 'a newer alpha moves the cask'
expect_track 'v0.1.0-alpha.9 v0.1.0-alpha.10' 0.1.0-alpha.9 v0.1.0-alpha.10 bump 'alpha numbers compare numerically'
expect_track 'v0.1.0-alpha.2 v0.1.0-rc.1' 0.1.0-alpha.2 v0.1.0-rc.1 bump 'an rc moves the cask past alphas'
expect_track 'v0.1.0-rc.1 v0.1.0-rc.2' 0.1.0-rc.1 v0.1.0-rc.2 bump 'a newer rc moves the cask'
expect_track 'v0.1.0-alpha.2' 0.1.0-alpha.2 v0.1.0-alpha.2 skip 'an alpha rerun leaves the cask'
expect_track 'v0.1.0-rc.1' 0.1.0-rc.1 v0.1.0-rc.1 skip 'an rc rerun leaves the cask'
expect_track 'v0.1.0-alpha.2 v0.1.0-alpha.3' 0.1.0-alpha.3 v0.1.0-alpha.2 skip 'an older alpha leaves the cask'
expect_track 'v0.1.0-rc.1 v0.1.0' 0.1.0-rc.1 v0.1.0 bump 'a stable release replaces the rc cask'
expect_track 'v0.1.0-alpha.3 v0.1.0' 0.1.0-alpha.3 v0.1.0 bump 'a stable release replaces the alpha cask'
expect_track 'v0.1.0 v0.2.0-alpha.1' 0.1.0 v0.2.0-alpha.1 skip 'a post-stable alpha leaves the cask'
expect_track 'v0.1.0 v0.2.0-rc.1' 0.1.0 v0.2.0-rc.1 skip 'a post-stable rc leaves the cask'
expect_track 'v0.1.0 v0.2.0-alpha.1' '' v0.2.0-alpha.1 skip 'a post-stable alpha does not bootstrap a cask'
expect_track 'v0.1.0:draft v0.2.0-alpha.1' 0.1.0-alpha.3 v0.2.0-alpha.1 bump 'a draft stable release does not block the cask'
expect_track 'v0.1.0 v0.1.1' 0.1.0 v0.1.1 bump 'a newer stable release moves the cask'
expect_track 'v0.1.0' 0.1.0 v0.1.0 skip 'a stable rerun leaves the cask'
expect_track 'v0.1.0 v0.2.0 v0.1.2' 0.2.0 v0.1.2 skip 'an out-of-order stable release leaves the newer cask'
expect_track 'v0.9.0 v0.10.0' 0.9.0 v0.10.0 bump 'stable versions compare numerically'
expect_track 'v0.1.0' '' v0.1.0 bump 'a stable release bootstraps a missing cask independently of the formula'
for tag in v0.1.0-alpha.3 v0.1.0; do
    printf '%064d  %s\n' \
        0 koidrive-aarch64-apple-darwin.tar.gz \
        0 koidrive-aarch64-unknown-linux-musl.tar.gz \
        0 koidrive-x86_64-unknown-linux-musl.tar.gz > "$tmp/SHA256SUMS"
    bash "$root/scripts/homebrew-formula.sh" "$tag" "$tmp/SHA256SUMS" > "$tmp/formula.rb"
    formula=$(bash "$root/scripts/homebrew-track.sh" "$tag" "$tmp/formula.rb" 2>/dev/null)
    cask=$(bash "$root/scripts/homebrew-track.sh" "$tag" "$tmp/missing.rb" 2>/dev/null)
    [ "$formula $cask" = 'skip bump' ] || fail "$tag did not bootstrap the cask beside a current formula"
    pass "$tag bootstraps the missing cask when its formula is already current"
done
for current in latest mismatch; do
    cask_at "$current"
    expect_track_error "a $current cask URL stops the bump"
done
cask_at 0.1.0
printf '  url "https://example.com/download/v0.2.0/Koidrive-#{version}-arm64.dmg"\n' >> "$tmp/koidrive.rb"
expect_track_error 'multiple current cask releases stop the bump'
if GH_STUB_FAIL_LIST=1 bash "$root/scripts/homebrew-track.sh" v0.1.0-alpha.2 "$tmp/koidrive.rb" > /dev/null 2>&1; then
    fail 'a failed release list did not stop cask tracking'
fi
pass 'a failed release list stops cask tracking'

"$BASH" "$root/scripts/test-homebrew-cask-install-errors.sh"
echo "Passed $passed Homebrew cask tests"
