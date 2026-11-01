# Octra

English | [简体中文](./README.zh-CN.md)

**Local-first Unified Gateway** — manage API, LLM, and MCP services in one place, with a cross-platform desktop app.

This repository is the runnable foundation of Octra: a standalone Daemon (Axum control plane + SQLite), an admin CLI, a Tauri 2 desktop app, and dual Rust/TypeScript workspaces. LLM protocol translation, the Pingora data plane, the MCP Gateway, and more are being implemented phase by phase according to the architecture document.

## Repository layout

```text
octra/
├── apps/
│   └── desktop/            # Tauri 2 + React + Vite desktop app
│       └── src-tauri/      # Tauri Rust host (where sidecar management lives)
├── crates/
│   ├── octra-core/         # Domain types and errors
│   ├── octra-config/       # Configuration and local directory resolution
│   ├── octra-storage/      # SQLite + SQLx migrations
│   ├── octra-observability/# tracing initialization
│   ├── octra-gateway-runtime/ # Runtime snapshot + atomic reload
│   ├── octra-gateway-api/  # Axum control plane (Bearer auth)
│   ├── octra-daemon/       # Standalone gateway daemon
│   └── octra-cli/          # Command-line administration
├── packages/
│   ├── types/              # Shared TypeScript types
│   ├── api-client/         # Control-plane API client
│   ├── ui/                 # Shared UI components
│   ├── tsconfig/           # Shared tsconfig
│   └── design-tokens/      # Design tokens
├── migrations/             # SQLx migrations
├── configs/default.toml    # Default daemon configuration
├── scripts/
└── tests/                  # Integration / protocol compatibility / cross-platform tests
```

## Quick start

Prerequisites: Rust 1.97+, Node 24+, pnpm 10+.

```bash
pnpm install

# Terminal 1: start the daemon (control plane at http://127.0.0.1:8081)
pnpm daemon

# Terminal 2: check status
pnpm cli status
pnpm cli services

# Desktop app (requires Tauri dependencies, see below)
pnpm dev

# Browser-only frontend development
pnpm dev:web
```

On first start, the daemon creates the following under `$OCTRA_HOME` (defaults to `~/.octra`):

| File | Description |
|---|---|
| `config.toml` | Daemon configuration |
| `octra.db` | SQLite database (WAL) |
| `control.token` | Control-plane Bearer token; mode `0600` on Unix |

The control plane binds to the loopback address by default, and every endpoint enforces Bearer authentication. The CLI and desktop app read credentials from `control.token`; `OCTRA_CONTROL_TOKEN` overrides it.

Environment variables: `OCTRA_HOME`, `OCTRA_CONTROL_BIND`, `OCTRA_LOG_LEVEL`, `OCTRA_LOG_FORMAT` (`pretty` / `json`), `OCTRA_CONTROL_URL` (CLI).

## Implemented today

- Standalone daemon lifecycle with graceful shutdown.
- Axum control plane: `/api/v1/system/health`, `/api/v1/system/config`, `/api/v1/services`, `/api/v1/routes`, `/api/v1/system/reload`.
- Constant-time Bearer token authentication; the token is randomly generated and persisted to disk.
- SQLite + SQLx migrations with WAL / foreign_keys / busy_timeout.
- Runtime snapshot + atomic swap; the proxy hot path never queries the database directly (the landing point described in section 13.2 of the architecture document).
- CLI: `status`, `services`, `routes`, `reload`, `doctor`.
- Tauri 2 + React desktop skeleton; the dashboard reaches the daemon through Tauri commands.
- Dual workspace and CI (Rust fmt/clippy/test; frontend typecheck/test/build).

## Not yet implemented (tracked on the roadmap)

- Pingora data plane (actual API Gateway forwarding).
- LLM canonical IR, protocol adapters, provider routing.
- Agent discovery / config adapter / bridge.
- MCP Gateway and tool ACL.
- Plugin runtime (native / WASM).
- OS Keychain integration for secrets.
- Cross-platform packaging, signing, and notarization.

## Security boundaries

- SQLite stores only secret references, never plaintext.
- The control plane enforces authentication and binds to loopback by default.
- The daemon is not embedded in the Tauri main process, so it can run and be tested standalone.
- This is currently a development skeleton; secrets, permissions, and packaging must be hardened before production use.

## Verification

```bash
./scripts/check.sh
```

Or run each step individually:

```bash
cargo fmt --all --check
cargo clippy --workspace --exclude octra-desktop --all-targets -- -D warnings
cargo test --workspace --exclude octra-desktop
pnpm typecheck && pnpm test && pnpm build
```

The Tauri desktop app requires system WebView dependencies (Xcode Command Line Tools on macOS, WebKitGTK on Linux), so the CI Rust job does not compile `octra-desktop` for now.

## License

Released under the [MIT License](./LICENSE).
