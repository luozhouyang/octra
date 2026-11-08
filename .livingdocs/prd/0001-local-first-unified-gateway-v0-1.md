---
type: PRD
title: Local-first unified gateway v0.1
description: Deliver one local daemon that unifies API, LLM, and MCP gateways with safe agent onboarding, SQLite persistence, an extensible plugin system, and a live desktop console.
status: Draft
timestamp: 2026-11-07T12:40:00Z
---

# 0001. Local-first unified gateway v0.1

## Problem / Motivation

Developers who use multiple AI coding agents, HTTP APIs, and MCP servers configure each tool separately. Provider keys are copied into agent configs, model changes require editing every client, and there is no single local place to see or govern the traffic. Server-oriented gateways assume cloud deployment, external databases, and multi-tenant operations, which is the wrong shape for one developer's machine.

**Who asked:** the project maintainer and the target users — developers and AI-agent users who want one local control plane instead of a collection of per-tool proxies and config files.

## Reference Models

Octra is a local-first composite of three external models. The full adopt/not-adopt matrix and implementation status live in [`architecture/reference-models.md`](../architecture/reference-models.md).

| Area | Reference | Adopted in v0.1 | Deliberately not adopted |
|---|---|---|---|
| API gateway | Kong Gateway | Service/route/upstream/target, route priority, plugin phases and 12-level scopes, round-robin/weighted/consistent hashing/least-connections/latency, active/passive health checks, Kong-shaped Admin API, consumer groups, key/basic/JWT/ACL auth | Nginx/Lua runtime, PDK, workspaces, RBAC, Dev Portal, full Admin API compatibility, sticky-session cookies |
| LLM gateway | yetone/magpie | One local endpoint for OpenAI Chat/Responses, Anthropic, Gemini, Vertex, and Bedrock Converse; canonical IR; virtual models and routing groups; provider accounts with static/OAuth credentials, quota, cooldown, pacing; virtual-key/consumer policies; usage/cost; opt-in PII masking; agent config adapters; all documented routing strategies; Bedrock SigV4 | Signed-in subscription discovery, menu-bar app, remote sharing, Docker/NAS mode, Bun/JS plugin runtime, classifier intent routing |
| MCP gateway | Kong AI MCP Proxy | One `/mcp` endpoint, Streamable HTTP ingress, stdio/HTTP upstreams, sessions, tool aggregation with `<server>__<tool>` names, Kong-shaped server `type`/`sources`, server-default + per-tool ACLs, MCP API keys, invocation audit | OpenAPI conversion modes, per-server routes, Kong auth strategies, CEL ACLs, server-initiated notifications, session sharing |

## Goals

- One local daemon serves API, LLM, and MCP traffic through one authenticated control plane and one SQLite persistence layer.
- Provider, model, routing, and virtual-key governance are centralized; agents receive Octra virtual keys, not provider credentials.
- A developer can point Claude Code, Codex, or OpenCode at Octra from the desktop app and restore the previous configuration safely.
- Request logs, audit logs, LLM usage, MCP invocations, and metrics make local traffic observable.
- A first-party plugin API and a sandboxed third-party WASM path make the gateway extensible without forking the daemon.
- The desktop app and CLI use the same control-plane API, with live data and a token boundary that keeps the control token out of the webview.
- The reference-model boundaries are explicit, so operators can tell which Kong/magpie concepts are supported and which are not.

## Non-goals

- A cloud control plane, remote multi-user administration, a distributed gateway cluster, or Kubernetes deployment.
- PostgreSQL, MySQL, Redis, Elasticsearch, or any required external database, cache, or broker.
- Complete Kong Lua plugin compatibility, Kong PDK compatibility, or a drop-in Kong Admin API replacement.
- A general-purpose agent runtime, magpie's signed-in subscription discovery, or magpie's remote sharing/menu-bar product surface.
- OpenAPI-to-MCP conversion modes in v0.1.
- Unsandboxed third-party native code or Rust dynamic-library plugin loading.
- Signed installers and notarized packages in v0.1; the packaging workflow produces unsigned bundles and documents signing placeholders.

## Requirements

### R1 Local runtime

`pnpm daemon` starts one daemon process with a control listener (`8081` by default) and a data listener (`8080` by default). Both bind loopback by default. Every control endpoint requires the Bearer token. The daemon creates `$OCTRA_HOME`, applies every migration, and exposes health, config, overview, and reload under `/api/v1/system/*`.

### R2 API gateway

