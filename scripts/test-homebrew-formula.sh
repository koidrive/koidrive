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
        3333333333333333333333333333333333333333333333333333333333333333 koidrive-aarch64-unknown-linux-musl.tar.gz \
        4444444444444444444444444444444444444444444444444444444444444444 koidrive-x86_64-unknown-linux-musl.tar.gz \
        5555555555555555555555555555555555555555555555555555555555555555 Koidrive-0.1.0-alpha.3-arm64.dmg
}
render() {
    bash "$root/scripts/homebrew-formula.sh" "$@"
}
expect_refused() {
    local message=$1 description=$2 output
    shift 2
    if output=$(render "$@" 2>&1); then
        fail "$description: the formula rendered"
    fi
    grep '^::error::' <<< "$output" | grep -qF "$message" || fail "$description: expected an error with '$message' in: $output"
    pass "$description"
}

sums > "$tmp/SHA256SUMS"
for tag in v01.0.0 v0.01.0 v0.1.00 v0.1.0-alpha.01; do
    expect_refused "$tag is not a vX.Y.Z" "a tag with a leading zero, $tag, is refused" "$tag" "$tmp/SHA256SUMS"
done
cat > "$tmp/expected.rb" <<'EOF'
class Koidrive < Formula
  desc "File sync you deploy to your own Cloudflare account"
  homepage "https://koidrive.com"
  version "0.1.0-alpha.3"
  license all_of: ["Apache-2.0", "AGPL-3.0-or-later"]

  on_macos do
    depends_on arch: :arm64

    on_arm do
      url "https://github.com/koidrive/koidrive/releases/download/v0.1.0-alpha.3/koidrive-aarch64-apple-darwin.tar.gz"
      sha256 "1111111111111111111111111111111111111111111111111111111111111111"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/koidrive/koidrive/releases/download/v0.1.0-alpha.3/koidrive-aarch64-unknown-linux-musl.tar.gz"
      sha256 "3333333333333333333333333333333333333333333333333333333333333333"
    end
    on_intel do
      url "https://github.com/koidrive/koidrive/releases/download/v0.1.0-alpha.3/koidrive-x86_64-unknown-linux-musl.tar.gz"
      sha256 "4444444444444444444444444444444444444444444444444444444444444444"
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
render v0.1.0-alpha.3 "$tmp/SHA256SUMS" > "$tmp/actual.rb"
diff -u "$tmp/expected.rb" "$tmp/actual.rb" || fail 'the formula for an alpha tag differs from the expected formula'
pass 'an alpha tag renders the expected formula'

render v0.2.0 "$tmp/SHA256SUMS" > "$tmp/actual.rb"
! grep -q '^ *version ' "$tmp/actual.rb" || fail 'a release tag sets a version, which Homebrew reads from its URLs'
grep -qxF '      url "https://github.com/koidrive/koidrive/releases/download/v0.2.0/koidrive-x86_64-unknown-linux-musl.tar.gz"' "$tmp/actual.rb" \
    || fail 'a release tag does not name its release in the URLs'
pass 'a release tag leaves the version to the URLs it names'

KOIDRIVE_RELEASE_BASE_URL=file:///srv/releases render v0.2.0 "$tmp/SHA256SUMS" \
    | grep -qxF '      url "file:///srv/releases/download/v0.2.0/koidrive-x86_64-unknown-linux-musl.tar.gz"' \
    || fail 'KOIDRIVE_RELEASE_BASE_URL does not replace the release URL'
pass 'KOIDRIVE_RELEASE_BASE_URL replaces the release URL'

for target in aarch64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    sums | grep -v "koidrive-$target.tar.gz" > "$tmp/SHA256SUMS"
    expect_refused "has none or several for: koidrive-$target.tar.gz" "SHA256SUMS without koidrive-$target.tar.gz is refused" v0.2.0 "$tmp/SHA256SUMS"
done
sums | grep -v linux-musl > "$tmp/SHA256SUMS"
expect_refused 'koidrive-aarch64-unknown-linux-musl.tar.gz koidrive-x86_64-unknown-linux-musl.tar.gz' 'every missing tarball is named' v0.2.0 "$tmp/SHA256SUMS"
{ sums; sums | grep aarch64-apple-darwin; } > "$tmp/SHA256SUMS"
expect_refused 'koidrive-aarch64-apple-darwin.tar.gz' 'a tarball listed twice is refused' v0.2.0 "$tmp/SHA256SUMS"
sums | sed 's/^1111111111111111111111111111111111111111111111111111111111111111 /invalid /' > "$tmp/SHA256SUMS"
expect_refused 'koidrive-aarch64-apple-darwin.tar.gz' 'a malformed SHA-256 is refused' v0.2.0 "$tmp/SHA256SUMS"
sums > "$tmp/SHA256SUMS"
expect_refused 'is not a vX.Y.Z' 'a tag that is not a release tag is refused' 0.2.0 "$tmp/SHA256SUMS"
expect_refused 'does not exist' 'a missing SHA256SUMS is refused' v0.2.0 "$tmp/missing"

