#!/usr/bin/env bash
# Prepares a fresh Ubuntu 24.04 machine for scripts/check.sh and scripts/package.sh:
# installs the apt packages in packaging/build-deps.txt and the Rust toolchain named in
# rust-toolchain.toml. Used as is by the GitHub workflows and by scripts/verify-in-container.sh,
# so what the container verifies is what CI runs.
#
#   packaging/setup-build-env.sh
#
# Runs apt-get through sudo unless it is already root. Installs rustup (no default toolchain)
# if it is missing. On GitHub Actions it adds ~/.cargo/bin to GITHUB_PATH.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

mapfile -t packages < <(sed -e 's/#.*//' -e 's/[[:space:]]//g' "$root/packaging/build-deps.txt" | grep -v '^$')
echo "setup-build-env: apt packages: ${packages[*]}"

sudo=()
if [ "$(id -u)" -ne 0 ]; then
    sudo=(sudo)
fi
export DEBIAN_FRONTEND=noninteractive
"${sudo[@]}" apt-get update -q
"${sudo[@]}" apt-get install -y -q --no-install-recommends "${packages[@]}"

if ! command -v rustup >/dev/null 2>&1; then
    if [ -x "$HOME/.cargo/bin/rustup" ]; then
        export PATH="$HOME/.cargo/bin:$PATH"
    else
        echo "setup-build-env: installing rustup"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --profile minimal --default-toolchain none --no-modify-path
        export PATH="$HOME/.cargo/bin:$PATH"
    fi
fi
if [ -n "${GITHUB_PATH:-}" ]; then
    echo "$HOME/.cargo/bin" >>"$GITHUB_PATH"
fi

toolchain_file="$root/rust-toolchain.toml"
channel="$(sed -n 's/^channel *= *"\(.*\)".*/\1/p' "$toolchain_file")"
components="$(sed -n 's/^components *= *\[\(.*\)\].*/\1/p' "$toolchain_file" | tr -d '" ')"
if [ -z "$channel" ]; then
    echo "setup-build-env: no channel in $toolchain_file" >&2
    exit 1
fi
echo "setup-build-env: Rust $channel with components: ${components:-none}"
rustup toolchain install "$channel" --profile minimal ${components:+--component "$components"}

(cd "$root" && rustc --version && cargo --version)
