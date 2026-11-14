#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
set -eu

binary=${1:?Usage: test-install.sh <binary> <target>}
target=${2:?Usage: test-install.sh <binary> <target>}
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d)
server=
trap 'if [ -n "$server" ]; then kill "$server"; fi; if [ -s "$tmp/daemon.pids" ]; then xargs kill < "$tmp/daemon.pids" 2>/dev/null; fi; rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM
export HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config"
export KOIDRIVE_VERSION=v0.0.0
unset KOIDRIVE_INSTALL_DIR
mkdir -p "$HOME" "$tmp/package" "$tmp/releases/download/v0.0.0" "$tmp/releases/latest" "$tmp/bin" "$tmp/services"
export PATH="$tmp/services:$PATH"
cat > "$tmp/services/systemctl" <<'EOF'
#!/bin/sh
set -f
printf '%s\n' "$*" >> "$HOME/services.log"
if [ "$1 $2 $3 $4" = '--user show --property=MainPID --value' ]; then
    awk -v unit="$5" '$1 == unit {print $2}' "$HOME/running" 2>/dev/null
    exit 0
fi
[ "$1 $2" = '--user list-units' ] || exit 0
if [ -e "$HOME/unreachable" ]; then
    printf 'Failed to connect to bus: No medium found\n' >&2
    exit 1
fi
shift 2
state= patterns=
for arg; do
    case $arg in
        --state=*) state=${arg#--state=} ;;
        -*) ;;
        *) patterns="$patterns $arg" ;;
    esac
done
{
    printf '%s\n' 'other.service active' 'koidrive-backup.service active' 'koidrive-0badf00d.service failed'
    [ ! -e "$HOME/running" ] || awk '{print $1, "active"}' "$HOME/running"
} | while read -r unit active; do
    [ -z "$state" ] || [ "$active" = "$state" ] || continue
    for pattern in $patterns; do
        case $unit in
            $pattern) printf '%s loaded %s running Koidrive\n' "$unit" "$active"; break ;;
        esac
    done
done
EOF
cat > "$tmp/services/launchctl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$HOME/services.log"
case "$*" in
    "print gui/$(id -u)/"*)
        label=${2#gui/*/}
        [ ! -e "$HOME/running" ] || awk -v label="$label" '$1 == label && NF > 2 { sub(/^[^ ]+ [^ ]+ /, ""); print "\tprogram = " $0 }' "$HOME/running"
        exit 0
        ;;
    "print gui/$(id -u)") ;;
    *) exit 0 ;;
esac
[ ! -e "$HOME/unreachable" ] || exit 113
printf 'gui/%s = {\n\ttype = login\n\tservices = {\n' "$(id -u)"
printf '\t\t%8s %6s \t%s\n' 101 - com.example.other 102 - dev.koidrive.koiz 104 - dev.koidrive.koi.backup 0 0 dev.koidrive.koi.0badf00d
[ ! -e "$HOME/running" ] || awk '{printf "\t\t%8s %6s \t%s\n", $2, "-", $1}' "$HOME/running"
printf '\t}\n\n\tunmanaged processes = {\n\t\t     105      - \tdev.koidrive.koi.1a2b3c4e\n\t}\n}\n'
EOF
cat > "$tmp/services/codesign" <<'EOF'
#!/bin/sh
exit 0
EOF
cat > "$tmp/services/ps" <<'EOF'
#!/bin/sh
if [ "$1 $2 $3" = '-ww -o cgroup=' ] && [ -e "$HOME/cgroup" ]; then
    cat "$HOME/cgroup"
    exit 0
fi
exec /bin/ps "$@"
EOF
chmod +x "$tmp/services/systemctl" "$tmp/services/launchctl" "$tmp/services/codesign" "$tmp/services/ps"
cp "$binary" "$tmp/package/koi"
archive=koidrive-$target.tar.gz
release=$tmp/releases/download/v0.0.0
tar -czf "$release/$archive" -C "$tmp/package" koi
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$release" && sha256sum "$archive") > "$release/SHA256SUMS"
else
    (cd "$release" && shasum -a 256 "$archive") > "$release/SHA256SUMS"
