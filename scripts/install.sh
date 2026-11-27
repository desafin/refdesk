#!/usr/bin/env bash
# Installs refdesk for the current user: the executable, the .desktop entry and the icon.
#
#   install.sh [--prefix DIR] [--set-default] [--dry-run]
#
# From the source tree (scripts/install.sh) it builds the release executable first; from an
# unpacked release bundle (refdesk-<version>/install.sh) it installs the bundled one.
# Files go to DIR/bin, DIR/share/applications and DIR/share/icons (default DIR: ~/.local).
# The default PDF viewer is left alone unless --set-default is given.
# --dry-run only prints what would happen and changes nothing (the build is skipped too).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"

set_default=0
dry_run=0
prefix="$HOME/.local"
while [ "$#" -gt 0 ]; do
    case "$1" in
        --set-default) set_default=1 ;;
        --dry-run) dry_run=1 ;;
        --prefix)
            if [ "$#" -lt 2 ] || [ -z "$2" ]; then
                echo "install.sh: --prefix needs a directory" >&2
                exit 2
            fi
            prefix="$2"
            shift
            ;;
        --prefix=*) prefix="${1#--prefix=}" ;;
        -h | --help)
            sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "install.sh: unknown option: $1" >&2
            exit 2
            ;;
    esac
    shift
done
case "$prefix" in
    /*) ;;
    *) prefix="$PWD/$prefix" ;;
esac

# A release bundle has the executable and the data next to this script.
if [ -f "$here/bin/refdesk" ] && [ -f "$here/share/applications/refdesk.desktop" ]; then
    bundle=1
    binary="$here/bin/refdesk"
    desktop="$here/share/applications/refdesk.desktop"
    icon="$here/share/icons/hicolor/scalable/apps/refdesk.svg"
else
    bundle=0
    root="$(cd "$here/.." && pwd)"
    binary="${CARGO_TARGET_DIR:-$root/target}/release/refdesk"
    desktop="$root/assets/refdesk.desktop"
    icon="$root/assets/refdesk.svg"
fi

bin_dir="$prefix/bin"
apps_dir="$prefix/share/applications"
icons_dir="$prefix/share/icons/hicolor/scalable/apps"

# Prints the command in dry-run mode, runs it otherwise.
run() {
    if [ "$dry_run" -eq 1 ]; then
        printf 'would run:'
        printf ' %q' "$@"
        printf '\n'
    else
        "$@"
    fi
}

if [ "$dry_run" -eq 1 ]; then
    echo "dry run: nothing will be changed"
fi
if [ "$bundle" -eq 1 ]; then
    echo "installing the bundled executable from $here"
elif [ "$dry_run" -eq 1 ]; then
    echo "would run: cargo build --release -p refdesk (skipped in a dry run)"
else
    (cd "$root" && cargo build --release -p refdesk)
    if [ ! -x "$binary" ]; then
        echo "install.sh: build finished but $binary is missing" >&2
        exit 1
    fi
fi

in_path=0
case ":$PATH:" in
    *":$bin_dir:"*) in_path=1 ;;
esac

run install -d "$bin_dir" "$apps_dir" "$icons_dir"
run install -m 755 "$binary" "$bin_dir/refdesk"
if [ "$in_path" -eq 1 ]; then
    run install -m 644 "$desktop" "$apps_dir/refdesk.desktop"
else
    # The entry runs `refdesk` through PATH; point it at the installed file instead.
    # Desktop Entry Exec quoting: double quotes, with `"`, `` ` ``, `$` and `\` escaped.
    quoted="$(printf '%s' "$bin_dir/refdesk" | sed 's/[\\"`$]/\\\\&/g')"
    if [ "$dry_run" -eq 1 ]; then
        echo "would write: $apps_dir/refdesk.desktop (Exec=\"$quoted\" %F)"
    else
        tmp="$(mktemp)"
        EXEC_LINE="Exec=\"$quoted\" %F" \
            awk '/^Exec=/ { print ENVIRON["EXEC_LINE"]; next } { print }' "$desktop" >"$tmp"
        install -m 644 "$tmp" "$apps_dir/refdesk.desktop"
        rm -f "$tmp"
    fi
fi
run install -m 644 "$icon" "$icons_dir/refdesk.svg"

if command -v update-desktop-database >/dev/null 2>&1; then
    run update-desktop-database "$apps_dir"
fi

if [ "$set_default" -eq 1 ]; then
    run xdg-mime default refdesk.desktop application/pdf
else
    echo "the default PDF viewer is unchanged (use --set-default to make refdesk the default)"
fi

if [ "$in_path" -eq 0 ]; then
    echo "note: $bin_dir is not in PATH; the .desktop entry runs $bin_dir/refdesk directly" >&2
fi

if [ "$dry_run" -eq 1 ]; then
    echo "dry run finished: nothing was changed"
else
    echo "installed refdesk to $bin_dir/refdesk"
fi