sums > "$tmp/SHA256SUMS"
formula_at() {
    case $1 in
        '') rm -f "$tmp/koidrive.rb" ;;
        latest) printf 'class Koidrive < Formula\n  url "https://example.com/download/latest/koidrive-x86_64-unknown-linux-musl.tar.gz"\nend\n' > "$tmp/koidrive.rb" ;;
        *) render "v$1" "$tmp/SHA256SUMS" > "$tmp/koidrive.rb" ;;
    esac
}
expect_track() {
    local releases=$1 formula=$2 tag=$3 expected=$4 description=$5 actual
    formula_at "$formula"
    actual=$(GH_STUB_RELEASES=$releases bash "$root/scripts/homebrew-track.sh" "$tag" "$tmp/koidrive.rb" 2>/dev/null) \
        || fail "$description: homebrew-track.sh failed"
    [ "$actual" = "$expected" ] || fail "$description: got $actual, expected $expected"
    pass "$description"
}
expect_track_error() {
    local releases=$1 formula=$2 tag=$3 message=$4 description=$5 output
    formula_at "$formula"
    if output=$(GH_STUB_RELEASES=$releases bash "$root/scripts/homebrew-track.sh" "$tag" "$tmp/koidrive.rb" 2>&1); then
        fail "$description: homebrew-track.sh succeeded with $output"
    fi
    grep -qF "$message" <<< "$output" || fail "$description: expected '$message' in: $output"
    pass "$description"
}

expect_track 'v0.1.0-alpha.1' '' v0.1.0-alpha.1 bump 'the first alpha creates the formula'
expect_track 'v0.1.0-alpha.1 v0.1.0-alpha.2' 0.1.0-alpha.1 v0.1.0-alpha.2 bump 'a higher alpha moves the formula while no release is stable'
expect_track 'v0.1.0-alpha.2 v0.1.0-rc.1' 0.1.0-alpha.2 v0.1.0-rc.1 bump 'an rc moves the formula past the alphas'
expect_track 'v0.1.0-alpha.2' 0.1.0-alpha.2 v0.1.0-alpha.2 skip 'a rerun of the same tag leaves the formula'
expect_track 'v0.1.0-alpha.2 v0.1.0-alpha.3' 0.1.0-alpha.3 v0.1.0-alpha.2 skip 'an older alpha leaves the formula'
expect_track 'v0.1.0-rc.1 v0.1.0' 0.1.0-rc.1 v0.1.0 bump 'the release moves the formula past its rc'
expect_track 'v0.1.0 v0.2.0-alpha.1' 0.1.0 v0.2.0-alpha.1 skip 'an alpha after a stable release leaves the formula'
expect_track 'v0.1.0 v0.2.0-alpha.1' '' v0.2.0-alpha.1 skip 'an alpha after a stable release does not create the formula'
expect_track 'v0.1.0:draft v0.2.0-alpha.1' 0.1.0-alpha.3 v0.2.0-alpha.1 bump 'a draft stable release does not hold back an alpha'
expect_track 'v0.1.0 v0.1.1' 0.1.0 v0.1.1 bump 'a higher stable release moves the formula'
expect_track 'v0.1.0 v0.2.0 v0.1.2' 0.2.0 v0.1.2 skip 'a backport published after a higher release leaves the formula'
expect_track 'v0.9.0 v0.10.0' 0.9.0 v0.10.0 bump 'versions compare as numbers'
expect_track 'v0.1.0' '' v0.1.0 bump 'a stable release creates a missing formula'
expect_track_error 'v0.1.0' 'latest' v0.1.1 "names the release ''" 'a formula version that is not a release version stops the bump'
expect_track_error 'v0.1.0' 0.1.0 0.1.1 'is not a vX.Y.Z' 'a tag that is not a release tag is refused'
if GH_STUB_FAIL_LIST=1 GH_STUB_RELEASES='' bash "$root/scripts/homebrew-track.sh" v0.1.0-alpha.2 "$tmp/koidrive.rb" > /dev/null 2>&1; then
    fail 'homebrew-track.sh succeeded although the release list failed'
fi
pass 'a failed release list stops the bump'

echo "Passed $passed Homebrew formula tests"