The API gateway is the catch-all data-plane handler. It supports:

- Kong-style services, routes, upstreams, and targets.
- Route matching on protocol, host, path (prefix or regex), method, and headers; ordering by `regex_priority`, longest path, then id.
- Round-robin, weighted round-robin, and consistent hashing; passive and active health checks.
- Retries for idempotent methods, bounded by service retries and the attempt budget.
- API-key, basic-auth, JWT, and ACL plugins; consumer groups; trusted-proxy client IP resolution.
- Kong-style plugin phases (request, upstream request, response, response body, error, log; certificate and rewrite are reserved) and the 12-level scope precedence matrix.
- Request/response transformation, CORS, rate limiting, circuit breaking, IP restriction, request-size limiting, and request termination.
- Access logs, JSON metrics, Prometheus metrics, and an `api.request.completed` event.

### R3 LLM gateway

The LLM gateway accepts OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, and Gemini requests; converts them through the canonical IR; and returns the response in the ingress protocol. It supports:

- Virtual models and routing groups with priority rounds, weighted targets, and fallback.
- Strategies `priority`, `order`, `rotate`/`round_robin`, `smart`, `usage`, `quota`, `pace`, and `intent`.
- Provider accounts with static `env:`/`file:` secrets or OAuth tokens, quota, cooldown, pacing, and EWMA health.
- Consumer policies (`allowed_models`, `allowed_providers`) and virtual-key permissions (`allowed_models`, `allowed_providers`, `allowed_accounts`, hourly/daily/weekly/monthly token or cost limits).
- Usage, cost, hourly rollups, and retention.
- Opt-in PII redaction with reversible/irreversible modes and local or loopback engines.
- Agent configuration adapters for Claude Code, Codex, and OpenCode.

### R4 MCP gateway

The MCP gateway fronts many upstream MCP servers behind `/mcp`. It supports:

- Streamable HTTP ingress (`POST`, `GET` SSE keepalive, `DELETE`).
- stdio and Streamable HTTP upstreams.
- `initialize`, `ping`, `tools/list`, and `tools/call`; session binding through `Mcp-Session-Id`.
- Tool discovery and aggregation with a `<server>__<tool>` namespace.
- MCP API keys and consumer/group/tool-default tool ACLs.
- Invocation audit rows for calls, denials, and unknown tools.
- A refresh action that re-runs discovery.

### R5 Agent integration

Claude Code, Codex, and OpenCode adapters detect installations, render a preview, apply changes atomically, restore the original, and run under a sandbox home without touching real configs. Apply is backed up first, uses a SHA-256 conflict check, resolves symlinks, and keeps the newest 20 backups per agent.

### R6 Plugins

Built-in plugins use the API and LLM gateway traits. Third-party plugins use the WASM component ABI with a manifest, approved capabilities, signature policy, and a host-owned per-plugin SQLite database. Durable events and scheduled jobs are delivered by the daemon. API plugin contract version is 2; LLM plugin contract version is 5.

### R7 Persistence

SQLite is the only persistence engine, with SQLx migrations, WAL, foreign keys, a busy timeout, usage rollups, and a retention worker. No external service is required. Plugin-owned data lives in separate SQLite files.

### R8 Console and CLI

Every desktop page reads the live control plane. The desktop has eight pages: Overview, API Gateway, LLM Gateway, MCP Gateway, AI Control, Agents, Plugins, and Settings. The Tauri host holds the control token, exposes one path- and method-allow-listed bridge command plus daemon lifecycle commands, and can start/stop/restart a bundled daemon sidecar.

The CLI exposes `status`, API Gateway services/routes/upstreams/targets CRUD, MCP servers/tools/keys/ACLs CRUD, consumer-policy CRUD, `reload`, `doctor`, `keys`, `usage`, and `ai` control-plane commands over the same API.

### R9 Security boundary

Static provider secrets resolve from `env:` or `file:` references at request time; virtual keys, API keys, and MCP keys are hashed; the control token is admin-only; the database and WAL/SHM sidecars are `0600` on Unix; the control API never returns secret values or backup content. OAuth tokens, KeyAuth keys, BasicAuth passwords, and JWT secrets are sealed with AES-256-GCM under an OS-keychain-backed master key, with `OCTRA_MASTER_KEY` and a `0600` key file as fallbacks.

### R10 Observability and retention

Request logs, audit logs, LLM usage/rollups, MCP invocations, plugin events/jobs, JSON/Prometheus metrics, and plugin stage metrics are recorded. The retention worker bounds every append-only table. Pending events are never pruned.

