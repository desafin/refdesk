#!/usr/bin/env bash
# Writes THIRD-PARTY-NOTICES.md: what the refdesk executable contains besides its own code,
# with versions and licenses. Rust crates come from `cargo tree` (normal dependencies for this
# platform, without proc-macros and build scripts, which do not end up in the executable);
# MuPDF, the libraries it compiles in and the fonts are read from the mupdf-sys,
# freetype-sys and epaint_default_fonts sources that cargo downloaded.
#
#   scripts/third-party-notices.sh [--check] [--features LIST]
#
# --check compares instead of writing and fails when the file is out of date.
# --features is passed to cargo (default: embedded, the build that is released).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

check=0
features="embedded"
while [ "$#" -gt 0 ]; do
    case "$1" in
        --check) check=1 ;;
        --features)
            if [ "$#" -lt 2 ]; then
                echo "third-party-notices.sh: --features needs a value" >&2
                exit 2
            fi
            features="$2"
            shift
            ;;
        --features=*) features="${1#--features=}" ;;
        -h | --help)
            sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "third-party-notices.sh: unknown option: $1" >&2
            exit 2
            ;;
    esac
    shift
done

feature_args=()
if [ -n "$features" ]; then
    feature_args=(--features "$features")
fi

out="$root/THIRD-PARTY-NOTICES.md"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

# Source directory of a downloaded crate, from `cargo metadata`.
crate_dir() {
    local manifest
    manifest="$(cargo metadata --format-version 1 --locked "${feature_args[@]}" 2>/dev/null \
        | grep -o "\"manifest_path\":\"[^\"]*/$1-[0-9][^\"/]*/Cargo.toml\"" | head -n 1 \
        | sed 's/^"manifest_path":"//; s/"$//')"
    if [ -z "$manifest" ]; then
        echo "third-party-notices.sh: crate $1 not found in cargo metadata" >&2
        exit 1
    fi
    dirname "$manifest"
}

# Value of a `#define NAME value` line.
define_of() {
    sed -n "s/^#define[[:space:]]\{1,\}$2[[:space:]]\{1,\}//p" "$1" | head -n 1 | tr -d '"()' \
        | sed 's/[[:space:]]*$//'
}

freetype_version() {
    local h="$1/include/freetype/freetype.h"
    echo "$(define_of "$h" FREETYPE_MAJOR).$(define_of "$h" FREETYPE_MINOR).$(define_of "$h" FREETYPE_PATCH)"
}

mupdf_sys="$(crate_dir mupdf-sys)"
freetype_sys="$(crate_dir freetype-sys)"
default_fonts="$(crate_dir epaint_default_fonts)"
mupdf_src="$mupdf_sys/mupdf"
third="$mupdf_src/thirdparty"

mupdf_version="$(define_of "$mupdf_src/include/mupdf/fitz/version.h" FZ_VERSION)"
mupdf_freetype="$(freetype_version "$third/freetype")"
jbig2dec="$(define_of "$third/jbig2dec/jbig2.h" JBIG2_VERSION_MAJOR).$(define_of "$third/jbig2dec/jbig2.h" JBIG2_VERSION_MINOR)"
lcms_raw="$(define_of "$third/lcms2/include/lcms2mt.h" LCMS_VERSION | sed 's/ - 2000//')"
lcms2="$((lcms_raw / 1000)).$(((lcms_raw % 1000) / 10))"
libjpeg="$(define_of "$third/libjpeg/jversion.h" JVERSION | awk '{ print $1 }')"
openjpeg="$(grep -rh '#define OPJ_PACKAGE_VERSION' "$third/openjpeg" | head -n 1 | sed 's/.*"\(.*\)".*/\1/')"
zlib="$(define_of "$third/zlib/zlib.h" ZLIB_VERSION)"
crate_freetype="$(freetype_version "$freetype_sys/freetype2")"
mupdf_sys_version="$(basename "$mupdf_sys" | sed 's/^mupdf-sys-//')"
freetype_sys_version="$(basename "$freetype_sys" | sed 's/^freetype-sys-//')"
default_fonts_version="$(basename "$default_fonts" | sed 's/^epaint_default_fonts-//')"

for value in "$mupdf_version" "$mupdf_freetype" "$jbig2dec" "$lcms2" "$libjpeg" "$openjpeg" "$zlib" \
    "$crate_freetype"; do
    if [ -z "$value" ] || [[ "$value" == *..* ]]; then
        echo "third-party-notices.sh: could not read a bundled library version (got '$value')" >&2
        exit 1
    fi
done

# The base 14 fonts are compiled in unless mupdf-sys prunes them (feature base14-fonts).
base14="$(cd "$mupdf_src/resources/fonts/urw" && ls -- *.cff | grep -v '^NimbusBoxes' | sed 's/\.cff$//' \
    | LC_ALL=C sort | paste -sd, | sed 's/,/, /g')"

