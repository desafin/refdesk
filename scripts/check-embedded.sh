#!/usr/bin/env bash
# Gate for the optional embedded translation engine (WP-83): formatting, lints, build and tests
# with the `embedded-vulkan` feature. The default gate (scripts/check.sh) does not build it.
#
# Needs cmake, libclang, a C++ compiler, glslc, libvulkan-dev and SPIRV-Headers. Without the
# spirv-headers package, point SPIRV_PREFIX at a prefix where SPIRV-Headers is installed
# (`cmake --install` of KhronosGroup/SPIRV-Headers); it is passed to CMake and the C++ compiler.
#
# REFDESK_TEST_GGUF=/path/model.gguf also runs the tests that load a real model (one at a time,
# a few GB of GPU memory). Without it those tests say they were skipped.
set -euo pipefail

cd "$(dirname "$0")/.."

if ! dpkg -s spirv-headers >/dev/null 2>&1 && [ -n "${SPIRV_PREFIX:-}" ]; then
    echo "check-embedded: SPIRV-Headers from $SPIRV_PREFIX"
    export CMAKE_PREFIX_PATH="$SPIRV_PREFIX${CMAKE_PREFIX_PATH:+:$CMAKE_PREFIX_PATH}"
    export CPLUS_INCLUDE_PATH="$SPIRV_PREFIX/include${CPLUS_INCLUDE_PATH:+:$CPLUS_INCLUDE_PATH}"
fi

if [ -n "${REFDESK_TEST_GGUF:-}" ] && [ -f "$REFDESK_TEST_GGUF" ]; then
    echo "check-embedded: tests with the model $REFDESK_TEST_GGUF"
    model_tests=1
else
    echo "check-embedded: REFDESK_TEST_GGUF not set (or not a file): the real-model tests are skipped"
    model_tests=0
fi

features=(--features embedded-vulkan)

cargo fmt --all --check
cargo clippy --workspace --all-targets "${features[@]}" -- -D warnings
cargo build -p refdesk "${features[@]}"
cargo test --workspace "${features[@]}"
if [ "$model_tests" = 1 ]; then
    # Again on its own with output, so the measured times and translations are visible.
    cargo test -p refdesk-core "${features[@]}" --test translate_embedded -- --nocapture gguf_
fi
