# Octra

English | [简体中文](./README.zh-CN.md)

**Local-first Unified Gateway** — manage API, LLM, and MCP services in one place, with a cross-platform desktop app.

This repository is the runnable foundation of Octra: a standalone Daemon with a unified data plane (API, LLM, and MCP gateways), an authenticated control plane, SQLite persistence, an admin CLI, a Tauri 2 desktop app, and dual Rust/TypeScript workspaces. The three gateways, plugin runtimes, agent integration, and desktop console are implemented; production hardening, packaging, and full reference-model parity remain on the roadmap.

## Repository layout

```text
octra/
├── apps/
│   └── desktop/            # Tauri 2 + React + Vite desktop app
│       └── src-tauri/      # Tauri Rust host (control-plane bridge)
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
│   ├── octra-llm-protocol/ # OpenAI/Anthropic/Gemini/Vertex/Bedrock protocol adapters
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

## Engineering decisions

Tracked decisions and architecture views live in [`.livingdocs/`](./.livingdocs/index.md), an OKF bundle maintained with [Living Docs](https://github.com/ejklock/living-docs-skill). Read the in-force view with `living-docs read --docs-dir .livingdocs` and run `living-docs check .livingdocs` after a structural change. Local scratch notes remain in the git-ignored `docs/` directory.

The gateways follow three explicit reference models: the API gateway adopts Kong's service/route/upstream/target model, the LLM gateway adopts yetone/magpie's local routing model (with Kong AI Gateway 2.x as a secondary control-plane reference), and the MCP gateway adopts Kong's AI MCP Proxy fronting model. The adopt/not-adopt matrix and current parity are recorded in [`.livingdocs/architecture/reference-models.md`](./.livingdocs/architecture/reference-models.md).

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

| File            | Description                                     |
| --------------- | ----------------------------------------------- |
| `config.toml`   | Daemon configuration                            |
| `octra.db`      | SQLite database (WAL)                           |
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

### Testing without touching real configs

Start the daemon with a sandbox before opening the Agents page:

```toml
[agents]
sandbox_home = "/tmp/octra-agent-sandbox"
sandbox_path = "/tmp/octra-agent-bin"
```

`sandbox_home` replaces the real user home for detection, reads and writes.
`sandbox_path` is searched for `claude`, `codex` and `opencode`; fake
executables are enough. When it is omitted, the real `PATH` is used. The
control API reports the mode at `GET /api/v1/agents/mode`, the Agents page
shows a sandbox banner, and preview never writes anything.

## Implemented today

- Standalone daemon with two listeners and unified graceful shutdown.
- Authenticated control plane: system health/config/reload plus CRUD for services, routes, upstreams, targets, consumers, consumer groups, credentials, plugins/configs, AI providers/accounts/models/targets/policies/attachments, virtual keys, LLM consumer policies, and MCP servers/tools/ACLs/keys; request and audit logs, events, and jobs are queryable.
- Constant-time Bearer token authentication; the token is randomly generated and persisted to disk.
- SQLite + SQLx migrations with WAL / foreign_keys / busy_timeout; runtime snapshots keep routing and authorization decisions off the database (virtual-key limit checks still read persisted usage).
- API Gateway: host/path/method/header routing, longest-prefix matching, round-robin, weighted, consistent-hashing, least-connections, and latency load balancing with active/passive health checks, path stripping, body limits, connect/request timeouts, retries, key/basic/JWT/ACL authentication with consumers and consumer groups, streaming proxy, persisted access logs, and JSON and Prometheus metrics.
- Plugin system: Kong-style phase chain with the full 12-level consumer/consumer-group/route/service/global scope precedence, priority ordering, fail-open/fail-closed policies, and built-in `key-auth`, `basic-auth`, `jwt`, `acl`, `correlation-id`, `request-transformer`, `response-transformer`, `cors`, `rate-limiting`, `header-rewrite`, `circuit-breaker`, `ip-restriction`, `request-size-limiting`, and `request-termination` plugins configured per scope.
- LLM Gateway: canonical IR, OpenAI Chat/Responses, Anthropic, Gemini, Vertex, and Bedrock Converse request/response/SSE conversion (Bedrock uses its binary event stream upstream), virtual models, routing groups with priority/weight fallback and `order`/`manual`/`weight`/`rotate`/`fast`/`cheap`/`free`/`drain`/`expiring`/`smart`/`usage`/`quota`/`pace`/`intent`/`semantic` strategies, provider accounts with static or OAuth credentials, provider query/header configuration, quota/cooldown/pacing, virtual keys hashed in SQLite, consumer policies that restrict allowed models and providers, usage/cost rollups, `count_tokens`, and opt-in PII redaction.
- MCP Gateway: Streamable HTTP and stdio upstreams, Kong-shaped server `type`/`sources`/`route_path`, global and per-server `/mcp` JSON-RPC endpoints with sessions, server-default and per-tool ACLs with consumer/group/default scopes and CEL `allow_when`/`deny_when`, OIDC JWKS/discovery/introspection auth strategies plus MCP API keys, JSON/YAML OpenAPI conversion modes, tool discovery and aggregation with a `<server>__<tool>` namespace, and invocation audit rows.
- Plugins: native and WASM (API and LLM) plugin runtimes, plugin-owned SQLite storage, config schemas, per-instance metrics and latency percentiles, durable plugin events, scheduled jobs, and shared rate-limit/budget counters.
- AI-native LLM control plane (Phase 43, AI-native only): `ai_*` schema, `octra-ai-config` compiler, `AiStore`, `/api/v1/ai/*` CRUD, validate/preview/apply/revisions/rollback, revision snapshots, transactional replace, per-target model metadata (context, capabilities, pricing), built-in policy execution for every policy type (rate limit, budget guard, PII, model map, param override, system prompt, request audit, think tags, PromptGuard, PromptCompress, SemanticCache), `semantic` target routing over AI-native descriptions with optional OpenAI-compatible embeddings and lexical fallback, provider `config_json.query` parameters and protocol presets (Ollama, vLLM, LM Studio, Groq, OpenRouter, DeepSeek, Together, Fireworks, Claude, Google, Vertex, Bedrock, OpenAI-compatible), Vertex and Bedrock Converse adapters with optional SigV4, streaming cache population and SSE replay, AI-native account OAuth with exposed status, desktop controls, and a loopback callback listener, request-audit `cache_status` and cache/routing logs, full CRUD CLI with inline/stdin/file JSON bodies, and a desktop AI Control page with typed forms for providers, accounts, models, targets, policies, and attachments. Stored OAuth tokens, KeyAuth keys, BasicAuth passwords, and JWT secrets are sealed with AES-256-GCM under an OS-keychain-backed master key and can be rotated and retired. The legacy model tables, `/admin/llm/*` model endpoints, `legacy`/`shadow`/`on` modes, and import-legacy path are removed without backward compatibility.
- Agent integration: detects Claude Code, Codex and OpenCode, previews and applies the Octra provider into their native config files, and restores backups atomically.
- CLI: `status`, `services`/`routes`/`upstreams`/`targets` CRUD, `mcp` servers/tools/keys/ACLs CRUD, `policies` consumer-policy CRUD, `reload`, `doctor`, `keys`, `usage`, and `ai` status/actions plus full CRUD for providers, accounts, models, targets, policies, and attachments.
- Tauri 2 + React desktop app with a shadcn-style component set and Tailwind CSS v4 design tokens for dark and light modes; AI Control, API Gateway, MCP, and consumer policies use typed forms, the Tauri host can start/stop/restart a bundled daemon sidecar, and every console page reads the local control plane.
- Dual workspace and CI (Rust fmt/clippy/test; frontend typecheck/test/build).

## Not yet implemented (tracked on the roadmap)

- Additional vendor-specific protocol adapters beyond OpenAI, Anthropic,
  OpenAI Responses, Gemini, Vertex, and Bedrock Converse. Bedrock supports
  API-key bearer auth and optional SigV4.
- Agent discovery / config adapter / bridge for agents beyond the three
  supported adapters.
- Full reference-model parity where justified: full OpenAPI serialization
  styles/external `$ref`s, OIDC discovery/introspection, and MCP
  server-initiated notifications or session sharing.
- Remaining hardening: signed/notarized release
  execution (wired, needs Apple credentials) and full OpenAPI serialization
  parity. The Phase 43
  control API, rollback, full policy execution including PromptGuard,
  PromptCompress, and SemanticCache, `semantic` target routing with optional
  embeddings, provider query parameters and presets, streaming cache
  population/SSE replay, Vertex/Bedrock adapters with SigV4, AI-native account
  OAuth with desktop controls and loopback callbacks, encrypted and rotatable
  stored secrets,
  cache/routing observability, full CRUD CLI, Kong-shaped MCP modes/ACLs, and
  desktop typed forms for all AI/API/MCP/consumer-policy entities already
  exist. The legacy model tables, endpoints, modes, and import path are
  removed; `ai_*` is the only model control plane. Semantic cache/routing use
  exact keys plus local lexical similarity by default.

## Security boundaries

- SQLite stores secret references and API-key hashes for static gateway credentials. OAuth access/refresh values, KeyAuth keys, BasicAuth passwords, and JWT secrets are sealed at rest with AES-256-GCM under a master key resolved from the OS keychain, `OCTRA_MASTER_KEY`, or a `0600` key file. Provider secrets are never returned by the control API.
- Agent configuration backups keep the exact original file so a restore is lossless. Those files may contain credentials, so the database and its WAL sidecars are created with `0600` permissions and only the newest 20 backups per agent are kept. The control plane never returns backup content.
- Applied agent configurations contain the virtual key by design; the control plane never returns it again.
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
