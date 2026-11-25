#!/usr/bin/env bash
# Removes what scripts/install.sh installed.
#
#   scripts/uninstall.sh [--dry-run]
#
# The default PDF viewer setting and the user's data (~/.config/refdesk, ~/.local/state/refdesk)
# are not touched. --dry-run only prints what would happen.
set -euo pipefail

dry_run=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=1 ;;
        -h | --help)
            sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "uninstall.sh: unknown option: $arg" >&2
            exit 2
            ;;
    esac
done

bin_dir="$HOME/.local/bin"
apps_dir="$HOME/.local/share/applications"
icons_dir="$HOME/.local/share/icons/hicolor/scalable/apps"

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
