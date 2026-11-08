#!/usr/bin/env bash
set -euo pipefail

# Builds the Octra daemon and stages it under the name Tauri expects for the
# `externalBin` bundle entry. Binaries are build artifacts and are git-ignored.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

usage() {
    cat <<'EOF'
Usage: bash scripts/build-desktop-sidecar.sh [--target <triple>]

Builds octra-daemon and stages it under apps/desktop/src-tauri/binaries/.

Pass --target universal-apple-darwin to build aarch64-apple-darwin and
x86_64-apple-darwin, stage both binaries, and combine them with lipo. This
requires macOS with the Xcode command-line tools.
EOF
}

TARGET=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --target=*)
            TARGET="${1#--target=}"
            shift
            ;;
        --target)
            if [[ $# -lt 2 || -z "${2:-}" ]]; then
                echo "--target requires a target triple" >&2
                exit 1
            fi
            TARGET="$2"
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        -*)
            echo "unknown option: $1" >&2
            usage >&2
            exit 1
            ;;
        *)
            TARGET="$1"
            shift
            ;;
    esac
done

if [[ -z "$TARGET" ]]; then
    TARGET="$(rustc -vV | sed -n 's/^host: //p' | tr -d '\r')"
fi

if [[ -z "$TARGET" ]]; then
    echo "cannot determine the Rust target triple; pass --target <triple>" >&2
    exit 1
fi

TARGET_DIR="${CARGO_TARGET_DIR:-target}"
SIDECAR_DIR="apps/desktop/src-tauri/binaries"

if [[ "$TARGET" == "universal-apple-darwin" ]]; then
    if ! command -v lipo >/dev/null 2>&1; then
        echo "lipo is required for a universal macOS build; run this on macOS with the Xcode command-line tools installed" >&2
        exit 1
    fi

    ARCHES=("aarch64-apple-darwin" "x86_64-apple-darwin")
    mkdir -p "$SIDECAR_DIR"

    for ARCH in "${ARCHES[@]}"; do
        echo "Building octra-daemon for ${ARCH}"
        cargo build -p octra-daemon --release --target "$ARCH"

        BUILT_BINARY="${TARGET_DIR}/${ARCH}/release/octra-daemon"
        if [[ ! -f "$BUILT_BINARY" ]]; then
            echo "expected ${BUILT_BINARY} to exist after the build" >&2
            exit 1
        fi

        cp "$BUILT_BINARY" "${SIDECAR_DIR}/octra-daemon-${ARCH}"
        chmod +x "${SIDECAR_DIR}/octra-daemon-${ARCH}"
    done

    lipo -create \
        "${SIDECAR_DIR}/octra-daemon-aarch64-apple-darwin" \
        "${SIDECAR_DIR}/octra-daemon-x86_64-apple-darwin" \
        -output "${SIDECAR_DIR}/octra-daemon-universal-apple-darwin"
    chmod +x "${SIDECAR_DIR}/octra-daemon-universal-apple-darwin"

    echo "Staged universal sidecar at ${SIDECAR_DIR}/octra-daemon-universal-apple-darwin"
    echo "Staged per-architecture sidecars at ${SIDECAR_DIR}/octra-daemon-aarch64-apple-darwin and ${SIDECAR_DIR}/octra-daemon-x86_64-apple-darwin"
    exit 0
fi

EXTENSION=""
if [[ "$TARGET" == *windows* ]]; then
    EXTENSION=".exe"
fi

SIDECAR="${SIDECAR_DIR}/octra-daemon-${TARGET}${EXTENSION}"
BUILT_BINARY="${TARGET_DIR}/${TARGET}/release/octra-daemon${EXTENSION}"

echo "Building octra-daemon for ${TARGET}"
cargo build -p octra-daemon --release --target "$TARGET"

if [[ ! -f "$BUILT_BINARY" ]]; then
    echo "expected ${BUILT_BINARY} to exist after the build" >&2
    exit 1
fi

mkdir -p "$SIDECAR_DIR"
cp "$BUILT_BINARY" "$SIDECAR"
chmod +x "$SIDECAR"

echo "Staged sidecar at ${SIDECAR}"
