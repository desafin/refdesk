#!/usr/bin/env bash
# Removes what install.sh installed.
#
#   uninstall.sh [--prefix DIR] [--dry-run]
#
# DIR is the one given to install.sh (default: ~/.local). The default PDF viewer setting and
# the user's data (~/.config/refdesk, ~/.local/state/refdesk) are not touched.
# --dry-run only prints what would happen.
set -euo pipefail

dry_run=0
prefix="$HOME/.local"
while [ "$#" -gt 0 ]; do
    case "$1" in
        --dry-run) dry_run=1 ;;
        --prefix)
            if [ "$#" -lt 2 ] || [ -z "$2" ]; then
                echo "uninstall.sh: --prefix needs a directory" >&2
                exit 2
            fi
            prefix="$2"
            shift
            ;;
        --prefix=*) prefix="${1#--prefix=}" ;;
        -h | --help)
            sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "uninstall.sh: unknown option: $1" >&2
            exit 2
            ;;
    esac
    shift
done
case "$prefix" in
    /*) ;;
    *) prefix="$PWD/$prefix" ;;
esac

bin_dir="$prefix/bin"
apps_dir="$prefix/share/applications"
icons_dir="$prefix/share/icons/hicolor/scalable/apps"

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

for file in "$bin_dir/refdesk" "$apps_dir/refdesk.desktop" "$icons_dir/refdesk.svg"; do
    if [ -e "$file" ] || [ -L "$file" ]; then
        run rm -f -- "$file"
    else
        echo "not installed: $file"
    fi
done

if command -v update-desktop-database >/dev/null 2>&1 && [ -d "$apps_dir" ]; then
    run update-desktop-database "$apps_dir"
fi

if command -v xdg-mime >/dev/null 2>&1 \
    && [ "$(xdg-mime query default application/pdf 2>/dev/null)" = "refdesk.desktop" ]; then
    echo "note: refdesk is still the default PDF viewer; pick another one in the system settings" >&2
fi

echo "kept: ~/.config/refdesk and ~/.local/state/refdesk (settings and session)"
if [ "$dry_run" -eq 1 ]; then
    echo "dry run finished: nothing was changed"
fi