fi
printf '%064d  Koidrive-0.0.0-arm64.dmg\n' 0 >> "$release/SHA256SUMS"
sign() {
    rm -f "$release/SHA256SUMS.sig"
    ssh-keygen -q -Y sign -f "$tmp/release-key" -n koidrive-release "$release/SHA256SUMS"
}
ssh-keygen -q -t ed25519 -N '' -C test-release -f "$tmp/release-key"
pinned=$(cut -d' ' -f4 "$root/.github/release-allowed-signers")
sed "s|$pinned|$(cut -d' ' -f2 "$tmp/release-key.pub")|" "$root/install.sh" > "$tmp/install.sh"
if cmp -s "$root/install.sh" "$tmp/install.sh"; then
    printf 'install.sh does not pin the key in .github/release-allowed-signers\n' >&2
    exit 1
fi
installer=$tmp/install.sh
sign
cp "$release/SHA256SUMS" "$tmp/good-sums"
ln -s ../download/v0.0.0 "$tmp/releases/latest/download"
python3 "$root/scripts/release-fixture-server.py" "$tmp/releases" "$tmp/server" --tls "$tmp/tls" &
server=$!
waited=0
until [ -s "$tmp/server/origin" ]; do
    if ! kill -0 "$server" 2>/dev/null || [ "$waited" -ge 200 ]; then
        printf 'The HTTPS release server did not start\n' >&2
        exit 1
    fi
    waited=$((waited + 1))
    sleep 0.05
done
KOIDRIVE_RELEASE_BASE_URL=$(cat "$tmp/server/origin")
export KOIDRIVE_RELEASE_BASE_URL CURL_CA_BUNDLE="$tmp/tls/cert.pem"

passed=0
pass() { printf 'PASS %s\n' "$1"; passed=$((passed + 1)); }
reject() {
    if sh "$installer" > "$tmp/rejected.log" 2>&1; then
        cat "$tmp/rejected.log"
        printf 'Expected installation to fail\n' >&2
        exit 1
    fi
    cat "$tmp/rejected.log"
    grep -F "$2" "$tmp/rejected.log"
    cmp "$binary" "$HOME/.local/bin/koi"
    pass "$1"
}

sed '$d' "$installer" | HOME="$tmp/truncated" sh
[ ! -e "$tmp/truncated" ]
if head -n 60 "$installer" | HOME="$tmp/truncated" sh > "$tmp/truncated.log" 2>&1; then
    printf 'Expected a truncated function body to fail parsing\n' >&2
    exit 1
fi
[ ! -e "$tmp/truncated" ]
pass 'truncated download does not install anything'

cat "$installer" | sh > "$tmp/install.log" 2>&1
cat "$tmp/install.log"
grep -q 'not on PATH' "$tmp/install.log"
cmp "$binary" "$HOME/.local/bin/koi"
"$HOME/.local/bin/koi" --version
pass 'piped install, checksum, executable, and PATH warning'
sh "$installer"
cmp "$binary" "$HOME/.local/bin/koi"
pass 'repeat installation'
printf 'old Koidrive sync client\n' > "$HOME/.local/bin/koi"
sh "$installer"
cmp "$binary" "$HOME/.local/bin/koi"
pass 'upgrade replaces the old binary'
printf '#!/bin/sh\n' > "$HOME/.local/bin/koi"
cp "$HOME/.local/bin/koi" "$tmp/unrelated"
if sh "$installer" > "$tmp/unrelated.log" 2>&1; then
    cat "$tmp/unrelated.log"
    printf 'Expected installation over another koi to fail\n' >&2
    exit 1