## Current implementation status

This table is the authoritative progress summary for v0.1. "Implemented" means the code path and at least one test or direct inspection exist; it does not mean the acceptance criteria are all met.

| Requirement | Status | Evidence / remaining gap |
|---|---|---|
| R1 Local runtime | Implemented | `crates/octra-daemon/src/main.rs`, health/config/overview/reload routes |
| R2 API gateway | Implemented core | Matcher, balancer (round-robin/weighted/consistent hashing/least-connections/latency), health checks, plugins, Admin API, CLI and desktop typed-form CRUD |
| R3 LLM gateway | Implemented core | Six wire protocols (OpenAI Chat/Responses, Anthropic, Gemini, Vertex, Bedrock Converse with API-key or SigV4 auth), IR, routing with all magpie strategies, accounts, OAuth with loopback callbacks, virtual-key/consumer policies, usage/cost, PII; the AI-native control plane is the only model source with CRUD/validate/preview/apply/rollback, built-in policy execution for every policy type including PromptGuard, PromptCompress, and SemanticCache, semantic target routing with optional embeddings, provider query parameters and protocol presets, streaming cache population/replay, full CRUD CLI with stdin/file bodies, and typed desktop forms |
| R4 MCP gateway | Implemented core | `/mcp`, stdio/HTTP, sessions, namespace, keys, Kong-shaped server type/sources, server-default + per-tool ACL scopes, audit; no CEL ACL, conversion modes, per-server routes, or unanchored sessions |
| R5 Agent integration | Implemented | Three adapters, atomic apply/restore, backups; sandbox end-to-end test covers Claude Code only, not Codex/OpenCode |
| R6 Plugins | Implemented | Native/WASM, capabilities, storage, events, jobs; delivery/retry/no-subscriber/job tests cover the bus; no delivery-lag operational view |
| R7 Persistence | Implemented | 29 migrations, WAL, retention, `0600`; write-contention benchmark not done |
| R8 Console/CLI | Partial | Eight live pages: AI Control, API Gateway, MCP, and consumer policies have typed forms for their core entities, the MCP ACL form has a consumer-group picker, and Settings can start/stop/restart a bundled daemon; CLI covers status, API services/routes/upstreams/targets CRUD, MCP servers/tools/keys/ACLs CRUD, consumer-policy CRUD, keys/usage, full AI entity CRUD, and AI activation actions |
| R9 Security | Implemented with accepted exceptions | Secret references, hashed keys, control token boundary, `0600`, masked control responses, OS-keychain-backed AES-256-GCM encryption for stored secrets; browser mode stores a token in settings; no key rotation yet |
| R10 Observability/retention | Implemented core | Logs, usage, metrics, events, retention; API metrics are in-memory; no OTLP; no delivery-lag metric |

### Verification snapshot

- `cargo fmt --all --check` passes.
- `cargo clippy --workspace --exclude octra-desktop --all-targets -- -D warnings` passes.
- `cargo test --workspace --exclude octra-desktop` passes: 1017 passed, 0 failed, 17 ignored.
- WASM integration tests are ignored by default and require `cargo-component`; `scripts/test-wasm.sh` runs them in CI.
- `pnpm typecheck`, `pnpm test`, and `pnpm build` pass (5 frontend tests across the shared packages).

### Planned evolution (not part of v0.1 acceptance)

ADR 0018 replaced the legacy route/group LLM control plane with AI-native entities: AI Provider, AI Provider Account, AI Model, AI Model Target, AI Policy, AI Policy Attachment, config revisions, and a local compiler. The migration is complete without backward compatibility: the `ai_*` tables are the only model/routing/account/policy source, the legacy model tables and `/admin/llm/*` model endpoints are removed, and `legacy`/`shadow`/`on` modes plus import-legacy are gone. The AI-native control plane supports CRUD, validate/preview/apply/revisions/rollback, per-target context/capability/pricing metadata, built-in policy execution for every policy type, semantic target routing with optional embedding vectors and lexical fallback, provider query parameters and protocol presets, Vertex and Bedrock Converse adapters with optional SigV4, streaming cache population/replay, OAuth desktop controls with loopback callbacks, full CRUD CLI, and typed desktop forms for AI, API Gateway, MCP, and consumer-policy entities. Non-model state was renamed to `virtual_keys`, `ai_usage`, `ai_usage_rollup`, `budget_windows`, `shared_counters`, `plugin_state`, and `pii_engines`. OAuth tokens, KeyAuth keys, BasicAuth passwords, and JWT secrets are encrypted under an OS-keychain-backed master key; MCP servers expose Kong-shaped `type`/`sources` and two-tier ACLs; the desktop can supervise a bundled daemon sidecar. Signed installers and key rotation remain future work. See [LLM Control Plane](../architecture/llm-control-plane.md).

