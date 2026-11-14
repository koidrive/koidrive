#!/bin/sh
# SPDX-License-Identifier: Apache-2.0

main() {
    set -eu

    fail() {
        printf 'koi install: %s\n' "$*" >&2
        exit 1
    }

    no_setup=${KOIDRIVE_NO_SETUP:-0}
    no_modify_path=${KOIDRIVE_NO_MODIFY_PATH:-0}
    no_app=0
    for option do
        case "$option" in
            --no-setup) no_setup=1 ;;
            --no-modify-path) no_modify_path=1 ;;
            --no-app) no_app=1 ;;
            -h|--help)
                printf '%s\n' 'Usage: sh install.sh [--no-setup] [--no-modify-path] [--no-app]' \
                    '  --no-setup          Install without guided setup (KOIDRIVE_NO_SETUP=1)' \
                    '  --no-modify-path    Leave shell profiles unchanged (KOIDRIVE_NO_MODIFY_PATH=1)' \
                    '  --no-app            Pass --no-app to guided setup on macOS'
                return
                ;;
            *) fail "Unknown option: $option. Run with --help for usage" ;;
        esac
    done
    attended=0
    if [ "${CI+x}" != x ] && [ -t 1 ] && [ -t 2 ] && ( : < /dev/tty ) 2>/dev/null; then
        attended=1
    fi

    case "$(uname -s)" in
        Linux) os=unknown-linux-musl ;;
        Darwin) os=apple-darwin ;;
        *) fail 'Supported systems are Linux and macOS' ;;
    esac
    case "$(uname -m)" in
        x86_64|amd64) arch=x86_64 ;;
        aarch64|arm64) arch=aarch64 ;;
        *) fail 'Supported architectures are x86_64 and arm64' ;;
    esac
    if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ]; then
        [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ] || fail 'macOS requires Apple silicon'
        arch=aarch64
    fi

    required="curl tar mktemp awk grep sed"
    if [ "$os" = apple-darwin ]; then required="$required codesign"; fi
    for command in $required; do
        command -v "$command" >/dev/null 2>&1 || fail "Required command not found: $command"
    done
    if command -v sha256sum >/dev/null 2>&1; then
        checksum=sha256sum
    elif command -v shasum >/dev/null 2>&1; then
        checksum=shasum
    else
        fail 'Install sha256sum or shasum to verify the download'
    fi

    base=${KOIDRIVE_RELEASE_BASE_URL:-https://github.com/koidrive/koidrive/releases}
    case "$base" in https://*) ;; *) fail 'KOIDRIVE_RELEASE_BASE_URL must be an https:// URL' ;; esac
    base=${base%/}
    version=${KOIDRIVE_VERSION:-latest}
    if [ "$version" = latest ]; then
        download=$base/latest/download
    else
        version=${version#v}
        version=$version awk 'BEGIN { exit ENVIRON["version"] !~ /^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$/ }' || fail 'KOIDRIVE_VERSION must be a version such as v0.1.0'
        download=$base/download/v$version
    fi

    archive=koidrive-$arch-$os.tar.gz
    tmp=$(mktemp -d)
    staged=
    trap 'rm -rf "$tmp"; if [ -n "$staged" ]; then rm -f "$staged"; fi' EXIT
    trap 'exit 1' HUP INT TERM
    fetch() {
        curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$tmp/$1" -- "$download/$1"
    }
    printf 'Downloading %s (%s)\n' "$archive" "$version"
    fetch "$archive"
    fetch SHA256SUMS
    fetch SHA256SUMS.sig || fail 'Could not download SHA256SUMS.sig, so the release cannot be verified; the existing installation has not changed. Retry, or set KOIDRIVE_VERSION to v0.1.0-alpha.4 or later'
    if command -v ssh-keygen >/dev/null 2>&1 && ssh-keygen -\? 2>&1 | grep -q 'ssh-keygen -Y verify'; then
        printf '%s\n' 'koidrive namespaces="koidrive-release" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHrojC3pUxk7xAzY+3xU5cpMnCpa2jPxjMu/xH/xL/+n' > "$tmp/allowed-signers"
        ssh-keygen -Y verify -f "$tmp/allowed-signers" -I koidrive -n koidrive-release -s "$tmp/SHA256SUMS.sig" < "$tmp/SHA256SUMS" >/dev/null 2>&1 ||
            fail 'SHA256SUMS is not signed with the Koidrive release key; the existing installation has not changed'
        printf 'Signature verified for SHA256SUMS\n'
    else
        printf 'Warning: ssh-keygen from OpenSSH 8.1 or later was not found, so the release signature was not checked\n' >&2
    fi
    if [ "$version" != latest ]; then
        awk -v name="Koidrive-$version-arm64.dmg" '$2 == name { found = 1 } END { exit !found }' "$tmp/SHA256SUMS" ||
            fail "SHA256SUMS does not belong to v$version; the existing installation has not changed"
    fi
    expected=$(awk -v name="$archive" '$2 == name { count++; hash = $1 } END { if (count == 1) print hash; else exit 1 }' "$tmp/SHA256SUMS") || fail 'SHA256SUMS must contain exactly one entry for the tarball'
    [ "${#expected}" -eq 64 ] || fail 'Invalid SHA256SUMS entry'
    case "$expected" in *[!0-9a-f]*) fail 'Invalid SHA256SUMS entry' ;; esac
    if [ "$checksum" = sha256sum ]; then
        actual=$(sha256sum "$tmp/$archive")
    else
        actual=$(shasum -a 256 "$tmp/$archive")
    fi
    [ "${actual%% *}" = "$expected" ] || fail 'Checksum mismatch; the existing installation has not changed'
    printf 'SHA256 verified for %s\n' "$archive"
    tar -xzOf "$tmp/$archive" koi > "$tmp/koi"
    [ -s "$tmp/koi" ] || fail 'The tarball contains no koi binary'
    if [ "$os" = apple-darwin ]; then
        codesign --verify --strict -R '=identifier "koi" and anchor apple generic and certificate leaf[subject.OU] = "656SPP5XR8" and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists' "$tmp/koi" ||
            fail "The downloaded koi is not signed with Koidrive's Developer ID; the existing installation has not changed"
    fi

    install_dir=${KOIDRIVE_INSTALL_DIR:-$HOME/.local/bin}
    case "$install_dir" in /*) ;; *) fail 'KOIDRIVE_INSTALL_DIR must be an absolute path' ;; esac
    case "$install_dir" in *[[:cntrl:]]*) fail 'KOIDRIVE_INSTALL_DIR cannot contain control characters' ;; esac
    while [ "$install_dir" != / ] && [ "${install_dir%/}" != "$install_dir" ]; do install_dir=${install_dir%/}; done
    [ ! -d "$install_dir/koi" ] || fail 'The destination koi is a directory'
    if [ -f "$install_dir/koi" ] && ! grep -q 'Koidrive sync client' "$install_dir/koi"; then
        fail "$install_dir/koi is another program. Move it away, or install elsewhere with KOIDRIVE_INSTALL_DIR"
    fi
    mkdir -p "$install_dir"
    staged=$(mktemp "$install_dir/.koidrive.XXXXXXXX")
    cat "$tmp/koi" > "$staged"
    chmod 755 "$staged"
    mv -f "$staged" "$install_dir/koi"
    staged=
    printf 'Installed koi to %s/koi\n' "$install_dir"

    shell_quote() {
        printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
    }

    configure_path() {
        quoted_dir=$(shell_quote "$install_dir")
        login_shell=${SHELL:-}
        path_prelude=
        case "${login_shell##*/}" in
            zsh|bash)
                case "${login_shell##*/}" in
                    zsh) path_file=${ZDOTDIR:-$HOME}/.zprofile ;;
                    bash)
                        path_file=$HOME/.bash_profile
                        if [ ! -e "$path_file" ] && { [ -r "$HOME/.bash_login" ] || [ -r "$HOME/.profile" ]; }; then
                            path_prelude="if [ -r \"\$HOME/.bash_login\" ]; then . \"\$HOME/.bash_login\"; elif [ -r \"\$HOME/.profile\" ]; then . \"\$HOME/.profile\"; fi"
                        fi
                        ;;
                esac
                path_marker="; : 'koidrive PATH'"
                path_line="case \":\$PATH:\" in *:${quoted_dir}:*) ;; *) export PATH=${quoted_dir}:\"\$PATH\" ;; esac$path_marker"
                ;;
            fish)
                path_file=${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/koidrive.fish
                path_marker="; true 'koidrive PATH'"
                fish_dir=$(printf '%s' "$install_dir" | sed "s/\\\\/\\\\\\\\/g; s/'/\\\\'/g")
                path_line="contains -- '$fish_dir' \$PATH; or set -gx PATH '$fish_dir' \$PATH$path_marker"
                ;;
            *)
                printf 'Warning: %s is not on PATH. Add it to your shell profile.\n' "$install_dir" >&2
                return
                ;;
        esac
        path_target=$path_file
        path_links=0
        while [ -L "$path_target" ]; do
            path_links=$((path_links + 1))
            [ "$path_links" -le 40 ] || return 1
            path_link=$(readlink "$path_target") || return
            case "$path_link" in
                /*) path_target=$path_link ;;
                *) path_target=$(dirname "$path_target")/$path_link ;;
            esac
        done
        [ ! -e "$path_target" ] || [ -f "$path_target" ] || return 1
        if [ -f "$path_file" ] && grep -Fqx -- "$path_line" "$path_file"; then
            return
        fi
        mkdir -p "$(dirname "$path_target")" || return
        : > "$tmp/removed-lines" || return
        if [ -e "$path_file" ]; then
            KOIDRIVE_PATH_MARKER=$path_marker KOIDRIVE_PATH_REMOVED=$tmp/removed-lines awk '
                BEGIN { marker = ENVIRON["KOIDRIVE_PATH_MARKER"] }
                substr($0, length($0) - length(marker) + 1) == marker {
                    print "- " $0 > ENVIRON["KOIDRIVE_PATH_REMOVED"]
                    next
                }
                { print }
            ' "$path_file" > "$tmp/profile" || return
        else
            : > "$tmp/profile" || return
        fi
        if [ -n "$path_prelude" ]; then
            printf '%s\n' "$path_prelude" >> "$tmp/profile" || return
        fi
        printf '%s\n' "$path_line" >> "$tmp/profile" || return
        staged=$(mktemp "$(dirname "$path_target")/.koidrive-profile.XXXXXXXX") || return
        if [ -e "$path_target" ]; then
            cp -p "$path_target" "$staged" || return
        fi
        cat "$tmp/profile" > "$staged" || return
        mv -f "$staged" "$path_target" || return
        staged=
        printf 'Updated %s:\n' "$path_target"
        cat "$tmp/removed-lines"
        if [ -n "$path_prelude" ]; then
            printf '+ %s\n' "$path_prelude"
        fi
        printf '+ %s\n' "$path_line"
        printf 'Open a new terminal to use koi by name.\n'
    }

    other=$(command -v koi 2>/dev/null || true)
    if [ -z "$other" ] || ! [ "$other" -ef "$install_dir/koi" ]; then
        case ":${PATH:-}:" in
            *":$install_dir:"*) ;;
            *)
                if [ "$attended" = 1 ] && [ "$no_modify_path" != 1 ]; then
                    configure_path || printf 'Warning: could not update PATH. Add %s to your shell profile.\n' "$install_dir" >&2
                else
                    printf 'Warning: %s is not on PATH. Add it to your shell profile.\n' "$install_dir" >&2
                fi
                ;;
        esac
        if [ -z "$other" ]; then
            :
        elif grep -q 'Koidrive sync client' "$other" 2>/dev/null; then
            printf 'Warning: koi on PATH runs %s, another Koidrive install. Put %s before it on PATH.\n' "$other" "$install_dir" >&2
        else
            printf 'Warning: koi on PATH runs %s, another program. Put %s before it on PATH.\n' "$other" "$install_dir" >&2
        fi
    fi

    restart() {
        service=$1
        shift
        if "$@"; then
            printf 'Restarted the background service %s\n' "$service"
        else
            printf 'Warning: koi is installed, but the background service %s did not restart. Run: %s\n' "$service" "$*" >&2
        fi
    }
    this_koi() {
        case "$1" in /*) ;; *) return 2 ;; esac
        [ "$1" -ef "$install_dir/koi" ] && return 0
        [ -e "$1" ] || return 2
        return 1
    }
    process_program() {
        case "$os" in
            apple-darwin) program=$(LC_ALL=en_US.UTF-8 ps -ww -o args= -p "$1" 2>/dev/null) && printf '%s\n' "${program% _daemon}" ;;
            *) program=$(readlink "/proc/$1/exe" 2>/dev/null) && printf '%s\n' "${program% (deleted)}" ;;
        esac
    }
    unknown() {
        printf 'Warning: koi could not tell which koi the background service %s runs, so it did not restart it. If it runs %s/koi, run: %s\n' "$1" "$install_dir" "$2" >&2
    }
    if [ "$os" = apple-darwin ] && agents=$(launchctl print "gui/$(id -u)" 2>/dev/null); then
        printf '%s\n' "$agents" | awk '$1 == "services" && $2 == "=" {inside = 1; next} inside && $1 == "}" {inside = 0} inside && $1 ~ /^[1-9][0-9]*$/ && ($3 == "dev.koidrive.koi" || $3 ~ /^dev\.koidrive\.koi\.[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]$/) {print $1, $3}' |
            while read -r _ label; do
                if this_koi "$(launchctl print "gui/$(id -u)/$label" 2>/dev/null | LC_ALL=C awk '$1 == "program" && $2 == "=" { sub(/^[^=]*= /, ""); print; exit }')"; then
                    restart "$label" launchctl kickstart -k "gui/$(id -u)/$label"
                elif [ $? = 2 ]; then
                    unknown "$label" "launchctl kickstart -k gui/$(id -u)/$label"
                fi
            done
    elif [ "$os" = unknown-linux-musl ] && units=$(systemctl --user list-units --type=service --state=active --plain --no-legend 'koidrive.service' 'koidrive-*.service' 2>/dev/null); then
        for unit in $(printf '%s\n' "$units" | awk '$1 == "koidrive.service" || $1 ~ /^koidrive-[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]\.service$/ {print $1}'); do
            if this_koi "$(process_program "$(systemctl --user show --property=MainPID --value "$unit" 2>/dev/null)")"; then
                restart "$unit" systemctl --user restart "$unit"
            elif [ $? = 2 ]; then
                unknown "$unit" "systemctl --user restart $unit"
            fi
        done
    else
        if [ "$os" = apple-darwin ]; then
            daemons=$(LC_ALL=en_US.UTF-8 ps -ww -u "$(id -u)" -o pid= -o args= 2>/dev/null | awk '{pid = $1; sub(/^ *[0-9]+ +/, "")} $0 == "koi _daemon" || $0 ~ /\/koi _daemon$/ {print pid}')
        else
            daemons=$(for cmdline in /proc/[0-9]*/cmdline; do
                [ "$(tr '\000' '\n' < "$cmdline" 2>/dev/null | sed -n '2p;3q')" = _daemon ] && printf '%s\n' "${cmdline%/cmdline}"
            done | sed 's|^/proc/||')
        fi
        for pid in $daemons; do
            if this_koi "$(process_program "$pid")"; then
                :
            elif [ $? = 2 ] && [ "$os" = apple-darwin ]; then
                printf 'Warning: koi is installed, but koi could not tell which koi the daemon with process ID %s runs. If it runs the old koi, restart it with the service manager that started it.\n' "$pid" >&2
                continue
            else
                continue
            fi
            unit=$(ps -ww -o cgroup= -p "$pid" 2>/dev/null | tr ';' '\n' | sed -n 's|.*/\(koidrive[-0-9a-f]*\.service\)$|\1|p' | head -n 1)
            if [ -n "$unit" ]; then
                printf 'Warning: koi is installed, but the background service %s still runs the old koi, because this shell cannot reach the systemd user manager. Log in as %s and run: systemctl --user restart %s\n' "$unit" "$(id -un)" "$unit" >&2
            else
                printf 'Warning: koi is installed, but the koi daemon with process ID %s still runs the old koi. Restart it with the service manager that started it.\n' "$pid" >&2
            fi
        done
    fi

    set -- setup
    if [ "$no_app" = 1 ]; then set -- "$@" --no-app; fi
    if [ "$os" = apple-darwin ] && [ "$attended" = 1 ] && [ "$no_setup" != 1 ]; then
        printf 'Starting guided setup with %s/koi\n' "$install_dir"
        "$install_dir/koi" "$@" < /dev/tty
    else
        printf 'To set up Koidrive, run: %s/koi %s\n' "$(shell_quote "$install_dir")" "$*"
    fi
}

main "$@"