fi
cat "$tmp/unrelated.log"
grep -F "$HOME/.local/bin/koi is another program" "$tmp/unrelated.log"
cmp "$tmp/unrelated" "$HOME/.local/bin/koi"
cp "$binary" "$HOME/.local/bin/koi"
pass 'another program named koi in the install directory is kept'
unset KOIDRIVE_VERSION
KOIDRIVE_INSTALL_DIR="$HOME/custom bin" sh "$installer"
cmp "$binary" "$HOME/custom bin/koi"
pass 'latest release and custom install directory'
mkdir -p "$tmp/other"
printf '#!/bin/sh\n' > "$tmp/other/koi"
chmod +x "$tmp/other/koi"
PATH="$tmp/other:$PATH" sh "$installer" > "$tmp/other.log" 2>&1
cat "$tmp/other.log"
grep -F "koi on PATH runs $tmp/other/koi, another program" "$tmp/other.log"
PATH="$HOME/.local/bin:$tmp/other:$PATH" sh "$installer" > "$tmp/first.log" 2>&1
if grep -q 'another program' "$tmp/first.log"; then exit 1; fi
pass 'another koi on PATH is reported'
PATH="$HOME/.local/bin:$PATH" KOIDRIVE_INSTALL_DIR="$HOME/.local/bin/" sh "$installer" > "$tmp/slash.log" 2>&1
cat "$tmp/slash.log"
if grep -q -e 'Warning' "$tmp/slash.log"; then exit 1; fi
cmp "$binary" "$HOME/.local/bin/koi"
pass 'trailing slash in the install directory is the same directory'
ln -s "$HOME/.local/bin" "$tmp/linked"
PATH="$tmp/linked:$PATH" sh "$installer" > "$tmp/linked.log" 2>&1
cat "$tmp/linked.log"
if grep -q -e 'Warning' "$tmp/linked.log"; then exit 1; fi
pass 'install directory reached through a symlink on PATH'
mkdir -p "$tmp/older"
printf '#!/bin/sh\n# Koidrive sync client\n' > "$tmp/older/koi"
chmod +x "$tmp/older/koi"
PATH="$tmp/older:$HOME/.local/bin:$PATH" sh "$installer" > "$tmp/older.log" 2>&1
cat "$tmp/older.log"
grep -F "koi on PATH runs $tmp/older/koi, another Koidrive install" "$tmp/older.log"
if grep -q 'another program' "$tmp/older.log"; then exit 1; fi
pass 'an older Koidrive koi on PATH is named as one'
export KOIDRIVE_VERSION=0.0.0
printf '%064d  %s\n' 0 "$archive" > "$release/SHA256SUMS"
printf '%064d  Koidrive-0.0.0-arm64.dmg\n' 0 >> "$release/SHA256SUMS"
sign
reject 'checksum mismatch preserves the installed binary' 'Checksum mismatch'
printf '%064d  other.tar.gz\n' 0 > "$release/SHA256SUMS"
printf '%064d  Koidrive-0.0.0-arm64.dmg\n' 0 >> "$release/SHA256SUMS"
sign
reject 'missing checksum entry' 'exactly one entry'
cat "$tmp/good-sums" "$tmp/good-sums" > "$release/SHA256SUMS"
sign
reject 'duplicate checksum entries' 'exactly one entry'
cp "$tmp/good-sums" "$release/SHA256SUMS"
sign
mv "$release/$archive" "$tmp/saved.tar.gz"
reject 'missing tarball' 'curl:'
mv "$tmp/saved.tar.gz" "$release/$archive"
KOIDRIVE_VERSION=../../bad reject 'invalid version' 'KOIDRIVE_VERSION must be'
export KOIDRIVE_VERSION=0.0.0
KOIDRIVE_INSTALL_DIR=relative reject 'relative install directory' 'KOIDRIVE_INSTALL_DIR must be'
unset KOIDRIVE_INSTALL_DIR

if [ "$(uname -s)" = Linux ]; then
    set -- koidrive.service koidrive-1a2b3c4d.service koidrive-5e6f7a8b.service
    restarts='--user restart koidrive.service
--user restart koidrive-1a2b3c4d.service'
else
    set -- dev.koidrive.koi dev.koidrive.koi.1a2b3c4d dev.koidrive.koi.5e6f7a8b
    restarts="kickstart -k gui/$(id -u)/dev.koidrive.koi
kickstart -k gui/$(id -u)/dev.koidrive.koi.1a2b3c4d"
fi
mkdir "$tmp/daemons"
printf 'sleep 300;\n' > "$tmp/daemons/_daemon"
start_daemon() {
    program=$(command -v perl)
    if [ "$(uname -s)" = Linux ]; then
        if [ ! -h "$1" ]; then
            { cat "$program"; printf '\nKoidrive sync client\n'; } > "$1.new"
            chmod 755 "$1.new"
            mv -f "$1.new" "$1"
        fi
        program=$1
    fi
    (cd "$tmp/daemons" && exec perl -e 'my $program = shift; exec { $program } @ARGV' "$program" "$1" _daemon) &
    printf '%s\n' "$!" >> "$tmp/daemon.pids"
    waited=0
    until if [ "$(uname -s)" = Linux ]; then [ "/proc/$!/exe" -ef "$1" ]; else case "$(ps -ww -o args= -p "$!")" in "perl "*) false ;; *" _daemon") true ;; *) false ;; esac; fi; do
        if [ "$waited" -ge 200 ]; then
            printf 'The test daemon for %s did not start\n' "$1" >&2
            exit 1
        fi
        waited=$((waited + 1))
        sleep 0.05
    done
    [ -z "${2:-}" ] || printf '%s %s %s\n' "$2" "$!" "$1" >> "$HOME/running"
}
expect_restarts() {
    if [ "$(cat "$tmp/restarted")" != "$2" ]; then
        cat "$HOME/services.log" "$tmp/restart.log"
        printf 'Expected %s\n' "$1" >&2
        exit 1
    fi
    pass "$1"
}
rm -f "$HOME/services.log"
sh "$installer"
if grep -E -e '--user restart|kickstart' "$HOME/services.log"; then
    printf 'Expected an upgrade to leave services that are not running alone\n' >&2
    exit 1