## Acceptance criteria

- A fresh `pnpm daemon` creates `$OCTRA_HOME`, applies every migration, and `pnpm cli status` succeeds.
- A request through each gateway reaches a mock upstream, including an SSE response for LLM streaming.
- Applying and restoring an agent config in sandbox mode round-trips byte-identically for Claude Code, Codex, and OpenCode.
- The reference `consumer-quota` WASM plugin installs, serves a request, and retains plugin-owned data across an upgrade.
- A published event with a matching subscriber is delivered and retried, and a scheduled job runs; terminal events are pruned.
- `cargo fmt --all --check`, `cargo clippy --workspace --exclude octra-desktop --all-targets -- -D warnings`, `cargo test --workspace --exclude octra-desktop`, `pnpm typecheck`, `pnpm test`, and `pnpm build` pass.
- Control endpoints reject a missing or invalid Bearer token; the storage tests assert `0600` on the database and sidecars; the control API masks secrets.
- Every desktop page renders live control-plane data; the Tauri bridge rejects paths and methods outside its allow-list.
- The Kong/magpie/Kong reference boundaries in [`reference-models.md`](../architecture/reference-models.md) have a conformance test or an explicit "not implemented" assertion.
- `living-docs check .livingdocs` passes in CI and in `scripts/check.sh`; the CLI is pinned and the script does not silently skip when it is missing.

Open acceptance gaps, in priority order:

1. **Agent sandbox matrix.** Claude Code has an end-to-end sandbox apply/restore test; Codex and OpenCode need the same byte-identical round-trip test.
2. **Event bus integration tests.** The adapter and storage have coverage, but delivery, retry/backoff, scheduled job execution, and the no-subscriber skip path are not tested end to end.
3. **UI/CLI management.** The console is read-only for the three gateways; the CLI does not provide full CRUD. Either narrow the PRD claim or add the management flows.
4. **Reference conformance.** The reference matrix is prose; no test asserts that unimplemented Kong/magpie features are rejected or documented.
5. **Performance budget.** The p95 proxy overhead budget and benchmark remain open.
6. **Living-docs gate.** `scripts/check.sh` skips when the CLI is missing and CI has no docs job; add a pinned CLI and a failing default to make Q5 real.

## Success metrics

- A developer can configure one supported coding agent to use Octra in ten minutes or less, measured by a timed walkthrough using only the README and desktop console.
- Apply/restore completes 100 consecutive times in the sandbox matrix without a manual config repair or a corrupted file.
- All supported protocol conversions and all three agent adapters have passing fixtures or sandbox tests; the suite reports zero failures.
- A clean-machine startup test needs no external database, cache, or broker process.
- The reference third-party plugin installs and serves traffic in CI.
- Every documented reference-model boundary is either implemented and tested or explicitly rejected at configuration time.

## Open questions

- What p95 proxy-overhead budget should R2 hold, and which benchmark enforces it? Resolve with an ADR before publishing a performance claim.
- Should the desktop console grow more operational views, or remain a live console with targeted typed-form editors plus the Admin API as the management surface?
- Which embedding provider preset should be recommended for local-first installs, and should semantic cache reuse the global `llm.semantic_embedding` config instead of its per-policy embedding block?
- Which OAuth providers should get first-class presets instead of raw `config_json.oauth` documents?
- Which Kong MCP capability should be added next: CEL ACLs, OpenAPI conversion modes, per-server routes, or notifications?
- Should the v0.1 PRD be accepted once the acceptance criteria are demonstrated, or amended per capability?

## Related

- Constitution: [/constitution.md](/constitution.md)
- ADRs: [/adr/index.md](/adr/index.md)
- Architecture: [/architecture/index.md](/architecture/index.md)
- Reference models: [/architecture/reference-models.md](/architecture/reference-models.md)
- LLM control-plane design: [/adr/0018-model-the-llm-control-plane-on-ai-native-entities.md](/adr/0018-model-the-llm-control-plane-on-ai-native-entities.md), [/architecture/llm-control-plane.md](/architecture/llm-control-plane.md)
- Issues: none yet.
