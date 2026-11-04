# Octra

English | [简体中文](./README.zh-CN.md)

**Local-first Unified Gateway** — manage API, LLM, and MCP services in one place, with a cross-platform desktop app.

This repository is the runnable foundation of Octra: a standalone Daemon with a unified data plane (API, LLM, and MCP gateways), an authenticated control plane, SQLite persistence, an admin CLI, a Tauri 2 desktop app, and dual Rust/TypeScript workspaces. The core gateway functionality is implemented; agent integration, WASM plugins, and production hardening are still on the roadmap.

## Repository layout

```text
octra/
├── apps/
│   └── desktop/            # Tauri 2 + React + Vite desktop app
│       └── src-tauri/      # Tauri Rust host (where sidecar management lives)
├── crates/
│   ├── octra-core/         # Domain types and errors
│   ├── octra-config/       # Configuration and local directory resolution
│   ├── octra-storage/      # SQLite + SQLx migrations and repositories
│   ├── octra-observability/# tracing initialization
│   ├── octra-data-plane/   # Shared HTTP, header, and SSE helpers
│   ├── octra-api-core/     # API Gateway domain types
│   ├── octra-api-gateway/  # API proxy + service/route/upstream CRUD
│   ├── octra-runtime/      # API runtime snapshot, matcher, balancer
│   ├── octra-types/        # Shared control plane (health, auth, CORS)
│   ├── octra-llm-core/     # Canonical LLM IR
│   ├── octra-llm-protocol/ # OpenAI/Anthropic protocol adapters
│   ├── octra-llm-gateway/  # LLM routing, providers, data plane, control plane
│   ├── octra-mcp-core/     # MCP JSON-RPC types
│   ├── octra-mcp-gateway/  # MCP stdio/HTTP clients, /mcp endpoint, control plane
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

# Terminal 1: start the daemon
# control plane: http://127.0.0.1:8081, data plane: http://127.0.0.1:8080
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

The control plane binds to the loopback address by default, and every endpoint enforces Bearer authentication. The data plane binds to loopback by default as well; configure `gateway.listen_bind` if you need a different interface. The CLI and desktop app read credentials from `control.token`; `OCTRA_CONTROL_TOKEN` overrides it.

Environment variables: `OCTRA_HOME`, `OCTRA_CONTROL_BIND`, `OCTRA_GATEWAY_BIND`, `OCTRA_LOG_LEVEL`, `OCTRA_LOG_FORMAT` (`pretty` / `json`), `OCTRA_CONTROL_TOKEN`, `OCTRA_CONTROL_URL` (CLI).

## Connect coding agents

Claude Code, Codex CLI, and OpenCode can use Octra as their LLM endpoint. Create
an LLM route whose `virtual_model_id` matches the model name the agent sends,
then create a virtual key for the agent:

```bash
pnpm cli keys create --name claude-code --consumer <consumer-id> \
  --permissions '{"allowed_models":["octra-default"]}'
```

Put the returned `octra-...` key in the agent's settings. The control token
(`control.token`, port 8081) is an admin credential and must never be given to
an agent. See [examples/agents/README.md](./examples/agents/README.md) for the
control-plane setup, Claude Code / Codex / OpenCode templates, rate-limit
headers, and key rotation.

## Implemented today

- Standalone daemon with two listeners and unified graceful shutdown.
- Authenticated control plane: system health/config/reload plus CRUD for services, routes, upstreams, targets, consumers, credentials, policies, plugins/configs, LLM providers/models/routing groups/routes/virtual keys, and MCP servers/tools/ACLs/keys; request and audit logs are queryable.
- Constant-time Bearer token authentication; the token is randomly generated and persisted to disk.
- SQLite + SQLx migrations with WAL / foreign_keys / busy_timeout; runtime snapshots keep the hot path off the database.
- API Gateway: host/path/method/header routing, longest-prefix matching, round-robin and weighted load balancing with multi-upstream fallback, path stripping, body limits, connect/request timeouts, retries, API-key authentication, policy-based per-consumer route, model, and provider authorization with default deny, streaming proxy, persisted access logs, and JSON and Prometheus metrics.
- Plugin system: Kong-style phase chain with consumer > route > service > global scope precedence and priority ordering, fail-open/fail-closed policies, and built-in `key-auth`, `basic-auth`, `jwt`, `acl`, `correlation-id`, `request-transformer`, `response-transformer`, `cors`, `rate-limiting`, `header-rewrite`, and `circuit-breaker` plugins configured per scope.
- LLM Gateway: canonical IR, OpenAI ↔ Anthropic request/response/SSE conversion, virtual models, routing groups with priority/weight fallback, OpenAI-compatible and Anthropic providers (`env:` / `file:` secret references), virtual keys hashed in SQLite, usage records, `count_tokens`, and consumer policies that restrict allowed models and providers.
- MCP Gateway: Streamable HTTP and stdio upstreams, tool discovery and aggregation, `/mcp` JSON-RPC endpoint with sessions, per-consumer tool ACLs, API keys, and invocation audit rows.
- CLI: `status`, `services`, `routes`, `reload`, `doctor`.
- Tauri 2 + React desktop skeleton; the dashboard reaches the daemon through Tauri commands.
- Dual workspace and CI (Rust fmt/clippy/test; frontend typecheck/test/build).

## Not yet implemented (tracked on the roadmap)

- Pingora-based proxy engine (the current unified data plane uses Axum + reqwest).
- Additional vendor-specific protocol adapters beyond OpenAI, Anthropic,
  OpenAI Responses, and Gemini.
- Agent discovery / config adapter / bridge.
- WASM plugin runtime (the native plugin system and built-ins are implemented).
- OS Keychain integration for secrets.
- Request/response body transformation.
- Separate telemetry database (hourly usage rollups and retention workers are implemented).
- Cross-platform packaging, signing, and notarization.

## Security boundaries

- SQLite stores only secret references and API-key hashes, never plaintext credential values.
- The control plane enforces authentication and binds to loopback by default.
- The data plane binds to loopback by default; API keys are optional per service, and LLM/MCP keys are enforced once configured.
- Provider secrets resolve from `env:` or `file:` references at request time and are never logged.
- The daemon is not embedded in the Tauri main process, so it can run and be tested standalone.
- This is still a development build; secrets, permissions, and packaging must be hardened before production use.

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
