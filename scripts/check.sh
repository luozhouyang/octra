#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

cargo fmt --all --check
cargo clippy --workspace --exclude octra-desktop --all-targets -- -D warnings
cargo test --workspace --exclude octra-desktop

pnpm typecheck
pnpm test
pnpm build
