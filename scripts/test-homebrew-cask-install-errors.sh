#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d)
native_hdiutil=$(command -v hdiutil || true)
mounted=
cleanup() {
    if [ -n "$mounted" ]; then
        "$native_hdiutil" detach "$mounted" || return
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT
mkdir -p "$tmp/bin" "$tmp/images with spaces" "$tmp/tap/Casks"
cat > "$tmp/bin/brew" <<'EOF'
#!/usr/bin/env bash
case $1 in
    tap|tap-new|untap) ;;
    list) exit 1 ;;
    --prefix) printf '%s/prefix\n' "$CASK_FIXTURE_TEST_ROOT" ;;
    --repository) printf '%s/tap\n' "$CASK_FIXTURE_TEST_ROOT" ;;
    --cache) printf '%s/cache\n' "$CASK_FIXTURE_TEST_ROOT" ;;
    *) printf 'unexpected brew command: %s\n' "$*" >&2; exit 91 ;;
esac
EOF
cat > "$tmp/bin/uuidgen" <<'EOF'
#!/usr/bin/env bash
printf '00000000-0000-0000-0000-000000000000\n'
EOF
cat > "$tmp/bin/hdiutil" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$1" >> "$CASK_FIXTURE_TEST_ROOT/image-commands"
if [ "$CASK_FIXTURE_TEST_STAGE" = image ]; then
    case $1 in
        makehybrid|convert) exec "$CASK_FIXTURE_TEST_HDIUTIL" "$@" ;;
    esac
fi
case $1 in
    makehybrid)
        case $CASK_FIXTURE_TEST_STAGE in
            silent|native)
                if [ "$CASK_FIXTURE_TEST_STAGE" = native ]; then
                    case " $* " in *' -quiet '*) ;; *) printf 'native image creation detail\n' >&2 ;; esac
                fi
                exit 73
                ;;
        esac
        [ -f "${!#}/Koidrive.app/Contents/Info.plist" ]
        while [ "$1" != -o ]; do shift; done
        printf 'fixture image\n' > "$2"
        ;;
    convert)
        [ -f "$2" ]
        case $CASK_FIXTURE_TEST_STAGE in
            convert-silent|convert-native)
                if [ "$CASK_FIXTURE_TEST_STAGE" = convert-native ]; then
                    case " $* " in *' -quiet '*) ;; *) printf 'native image conversion detail\n' >&2 ;; esac
                fi
                exit 74
                ;;
        esac
        printf 'compressed fixture image\n' > "${!#}"
        ;;
    *) printf 'FAIL image construction must not create or attach a temporary volume: %s\n' "$*" >&2; exit 91 ;;
esac
EOF
cat > "$tmp/bin/shasum" <<'EOF'
#!/usr/bin/env bash
if [ "$CASK_FIXTURE_TEST_STAGE" = image ]; then
    set -- ./*.dmg
    [ "$#" -eq 1 ] && [ -f "$1" ] || exit 92
    cp "$1" "$CASK_FIXTURE_TEST_ROOT/image.dmg" || exit 93
fi
printf 'checksum failure detail\n' >&2
exit 75
EOF
chmod +x "$tmp/bin/"*
export PATH="$tmp/bin:$PATH" CASK_FIXTURE_TEST_ROOT=$tmp HOMEBREW_TEMP="$tmp/images with spaces" CASK_FIXTURE_TEST_HDIUTIL=$native_hdiutil

count=0
for stage in silent native convert-silent convert-native checksum image; do
    if [ "$stage" = image ] && [ -z "$native_hdiutil" ]; then
        continue
    fi
    : > "$tmp/image-commands"
    status=0
    CASK_FIXTURE_TEST_STAGE=$stage "$BASH" "$root/scripts/test-homebrew-cask-install.sh" > "$tmp/output" 2>&1 || status=$?
    expected=73
    command='    hdiutil makehybrid '
    case $stage in
        convert-*) expected=74; command='    hdiutil convert ' ;;
        checksum|image) expected=75; command="    (cd \"\$release\" && shasum " ;;
    esac
    line=$(grep -nF "$command" "$root/scripts/test-homebrew-cask-install.sh" | cut -d: -f1 || true)
    first=$(grep '^FAIL ' "$tmp/output" | head -1 || true)
    if [ "$status" -ne "$expected" ] || ! grep -qF "test-homebrew-cask-install.sh:$line status=$expected" <<< "$first" || ! grep -qF 'version=0.1.0-alpha.2' <<< "$first"; then
        cat "$tmp/output" >&2
        echo "FAIL $stage failure lacks exact first diagnostic line $line, version or original status $expected (observed $status)" >&2
        exit 1
    fi
    if [[ "$stage" != checksum && "$stage" != image ]] && [ "$(grep -c '^FAIL ' "$tmp/output")" -ne 1 ]; then
        cat "$tmp/output" >&2
        echo "FAIL $stage failure emitted unrelated diagnostics" >&2
        exit 1
    fi
    case $stage in
        silent) grep -qF 'command=hdiutil makehybrid' "$tmp/output" ;;
        native) grep -qF 'native image creation detail' "$tmp/output" ;;
        convert-silent) grep -qF 'command=hdiutil convert' "$tmp/output" ;;
        convert-native) grep -qF 'native image conversion detail' "$tmp/output" ;;
        checksum|image) grep -qF 'pipeline=75 0' "$tmp/output" && grep -qF 'checksum failure detail' "$tmp/output" ;;
    esac || { cat "$tmp/output" >&2; echo "FAIL $stage failure lost its diagnostic detail" >&2; exit 1; }
    commands=makehybrid
    case $stage in convert-*|checksum|image) commands=$'makehybrid\nconvert' ;; esac
    [ "$(cat "$tmp/image-commands")" = "$commands" ] || { echo "FAIL $stage retried or mounted an image during construction" >&2; exit 1; }
    if [ "$stage" = image ]; then
        "$native_hdiutil" imageinfo -plist "$tmp/image.dmg" > "$tmp/image-info.plist"
        [ "$(/usr/libexec/PlistBuddy -c 'Print Format' "$tmp/image-info.plist")" = UDZO ]
        mounted=$tmp/mounted
        mkdir -p "$mounted"
        "$native_hdiutil" attach -readonly -nobrowse -mountpoint "$mounted" "$tmp/image.dmg"
        plist=$mounted/Koidrive.app/Contents/Info.plist
        [ "$(/usr/libexec/PlistBuddy -c 'Print KoidriveVersion' "$plist")" = 0.1.0-alpha.2 ]
        [ "$(/bin/sh "$mounted/Koidrive.app/Contents/Helpers/koi" --version)" = 'koi 0.1.0-alpha.2' ]
        "$native_hdiutil" detach "$mounted"
        mounted=
        echo 'PASS cask fixture builds a mountable UDZO image with the app and bundled CLI without Homebrew changes'
    else
        echo "PASS cask fixture reports $stage failure and preserves exit status $expected"
    fi
    count=$((count + 1))
done
echo "Passed $count Homebrew cask image and failure diagnostic tests"