fi
pass 'an upgrade leaves services that are not running alone'
start_daemon "$HOME/.local/bin/koi" "$1"
start_daemon "$tmp/linked/koi" "$2"
start_daemon "$tmp/other/koi" "$3"
rm -f "$HOME/services.log"
sh "$installer" > "$tmp/restart.log" 2>&1
grep -E -e '--user restart|kickstart' "$HOME/services.log" > "$tmp/restarted" || true
expect_restarts 'an upgrade restarts the running service of every profile that runs this koi, and leaves another koi alone' "$restarts"
if grep -F 'could not tell' "$tmp/restart.log"; then exit 1; fi
rm "$HOME/running"

nonascii="$tmp/내 앱 👨‍💻 می‌خواهم"
mkdir "$nonascii"
cp "$HOME/.local/bin/koi" "$nonascii/koi"
start_daemon "$nonascii/koi" "$1"
rm -f "$HOME/services.log"
LC_ALL=C KOIDRIVE_INSTALL_DIR="$nonascii" sh "$installer" > "$tmp/restart.log" 2>&1
grep -E -e '--user restart|kickstart' "$HOME/services.log" > "$tmp/restarted" || true
expect_restarts 'an upgrade under LC_ALL=C restarts the service of a koi in a folder with Hangul, a ZWJ emoji and a ZWNJ' "$(printf '%s\n' "$restarts" | head -n 1)"
rm "$HOME/running"

true &
gone=$!
wait "$gone"
printf '%s %s\n' "$2" "$gone" > "$HOME/running"
rm -f "$HOME/services.log"
sh "$installer" > "$tmp/restart.log" 2>&1
grep -E -e '--user restart|kickstart' "$HOME/services.log" > "$tmp/restarted" || true
expect_restarts 'an upgrade does not restart a service whose koi it cannot tell' ''
if ! grep -F "koi could not tell which koi the background service $2 runs, so it did not restart it" "$tmp/restart.log"; then
    cat "$tmp/restart.log"
    printf 'Expected an upgrade to say which service it could not check\n' >&2
    exit 1
fi
pass 'an upgrade names a service whose koi it cannot tell'
rm "$HOME/running"

if [ "$(uname -s)" = Darwin ]; then
    start_daemon "$HOME/.local/bin/koi"
    daemon=$!
    start_daemon "$nonascii/koi"
    mangled=$!
    touch "$HOME/unreachable"
    sh "$installer" 2> "$tmp/nogui.log"
    rm "$HOME/unreachable"
    if ! grep -F "the koi daemon with process ID $daemon still runs the old koi" "$tmp/nogui.log" ||
        ! grep -F "koi could not tell which koi the daemon with process ID $mangled runs" "$tmp/nogui.log"; then
        cat "$tmp/nogui.log"
        printf 'Expected an upgrade without a launchd GUI domain to name the daemon that runs the old koi and the one it cannot tell\n' >&2
        exit 1
    fi
    pass 'an upgrade without a launchd GUI domain names the daemon that runs the old koi and the one it cannot tell'
fi

