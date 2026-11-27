#!/usr/bin/env bash
# Completion gate: formatting, lints, tests. Exits non-zero on the first failure.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo fmt --all --check
cargo clippy --workspace --all-targets --features embedded -- -D warnings
cargo test --workspace --features embedded
