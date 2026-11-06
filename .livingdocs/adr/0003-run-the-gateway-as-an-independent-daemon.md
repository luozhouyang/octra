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

Octra ships a Tauri desktop app, a CLI, and a daemon, and the gateway must keep serving while the GUI reloads, crashes, or is closed. Embedding the gateway in the Tauri process would tie network service lifetime to the WebView and make headless testing and CLI reuse difficult. The daemon also needs to run on operating systems where Tauri is not present. The desktop currently connects to an operator-started daemon; automatic sidecar supervision is planned but not implemented.

## Decision

We will run the gateway as a standalone daemon process with separate control and data listeners. The Tauri host is a control-plane client: it reads the configured endpoint and token, forwards requests through one allow-listed command, and never hosts gateway logic itself. The CLI uses the same control plane. Starting, stopping, and health-supervising the daemon from the desktop remains a future packaging concern rather than a prerequisite for the runtime boundary.

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
- The desktop currently requires the daemon to be started separately (`pnpm daemon`); a production package must add sidecar lifecycle management.
- Packaging must eventually ship and reconcile two processes and their versions.
- Cross-process communication adds an authenticated HTTP boundary inside the local machine.

**Follow-ups:**
- Add desktop-side daemon start/stop/health supervision and an explicit version handshake during packaging work.

## Verification

**Implementation impact:** `crates/octra-daemon/src/main.rs`, `apps/desktop/src-tauri/src/lib.rs`, `apps/desktop/src/lib/control-api.ts`.

**Verification criteria:**
- The daemon starts standalone and serves both listeners without the desktop app.
- The Tauri host exposes only the allow-listed `daemon_request` bridge; no gateway runtime code lives in `apps/desktop/src-tauri`.
- Daemon integration tests exercise the control and data planes without a WebView.

# References

[1] Octra repository. Available at: https://github.com/luozhouyang/octra. Accessed on: 2026-11-06.