if [ "$(uname -s)" = Linux ]; then
    mkdir "$tmp/nops"
    printf '#!/bin/sh\nexit 127\n' > "$tmp/nops/ps"
    chmod +x "$tmp/nops/ps"
    start_daemon "$HOME/.local/bin/koi" "$1"
    rm -f "$HOME/services.log"
    PATH="$tmp/nops:$PATH" sh "$installer" > "$tmp/restart.log" 2>&1
    grep -E -e '--user restart' "$HOME/services.log" > "$tmp/restarted" || true
    expect_restarts 'an upgrade on a systemd host without ps restarts this koi' "$(printf '%s\n' "$restarts" | head -n 1)"
    rm "$HOME/running"

    xargs kill < "$tmp/daemon.pids"
    rm "$tmp/daemon.pids"
    start_daemon "$HOME/.local/bin/koi"
    daemon=$!
    start_daemon "$tmp/other/koi"
    other=$!
    touch "$HOME/unreachable"
    COLUMNS=20 sh "$installer" 2> "$tmp/nobus.log"
    if ! grep -F "the koi daemon with process ID $daemon still runs the old koi" "$tmp/nobus.log" || grep -F "process ID $other " "$tmp/nobus.log"; then
        cat "$tmp/nobus.log"
        printf 'Expected an upgrade without a systemd user manager to name only the daemon that still runs the old koi\n' >&2
        exit 1
    fi
    pass 'an upgrade without a systemd user manager names only the daemon that still runs the old koi'
    PATH="$tmp/nops:$PATH" sh "$installer" 2> "$tmp/nobus.log"
    if ! grep -F "the koi daemon with process ID $daemon still runs the old koi" "$tmp/nobus.log"; then
        cat "$tmp/nobus.log"
        printf 'Expected an upgrade without a systemd user manager or ps to name the daemon that still runs the old koi\n' >&2
        exit 1
    fi
    pass 'an upgrade without a systemd user manager or ps names the daemon that still runs the old koi'
    printf '0::/user.slice/user-%s.slice/user@%s.service/app.slice/koidrive-1a2b3c4d.service\n' "$(id -u)" "$(id -u)" > "$HOME/cgroup"
    sh "$installer" 2> "$tmp/nobus.log"
    rm "$HOME/unreachable" "$HOME/cgroup"
    if ! grep -F "Log in as $(id -un) and run: systemctl --user restart koidrive-1a2b3c4d.service" "$tmp/nobus.log"; then
        cat "$tmp/nobus.log"
        printf 'Expected an upgrade without a systemd user manager to give the command that restarts the unit\n' >&2
        exit 1
    fi
    pass 'an upgrade without a systemd user manager gives the command that restarts the unit'
fi

cat > "$tmp/bin/uname" <<'EOF'
#!/bin/sh
case "$1" in
    -s) printf '%s\n' "$KOIDRIVE_TEST_OS" ;;
    -m) printf '%s\n' "$KOIDRIVE_TEST_ARCH" ;;
    *) exit 1 ;;
esac
EOF
cat > "$tmp/bin/sysctl" <<'EOF'
#!/bin/sh
[ "$*" = '-n hw.optional.arm64' ] || exit 1
[ "$KOIDRIVE_TEST_ARM64" != missing ] || exit 1
printf '%s\n' "$KOIDRIVE_TEST_ARM64"
EOF
chmod +x "$tmp/bin/uname" "$tmp/bin/sysctl"
hash=$(awk 'NR == 1 {print $1}' "$tmp/good-sums")
for platform in aarch64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    selected=koidrive-$platform.tar.gz
    if [ "$selected" != "$archive" ]; then
        cp "$release/$archive" "$release/$selected"
    fi
    printf '%s  %s\n' "$hash" "$selected"
done > "$release/SHA256SUMS"
printf '%064d  Koidrive-0.0.0-arm64.dmg\n' 0 >> "$release/SHA256SUMS"
sign
for platform in 'Darwin arm64 1 aarch64-apple-darwin' 'Darwin x86_64 1 aarch64-apple-darwin' 'Linux x86_64 0 x86_64-unknown-linux-musl' 'Linux aarch64 0 aarch64-unknown-linux-musl'; do
    read -r os arch arm64 selected <<EOF
$platform
EOF
    PATH="$tmp/bin:$PATH" KOIDRIVE_TEST_OS=$os KOIDRIVE_TEST_ARCH=$arch KOIDRIVE_TEST_ARM64=$arm64 sh "$installer" > "$tmp/arch.log" 2>&1
    cat "$tmp/arch.log"
    grep -F "Downloading koidrive-$selected.tar.gz" "$tmp/arch.log"
    cmp "$binary" "$HOME/.local/bin/koi"
    pass "$os $arch shell selects $selected with hw.optional.arm64=$arm64"
done
for arm64 in 0 missing; do
    PATH="$tmp/bin:$PATH" KOIDRIVE_TEST_OS=Darwin KOIDRIVE_TEST_ARCH=x86_64 KOIDRIVE_TEST_ARM64=$arm64 reject \
        "Intel Mac is refused with hw.optional.arm64=$arm64" 'macOS requires Apple silicon'
    if grep -q Downloading "$tmp/rejected.log"; then
        printf 'Expected an Intel Mac to be refused before downloading\n' >&2
        exit 1
    fi
done
printf 'Passed %s installer tests on %s\n' "$passed" "$(uname -s)"
