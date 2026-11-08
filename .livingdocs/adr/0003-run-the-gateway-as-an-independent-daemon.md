---
type: ADR
title: Run the gateway as an independent daemon
description: Run the gateway as a standalone daemon; the desktop and CLI are control-plane clients, not gateway hosts.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-06T15:44:39Z
---

# 0003. Run the gateway as an independent daemon

## Context

Octra ships a Tauri desktop app, a CLI, and a daemon, and the gateway must keep serving while the GUI reloads, crashes, or is closed. Embedding the gateway in the Tauri process would tie network service lifetime to the WebView and make headless testing and CLI reuse difficult. The daemon also needs to run on operating systems where Tauri is not present. The desktop can attach to an operator-started daemon or supervise a bundled sidecar.

## Decision

We will run the gateway as a standalone daemon process with separate control and data listeners. The Tauri host is a control-plane client: it reads the configured endpoint and token, forwards requests through one allow-listed command, and never hosts gateway logic itself. The CLI uses the same control plane. The desktop may also start, stop, restart, and health-check a daemon child discovered from `OCTRA_DAEMON_BIN`, a bundled sidecar, or a development target binary; it never kills a daemon it did not start.

Rejected alternatives:
- **Embedding the gateway in the Tauri main process** — a GUI crash would stop traffic, headless operation would be impossible, and tests would need a WebView.
- **Shipping only an OS service** — solves headless operation but loses the desktop lifecycle and status experience that is part of the product.
- **A remote gateway server** — contradicts the local-first, single-node scope.

## Consequences

**Easier / gained:**
- The daemon can be started, tested, and upgraded independently of the desktop app.
- The CLI and desktop share one control-plane contract.
- A GUI reload does not interrupt proxied traffic.

**Harder / accepted trade-offs:**
- The desktop can supervise a sidecar, but an externally started daemon is never killed by the GUI.
- Packaging ships and reconciles two processes and their versions; macOS bundles need Developer ID signing and notarization.
- Cross-process communication adds an authenticated HTTP boundary inside the local machine.

**Follow-ups:**
- Add an explicit daemon/desktop version handshake during upgrade work.
- Configure the Apple Developer secrets in CI; signing/notarization steps are wired but have not run in this environment.

## Verification

**Implementation impact:** `crates/octra-daemon/src/main.rs`, `crates/octra-desktop-daemon/`, `apps/desktop/src-tauri/src/lib.rs`, `apps/desktop/src/lib/control-api.ts`, `apps/desktop/src/app/pages/settings.tsx`, `scripts/build-desktop-sidecar.sh`, `scripts/check-desktop-linux.sh`, `.github/workflows/desktop.yml`.

**Verification criteria:**
- The daemon starts standalone and serves both listeners without the desktop app.
- The Tauri host exposes only allow-listed bridge/lifecycle commands; no gateway runtime code lives in `apps/desktop/src-tauri`.
- The lifecycle crate discovers a binary, starts/health-checks/stops an owned child, refuses to kill an external daemon, and reports early exits with a log tail.
- The Tauri crate compiles and links on Linux with WebKitGTK/GTK/DBus; `tauri build --debug --no-bundle` succeeds.
- Universal macOS sidecars are staged with `lipo`; the CI workflow signs/notarizes when Apple secrets are present.
- Daemon integration tests exercise the control and data planes without a WebView.

# References

[1] Octra repository. Available at: https://github.com/luozhouyang/octra. Accessed on: 2026-11-06.
