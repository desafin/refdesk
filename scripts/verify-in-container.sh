#!/usr/bin/env bash
# Checks the build and the bundles in clean ubuntu:24.04 containers (needs docker):
#
#   1. build: copy the source tree (tracked and untracked files, no target/ or dist/), run
#      packaging/setup-build-env.sh, scripts/check.sh, scripts/third-party-notices.sh --check
#      and scripts/package.sh — the same steps as the GitHub workflows. The bundles land in
#      target/container/dist/.
#   2. deb: `apt-get install ./refdesk_<version>_amd64.deb` in another fresh container, then
#      `refdesk --version` and `refdesk --help`.
#   3. tar.gz: unpack in a third fresh container (no extra packages), `install.sh --prefix`,
#      the same two commands, `install.sh` to ~/.local and `uninstall.sh`.
#
#   scripts/verify-in-container.sh [--install-only] [--image IMAGE]
#
# --install-only skips step 1 and checks the bundles already in target/container/dist/.
# Containers are removed afterwards; the image stays.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

image="ubuntu:24.04"
install_only=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --install-only) install_only=1 ;;
        --image)
            if [ "$#" -lt 2 ]; then
                echo "verify-in-container.sh: --image needs a value" >&2
                exit 2
            fi
            image="$2"
            shift
            ;;
        -h | --help)
            sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "verify-in-container.sh: unknown option: $1" >&2
            exit 2
            ;;
    esac
    shift
done

if ! command -v docker >/dev/null 2>&1; then
    echo "verify-in-container.sh: docker is not available" >&2
    exit 1
fi

out="$root/target/container"
dist="$out/dist"
containers=()
cleanup() {
    if [ "${#containers[@]}" -gt 0 ]; then
        docker rm -f "${containers[@]}" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

step() {
    printf '\n=== %s (%s)\n' "$1" "$(date '+%H:%M:%S')"
}

docker pull -q "$image" >/dev/null
started=$SECONDS

if [ "$install_only" -eq 0 ]; then
    step "1. build, gate and package in a clean $image"
    rm -rf "$out"
    mkdir -p "$out"
    name="refdesk-verify-build-$$"
    containers+=("$name")
    # No .git goes in; the last commit time is passed for the bundles' file times.
    epoch="$(git log -1 --format=%ct)"
    docker create --name "$name" -w /src -e CARGO_TERM_COLOR=never -e SOURCE_DATE_EPOCH="$epoch" \
        "$image" bash -euo pipefail -c '
        start=$SECONDS
        packaging/setup-build-env.sh
        export PATH="$HOME/.cargo/bin:$PATH"
        echo "setup took $((SECONDS - start)) s"
        t=$SECONDS
        scripts/check.sh
        echo "check.sh took $((SECONDS - t)) s"
        scripts/third-party-notices.sh --check
        t=$SECONDS
        scripts/package.sh
        echo "package.sh took $((SECONDS - t)) s"
        dpkg-deb -I dist/*.deb
        dpkg-deb -c dist/*.deb
        tar -tzvf dist/*.tar.gz
        echo "build container total $((SECONDS - start)) s"
    ' >/dev/null
    # Only what git knows about (tracked and untracked, not ignored): no target/, dist/ or
    # references/.
    git ls-files -z --cached --others --exclude-standard \
        | while IFS= read -r -d '' file; do [ -e "$file" ] && printf '%s\0' "$file"; done \
        | tar -c --null -T - -f - \
        | docker cp - "$name:/src"
    docker start -a "$name" 2>&1 | sed 's/^/  | /' || true
    status="$(docker inspect -f '{{.State.ExitCode}}' "$name")"
    if [ "$status" != 0 ]; then
        echo "verify-in-container.sh: the build container failed (exit $status)" >&2
        exit 1
    fi
    docker cp "$name:/src/dist" "$out/"
    echo "build step took $((SECONDS - started)) s"
fi

deb="$(ls "$dist"/refdesk_*_amd64.deb 2>/dev/null | head -n 1)"
tarball="$(ls "$dist"/refdesk-*-x86_64-linux.tar.gz 2>/dev/null | head -n 1)"
if [ -z "$deb" ] || [ -z "$tarball" ]; then
    echo "verify-in-container.sh: no bundles in $dist" >&2
    exit 1
fi
(cd "$dist" && sha256sum -c SHA256SUMS)

step "2. apt-get install ./$(basename "$deb") in a clean $image"
t=$SECONDS
name="refdesk-verify-deb-$$"
containers+=("$name")
docker run --name "$name" -v "$dist:/dist:ro" -e DEBIAN_FRONTEND=noninteractive "$image" \
    bash -euo pipefail -c '
        apt-get update -q >/dev/null
        cp /dist/refdesk_*_amd64.deb /tmp/
        apt-get install -y -q /tmp/refdesk_*_amd64.deb 2>&1 | tail -n 3
        echo "--- installed packages: $(dpkg-query -W | wc -l)"
        echo "--- which refdesk: $(command -v refdesk)"
        echo "--- refdesk --version"; refdesk --version
        echo "--- refdesk --help"; refdesk --help
        echo "--- dpkg -L refdesk"; dpkg -L refdesk
        apt-get remove -y -q refdesk >/dev/null
        if [ -e /usr/bin/refdesk ]; then echo "still installed after remove" >&2; exit 1; fi
        echo "--- removed cleanly"
    ' 2>&1 | sed 's/^/  | /'
echo "deb step took $((SECONDS - t)) s"

step "3. $(basename "$tarball") with install.sh in a clean $image (no extra packages)"
t=$SECONDS
name="refdesk-verify-tar-$$"
containers+=("$name")
docker run --name "$name" -v "$dist:/dist:ro" "$image" bash -euo pipefail -c '
        cd /tmp
        tar -xzf /dist/refdesk-*-x86_64-linux.tar.gz
        bundle="$(ls -d /tmp/refdesk-*/)"
        echo "--- bundle:"; (cd "$bundle" && find . -type f | sort)
        "$bundle/install.sh" --prefix /opt/refdesk
        echo "--- /opt/refdesk/bin/refdesk --version"; /opt/refdesk/bin/refdesk --version
        echo "--- /opt/refdesk/bin/refdesk --help"; /opt/refdesk/bin/refdesk --help
        grep "^Exec=" /opt/refdesk/share/applications/refdesk.desktop
        "$bundle/uninstall.sh" --prefix /opt/refdesk
        test ! -e /opt/refdesk/bin/refdesk
        echo "--- install.sh (default prefix ~/.local)"
        "$bundle/install.sh"
        "$HOME/.local/bin/refdesk" --version
        "$bundle/uninstall.sh"
        test ! -e "$HOME/.local/bin/refdesk"
        echo "--- installed, run and removed with both prefixes"
    ' 2>&1 | sed 's/^/  | /'
echo "tar.gz step took $((SECONDS - t)) s"

step "done"
ls -l "$dist"
echo "total $((SECONDS - started)) s"
