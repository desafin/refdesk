#!/usr/bin/env bash
# Prints the body of the "## [VERSION] - DATE" section of CHANGELOG.md (the release notes).
#
#   packaging/release-notes.sh VERSION [--require-date]
#
# Fails when there is no such section or it is empty. With --require-date it also fails while
# the section still says "Unreleased" instead of a date (the release workflow uses this).
set -euo pipefail

if [ "$#" -lt 1 ]; then
    echo "usage: release-notes.sh VERSION [--require-date]" >&2
    exit 2
fi
version="$1"
require_date=0
if [ "${2:-}" = --require-date ]; then
    require_date=1
fi
changelog="$(cd "$(dirname "$0")/.." && pwd)/CHANGELOG.md"

heading="$(grep -m 1 -F "## [$version]" "$changelog" || true)"
if [ -z "$heading" ]; then
    echo "release-notes.sh: CHANGELOG.md has no section for $version" >&2
    exit 1
fi
if [ "$require_date" -eq 1 ] && ! [[ "$heading" =~ ^"## [$version] - "[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]]; then
    echo "release-notes.sh: the $version section is not dated yet: '$heading'" >&2
    echo "release-notes.sh: write the release date as '## [$version] - YYYY-MM-DD'" >&2
    exit 1
fi

body="$(awk -v head="$heading" '
    $0 == head { inside = 1; next }
    inside && /^## \[/ { exit }
    inside && /^\[[^]]+\]: / { exit }
    inside { print }
' "$changelog" | sed -e '/./,$!d')"
if [ -z "${body//[[:space:]]/}" ]; then
    echo "release-notes.sh: the $version section of CHANGELOG.md is empty" >&2
    exit 1
fi
printf '%s\n' "$body"
