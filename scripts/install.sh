#!/usr/bin/env bash
# Installs refdesk for the current user: the release binary, the .desktop entry and the icon.
#
#   scripts/install.sh [--set-default] [--dry-run]
#
# The default PDF viewer is left alone unless --set-default is given.
# --dry-run only prints what would happen and changes nothing (the build is skipped too).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

set_default=0
dry_run=0
for arg in "$@"; do
    case "$arg" in
        --set-default) set_default=1 ;;
        --dry-run) dry_run=1 ;;
        -h | --help)
            sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "install.sh: unknown option: $arg" >&2
            exit 2
            ;;
    esac
done

bin_dir="$HOME/.local/bin"
apps_dir="$HOME/.local/share/applications"
icons_dir="$HOME/.local/share/icons/hicolor/scalable/apps"
binary="${CARGO_TARGET_DIR:-$root/target}/release/refdesk"

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
    echo "would run: cargo build --release -p refdesk (skipped in a dry run)"
else
    (cd "$root" && cargo build --release -p refdesk)
    if [ ! -x "$binary" ]; then
        echo "install.sh: build finished but $binary is missing" >&2
        exit 1
    fi
fi

run install -d "$bin_dir" "$apps_dir" "$icons_dir"
run install -m 755 "$binary" "$bin_dir/refdesk"
run install -m 644 "$root/assets/refdesk.desktop" "$apps_dir/refdesk.desktop"
run install -m 644 "$root/assets/refdesk.svg" "$icons_dir/refdesk.svg"

if command -v update-desktop-database >/dev/null 2>&1; then
    run update-desktop-database "$apps_dir"
fi

if [ "$set_default" -eq 1 ]; then
    run xdg-mime default refdesk.desktop application/pdf
else
    echo "the default PDF viewer is unchanged (use --set-default to make refdesk the default)"
fi

case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *)
        echo "note: $bin_dir is not in PATH; the .desktop entry runs 'refdesk' and needs it there" >&2
        ;;
esac

if [ "$dry_run" -eq 1 ]; then
    echo "dry run finished: nothing was changed"
else
    echo "installed refdesk to $bin_dir/refdesk"
fi
