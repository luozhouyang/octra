#!/usr/bin/env bash
set -euo pipefail

# Compiles the Tauri desktop crate inside Ubuntu 24.04. The host may not have
# the WebKitGTK/GTK development packages (or sudo to install them), so Docker
# provides a reproducible environment instead.
#
# The repository is copied into the container before the build, so this script
# never writes build output, Cargo.lock changes, or root-owned files into the
# working tree. Cargo and rustup state lives in a named volume so repeated runs
# only rebuild what changed.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

IMAGE="${OCTRA_DESKTOP_LINUX_IMAGE:-ubuntu:24.04}"
CARGO_VOLUME="${OCTRA_DESKTOP_LINUX_CARGO_VOLUME:-octra-desktop-linux-cargo}"
RUST_VERSION="${OCTRA_DESKTOP_LINUX_RUST_VERSION:-1.97.1}"
APT_MIRROR="${OCTRA_DESKTOP_LINUX_APT_MIRROR:-}"

case "$(uname -m)" in
    x86_64 | amd64)
        TARGET_TRIPLE="x86_64-unknown-linux-gnu"
        ;;
    aarch64 | arm64)
        TARGET_TRIPLE="aarch64-unknown-linux-gnu"
        ;;
    *)
        echo "unsupported host architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

if ! command -v docker >/dev/null 2>&1; then
    echo "docker is required to run the Linux desktop compile check" >&2
    exit 1
fi

if ! docker info >/dev/null 2>&1; then
    echo "cannot connect to the Docker daemon; start Docker and try again" >&2
    exit 1
fi

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "Pulling ${IMAGE}"
    if ! docker pull "$IMAGE"; then
        echo "failed to pull ${IMAGE}" >&2
        echo "set OCTRA_DESKTOP_LINUX_IMAGE to an accessible Ubuntu 24.04 image and retry" >&2
        exit 1
    fi
fi

echo "Checking octra-desktop in ${IMAGE} as ${TARGET_TRIPLE}"

docker run --rm -i \
    -v "${ROOT}:/work:ro" \
    -v "${CARGO_VOLUME}:/cargo" \
    -e CARGO_HOME=/cargo \
    -e RUSTUP_HOME=/cargo/rustup \
    -e CARGO_TARGET_DIR=/cargo/target \
    -e CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}" \
    -e RUST_VERSION="$RUST_VERSION" \
    -e TARGET_TRIPLE="$TARGET_TRIPLE" \
    -e APT_MIRROR="$APT_MIRROR" \
    -w /build \
    "$IMAGE" \
    bash -s <<'CONTAINER_SCRIPT'
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive

if [[ -n "${APT_MIRROR:-}" ]]; then
    echo "Using Ubuntu package mirror ${APT_MIRROR}"
    sed -i -E "s#http://(archive|security)\.ubuntu\.com/ubuntu/?#${APT_MIRROR%/}#g" \
        /etc/apt/sources.list.d/ubuntu.sources
fi

apt-get update
apt-get install -y --no-install-recommends \
    libwebkit2gtk-4.1-dev \
    libappindicator3-dev \
    librsvg2-dev \
    patchelf \
    libgtk-3-dev \
    libdbus-1-dev \
    pkg-config \
    curl \
    ca-certificates \
    build-essential

if [[ ! -x "$CARGO_HOME/bin/rustup" ]]; then
    echo "Installing Rust ${RUST_VERSION} with rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile minimal --default-toolchain "$RUST_VERSION"
fi

export PATH="$CARGO_HOME/bin:$PATH"

rustup set profile minimal
rustup toolchain install "$RUST_VERSION" --profile minimal --component clippy --component rustfmt
rustup default "$RUST_VERSION"
rustup target add "$TARGET_TRIPLE"

rustc --version
cargo --version

# Build from a copy so the read-only host mount stays untouched and parallel
# work in the repository cannot be surprised by container-owned files.
rm -rf /build
mkdir -p /build
tar -C /work \
    --exclude=.git \
    --exclude=node_modules \
    --exclude=target \
    -cf - . | tar -C /build -xf -
cd /build

# tauri-build validates bundle.externalBin during cargo check. A real sidecar
# is not needed to compile the desktop crate, so stage a placeholder only when
# the caller did not build one already.
SIDECAR="apps/desktop/src-tauri/binaries/octra-daemon-${TARGET_TRIPLE}"
if [[ ! -f "$SIDECAR" ]]; then
    echo "Staging placeholder sidecar at ${SIDECAR} (compile check only)"
    mkdir -p "$(dirname "$SIDECAR")"
    printf '#!/bin/sh\nexit 0\n' >"$SIDECAR"
    chmod +x "$SIDECAR"
fi

echo "Running cargo check -p octra-desktop"
cargo check -p octra-desktop

echo "Running cargo clippy -p octra-desktop -- -D warnings"
cargo clippy -p octra-desktop -- -D warnings

echo "octra-desktop compile check passed"
CONTAINER_SCRIPT