# The table below lists what MuPDF compiles in with mupdf-sys' format features off. Stop if a
# feature that adds libraries (HarfBuzz, Gumbo, MuJS, Brotli, extract, ...) gets switched on.
sys_features="$(cargo tree --color never --locked -p refdesk "${feature_args[@]}" -e normal --prefix none --format '{p}|{f}' \
    | sed -n 's/^mupdf-sys v[^|]*|//p' | head -n 1)"
for feature in ${sys_features//,/ }; do
    case "$feature" in
        base14-fonts | zerocopy) ;;
        *)
            echo "third-party-notices.sh: mupdf-sys feature '$feature' is on; update the MuPDF table" >&2
            exit 1
            ;;
    esac
done

crates="$(cargo tree --color never --locked -p refdesk "${feature_args[@]}" -e normal,no-proc-macro --prefix none \
    --format '{p}|{l}|{r}' \
    | sed 's/ (\*)$//; s/ (proc-macro)//' \
    | grep -v '^refdesk\(-core\)\? v' \
    | LC_ALL=C sort -u)"
crate_count="$(printf '%s\n' "$crates" | grep -c .)"

{
    cat <<EOF
# Third-party notices

The refdesk executable (Linux x86_64, default features${features:+; here with features \`$features\`})
contains the following software besides refdesk itself. refdesk is licensed under the
GNU Affero General Public License v3.0 or later (\`LICENSE\`); the source code of everything
listed here is available at the given locations or from crates.io.

This file is generated by \`scripts/third-party-notices.sh\`. Do not edit it by hand.

## MuPDF and the libraries it compiles in

MuPDF is compiled from source by the \`mupdf-sys\` crate $mupdf_sys_version (features: ${sys_features:-none}).
Its XPS, SVG, CBZ, image, HTML, EPUB, JavaScript, Brotli and DOCX parts are switched off, so
only the libraries below are compiled into the executable.

| Component | Version | License | Source |
|---|---|---|---|
| MuPDF | $mupdf_version | AGPL-3.0-or-later | https://mupdf.com/ (Artifex Software, Inc.) |
| FreeType (MuPDF's copy) | $mupdf_freetype | FTL (FreeType License; dual-licensed FTL or GPL-2.0-or-later) | https://freetype.org/ |
| jbig2dec | $jbig2dec | AGPL-3.0-or-later | https://jbig2dec.com/ |
| lcms2mt (Little CMS, Artifex thread-safe fork) | $lcms2 | MIT | https://github.com/ArtifexSoftware/thirdparty-lcms2 |
| libjpeg (Independent JPEG Group) | $libjpeg | IJG | https://ijg.org/ |
| OpenJPEG | $openjpeg | BSD-2-Clause | https://www.openjpeg.org/ |
| zlib | $zlib | Zlib | https://zlib.net/ |

The \`freetype-sys\` crate $freetype_sys_version (used by font-kit to find system fonts) compiles
another copy of FreeType $crate_freetype into the executable, also under the FreeType License.

Portions of this software are copyright © The FreeType Project (https://freetype.org/).
All rights reserved.

This software is based in part on the work of the Independent JPEG Group.

## Fonts

| Fonts | Where from | License |
|---|---|---|
| URW++ base 14 fonts: $base14 | MuPDF $mupdf_version (\`resources/fonts/urw\`), Copyright URW++ Design & Development | AGPL-3.0, as distributed by Artifex with MuPDF |
| Ubuntu Light | \`epaint_default_fonts\` $default_fonts_version (egui's built-in fonts) | Ubuntu Font Licence 1.0 |
| Hack Regular | \`epaint_default_fonts\` $default_fonts_version | MIT (Hack), Bitstream Vera License (Bitstream Vera Sans Mono) |
| Noto Emoji Regular | \`epaint_default_fonts\` $default_fonts_version | OFL-1.1 |
| emoji-icon-font, egui-icons (a subset of it) | \`epaint_default_fonts\` $default_fonts_version | MIT (Copyright 2014 John Slegers) |

Other fonts (for example Korean text) are not bundled; they are found on the system through
fontconfig at run time.

## Rust crates

$crate_count crates, from \`cargo tree -p refdesk -e normal,no-proc-macro\`. License texts and
copyright notices are in each crate's source package.

| Crate | Version | License | Repository |
|---|---|---|---|
EOF
    printf '%s\n' "$crates" | while IFS='|' read -r pkg license repo; do
        name="${pkg% v*}"
        version="${pkg##* v}"
        printf '| %s | %s | %s | %s |\n' "$name" "$version" "${license:-see the crate}" "${repo:-—}"
    done
} >"$tmp"

if [ "$check" -eq 1 ]; then
    if ! diff -u "$out" "$tmp"; then
        echo "third-party-notices.sh: $out is out of date; run scripts/third-party-notices.sh" >&2
        exit 1
    fi
    echo "third-party-notices.sh: $out is up to date"
else
    cp "$tmp" "$out"
    echo "third-party-notices.sh: wrote $out ($crate_count crates)"
fi
