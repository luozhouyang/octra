#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ "${OCTRA_SKIP_WASM_TESTS:-0}" == "1" ]]; then
    echo "Skipping WASM plugin tests (OCTRA_SKIP_WASM_TESTS=1)"
    exit 0
fi

if ! command -v cargo-component >/dev/null 2>&1; then
    echo "cargo-component is required: cargo install cargo-component --locked --version 0.21.1" >&2
    exit 1
fi

cargo component build --release \
    --manifest-path examples/plugins/consumer-quota/Cargo.toml

cargo test -p octra-api-plugin-wasm -- --ignored --test-threads=1
cargo test -p octra-api-gateway --test plugins -- \
    --ignored wasm_admin:: --test-threads=1
