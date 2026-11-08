#!/usr/bin/env bash
set -euo pipefail

# Builds the Octra daemon and stages it under the name Tauri expects for the
# `externalBin` bundle entry. Binaries are build artifacts and are git-ignored.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET=""
if [[ $# -gt 0 ]]; then
    case "$1" in
        --target=*)
            TARGET="${1#--target=}"
            ;;
        --target)
            TARGET="${2:-}"
            ;;
        *)
            TARGET="$1"
            ;;
    esac
fi

if [[ -z "$TARGET" ]]; then
    TARGET="$(rustc -vV | sed -n 's/^host: //p' | tr -d '\r')"
fi

if [[ -z "$TARGET" ]]; then
    echo "cannot determine the Rust target triple; pass --target <triple>" >&2
    exit 1
fi

EXTENSION=""
if [[ "$TARGET" == *windows* ]]; then
    EXTENSION=".exe"
fi

TARGET_DIR="${CARGO_TARGET_DIR:-target}"
SIDECAR_DIR="apps/desktop/src-tauri/binaries"
SIDECAR="${SIDECAR_DIR}/octra-daemon-${TARGET}${EXTENSION}"

echo "Building octra-daemon for ${TARGET}"
cargo build -p octra-daemon --release --target "$TARGET"

mkdir -p "$SIDECAR_DIR"
cp "${TARGET_DIR}/${TARGET}/release/octra-daemon${EXTENSION}" "$SIDECAR"
chmod +x "$SIDECAR"

echo "Staged sidecar at ${SIDECAR}"
