#!/usr/bin/env bash
# Prints the version of the refdesk package (from the workspace Cargo.toml), e.g. 0.1.0.
set -euo pipefail
cd "$(dirname "$0")/.."
pkgid="$(cargo pkgid --offline -p refdesk 2>/dev/null || cargo pkgid -p refdesk)"
version="${pkgid##*[@#]}"
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+ ]]; then
    echo "version.sh: cannot read the version from '$pkgid'" >&2
    exit 1
fi
printf '%s\n' "$version"
