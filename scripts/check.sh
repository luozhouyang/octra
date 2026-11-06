#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

if command -v living-docs >/dev/null 2>&1; then
    living-docs check .livingdocs
else
    echo "living-docs not installed; skipping the living-docs gate" >&2
fi

cargo fmt --all --check
cargo clippy --workspace --exclude octra-desktop --all-targets -- -D warnings
cargo test --workspace --exclude octra-desktop

if [[ "${OCTRA_SKIP_WASM_TESTS:-0}" != "1" ]]; then
    bash scripts/test-wasm.sh
fi

pnpm typecheck
pnpm test
pnpm build
