#!/usr/bin/env bash
# Builds the release executable and the downloadable bundles in dist/:
#
#   refdesk-<version>-x86_64-linux.tar.gz   executable, .desktop, icon, install/uninstall scripts,
#                                           LICENSE, README.md, THIRD-PARTY-NOTICES.md
#   refdesk_<version>_amd64.deb             made with dpkg-deb
#   SHA256SUMS
#
#   scripts/package.sh [--features LIST]
#
# The version comes from Cargo.toml. --features is passed to cargo (default: none). File times
# are set to SOURCE_DATE_EPOCH (default: the time of the last commit) and archive members are
# sorted, so the same executable gives the same bundles. Used as is by the release workflow.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

features=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --features)
            if [ "$#" -lt 2 ]; then
                echo "package.sh: --features needs a value" >&2
                exit 2
            fi
            features="$2"
            shift
            ;;
        --features=*) features="${1#--features=}" ;;
        -h | --help)
            sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "package.sh: unknown option: $1" >&2
            exit 2
            ;;
    esac
    shift
done

if [ "$(uname -m)" != x86_64 ]; then
    echo "package.sh: only x86_64 Linux is packaged (this is $(uname -m))" >&2
    exit 1
fi
for tool in dpkg-deb strip sha256sum tar gzip readelf; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "package.sh: $tool is missing (see packaging/build-deps.txt)" >&2
        exit 1
    fi
done

version="$(packaging/version.sh)"
if [ -z "${SOURCE_DATE_EPOCH:-}" ]; then
    SOURCE_DATE_EPOCH="$(git -C "$root" log -1 --format=%ct 2>/dev/null || echo 0)"
fi
export SOURCE_DATE_EPOCH
echo "package.sh: refdesk $version, features: ${features:-none}, SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH"

# freetype-sys links the system libfreetype when pkg-config finds one (the GitHub runner has
# it); keep the bundled copy so the executable needs nothing beyond libc and libgcc.
export FREETYPE2_NO_PKG_CONFIG=1

cargo_args=(build --release --locked -p refdesk)
if [ -n "$features" ]; then
    cargo_args+=(--features "$features")
fi
cargo "${cargo_args[@]}"

target_dir="${CARGO_TARGET_DIR:-$root/target}"
built="$target_dir/release/refdesk"
if [ ! -x "$built" ]; then
    echo "package.sh: build finished but $built is missing" >&2
    exit 1
fi

dist="$root/dist"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
rm -rf "$dist"
mkdir -p "$dist"

strip --strip-all -o "$work/refdesk" "$built"
chmod 755 "$work/refdesk"

# --- Dependencies of the executable -------------------------------------------------------
# Libraries linked directly (DT_NEEDED) map to the packages that ship them. Everything else
# (fontconfig, xkbcommon, Wayland, Vulkan/EGL, X11, D-Bus) is opened with dlopen at run time
# and is listed by hand below.
mapfile -t needed < <(readelf -d "$work/refdesk" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')
echo "package.sh: linked libraries: ${needed[*]}"

package_of() {
    local path="$1" candidate pkg
    for candidate in "$path" "$(readlink -f "$path")" "${path#/usr}" "/usr$path"; do
        # Lines are "pkg[, pkg]: path"; dpkg-divert adds "diversion by pkg from: path".
        pkg="$(dpkg-query -S "$candidate" 2>/dev/null | grep -v '^diversion ' | head -n 1 \
            | cut -d: -f1 | cut -d, -f1 | tr -d ' ')" || true
        if [ -n "$pkg" ]; then
            printf '%s\n' "$pkg"
            return 0
        fi
    done
    return 1
}

linked_packages=()
for lib in "${needed[@]}"; do
    path="$(ldd "$work/refdesk" | awk -v lib="$lib" '$1 == lib && $3 ~ /^\// { print $3 }')"
    if [ -z "$path" ] && [[ "$lib" == ld-linux* ]]; then
        path="$(ldd "$work/refdesk" | awk -v lib="$lib" 'index($1, lib) { print $1 }')"
    fi
    if [ -z "$path" ] || ! pkg="$(package_of "$path")"; then
        echo "package.sh: cannot tell which package provides $lib (${path:-not found})" >&2
        exit 1
    fi
    linked_packages+=("$pkg")
done
glibc="$(readelf --dyn-syms -W "$work/refdesk" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' \
    | sort -t. -k1,1n -k2,2n -k3,3n -u | tail -n 1)"
echo "package.sh: needs glibc >= $glibc"

depends=()
for pkg in $(printf '%s\n' "${linked_packages[@]}" | sort -u); do
    if [ "$pkg" = libc6 ]; then
        depends+=("libc6 (>= $glibc)")
    else
        depends+=("$pkg")
    fi
done
# dlopen()ed and required: fonts are found through fontconfig (mupdf's system fonts and
# eframe's font fallback unwrap the library), winit needs xkbcommon and Wayland, and wgpu
# needs Vulkan or EGL to draw anything.
depends+=(libfontconfig1 libxkbcommon0 libwayland-client0 "libvulkan1 | libegl1")
# dlopen()ed or run, and only needed for some features: wgpu's GL path on Wayland, the GPU
# driver, the file chooser (rfd talks to the desktop portal through libdbus), Korean text,
# opening web links.
recommends=(libwayland-egl1 "mesa-vulkan-drivers | libegl-mesa0" libdbus-1-3 xdg-desktop-portal
    "xdg-desktop-portal-gnome | xdg-desktop-portal-gtk" fonts-noto-cjk xdg-utils)
# winit's X11 backend (only when there is no Wayland session).
suggests=(libx11-6 libx11-xcb1 libxcb1 libxcursor1 libxi6 libxkbcommon-x11-0)

# Joins the arguments with ", " (a control file relation list).
join() {
    local out="" item
    for item in "$@"; do
        out+="${out:+, }$item"
    done
    printf '%s' "$out"
}

# Sets every file and directory under $1 to SOURCE_DATE_EPOCH.
clamp_times() {
    find "$1" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
}

# --- tar.gz ---------------------------------------------------------------------------------
name="refdesk-$version"
stage="$work/tar/$name"
install -d "$stage/bin" "$stage/share/applications" "$stage/share/icons/hicolor/scalable/apps"
install -m 755 "$work/refdesk" "$stage/bin/refdesk"
install -m 644 assets/refdesk.desktop "$stage/share/applications/refdesk.desktop"
install -m 644 assets/refdesk.svg "$stage/share/icons/hicolor/scalable/apps/refdesk.svg"
install -m 755 scripts/install.sh scripts/uninstall.sh "$stage/"
install -m 644 LICENSE README.md THIRD-PARTY-NOTICES.md "$stage/"
clamp_times "$work/tar"

tarball="$dist/refdesk-$version-x86_64-linux.tar.gz"
tar -C "$work/tar" --sort=name --format=gnu --owner=0 --group=0 --numeric-owner \
    --mtime="@$SOURCE_DATE_EPOCH" -cf - "$name" | gzip -9 -n >"$tarball"

# --- deb ------------------------------------------------------------------------------------
deb_root="$work/deb"
doc="$deb_root/usr/share/doc/refdesk"
install -d "$deb_root/DEBIAN" "$deb_root/usr/bin" "$deb_root/usr/share/applications" \
    "$deb_root/usr/share/icons/hicolor/scalable/apps" "$doc"
install -m 755 "$work/refdesk" "$deb_root/usr/bin/refdesk"
install -m 644 assets/refdesk.desktop "$deb_root/usr/share/applications/refdesk.desktop"
install -m 644 assets/refdesk.svg "$deb_root/usr/share/icons/hicolor/scalable/apps/refdesk.svg"
{
    cat packaging/deb/copyright
    echo
    echo "License: AGPL-3.0-or-later"
    sed -e 's/^[[:space:]]*$/./' -e 's/^/ /' LICENSE
} >"$doc/copyright"
chmod 644 "$doc/copyright"
install -m 644 THIRD-PARTY-NOTICES.md "$doc/THIRD-PARTY-NOTICES.md"
gzip -9 -n -c CHANGELOG.md >"$doc/changelog.gz"
chmod 644 "$doc/changelog.gz"

installed_size="$(du -sk --apparent-size "$deb_root/usr" | cut -f1)"
maintainer="${REFDESK_DEB_MAINTAINER:-refdesk project <https://github.com/desafin/refdesk/issues>}"
cat >"$deb_root/DEBIAN/control" <<CONTROL
Package: refdesk
Version: $version
Architecture: amd64
Maintainer: $maintainer
Installed-Size: $installed_size
Depends: $(join "${depends[@]}")
Recommends: $(join "${recommends[@]}")
Suggests: $(join "${suggests[@]}")
Section: text
Priority: optional
Homepage: https://github.com/desafin/refdesk
Description: viewer for reading several software documentation PDFs at once
 refdesk opens many technical PDFs side by side in tabs, splits and windows,
 follows internal links with back and forward, searches across every open
 document, compares two versions of a document and remembers where you were
 reading. Built with Rust, MuPDF and egui for GNOME on Wayland.
CONTROL
chmod 644 "$deb_root/DEBIAN/control"
clamp_times "$deb_root"

deb="$dist/refdesk_${version}_amd64.deb"
dpkg-deb --root-owner-group -Zxz --build "$deb_root" "$deb" >/dev/null

# --- checksums ------------------------------------------------------------------------------
(cd "$dist" && sha256sum -- "$(basename "$tarball")" "$(basename "$deb")" >SHA256SUMS)

echo "package.sh: executable $(stat -c %s "$work/refdesk") bytes (unstripped $(stat -c %s "$built"))"
(cd "$dist" && ls -l -- *)
