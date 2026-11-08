---
type: ADR
title: Model the LLM control plane on AI-native entities
description: Replace the route/group-oriented LLM tables with AI Provider, AI Model, AI Target, AI Policy, and AI Policy Attachment entities compiled into a local runtime snapshot, while keeping Octra local-first.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-08T01:19:43Z
---

# 0018. Model the LLM control plane on AI-native entities

> **Status: Phase 26 landed; AI-native only. Optional hardening pending.** The `ai_*` schema, compiler, `AiStore`, `/api/v1/ai/*` CRUD, validate/preview/apply/revisions/rollback, revision snapshots, transactional replace, AI-native runtime mapping, built-in policy execution for every policy type (including PromptGuard, PromptCompress, and SemanticCache), semantic target routing with optional embedding vectors and lexical fallback, provider query parameters and protocol presets, Vertex and Bedrock Converse adapters, streaming cache population and SSE replay, AI-native account OAuth with exposed status/expiry, desktop controls and loopback callbacks, cache/routing observability, full CRUD CLI with stdin/file bodies, typed desktop forms for every AI entity, and typed API Gateway/MCP forms exist and pass their tests. The legacy `llm_*` model control plane, `/admin/llm/*` model endpoints, `legacy`/`shadow`/`on` modes, `auto_import_legacy`, and the import-legacy path were removed without backward compatibility. Remaining `llm_*` tables hold virtual keys, usage, counters, plugin state, PII engines, and OAuth tokens only. Bedrock SigV4 auth, optional table renames, and production hardening remain future work.

## Context

Octra's current LLM control plane is route/group-oriented. `llm_providers` and `llm_models` describe upstreams, `llm_routes` map a client-facing `virtual_model_id` to a `llm_routing_groups` row, and `llm_routing_targets` attach provider/model/account/weight/priority. Virtual keys, consumer policies, provider accounts, OAuth tokens, PII engines, and plugin configurations live in separate tables. This model works for the current feature set, but it has four limitations:

- **No first-class policy surface (at the time of this decision).** Consumer policies were enforced by the LLM runtime but had no control-plane CRUD, and PII/rate-limit/budget behavior was scattered between policies, plugin configs, and virtual-key permissions.
- **No first-class target model.** A target is a routing row, not an object that can carry cost, capability, semantic description, format overrides, or its own policy attachments.
- **No nesting.** magpie routing groups can contain groups; the current route/group/target model cannot express that without abusing group/target rows.
- **No clean migration path for Kong AI Gateway 2.x concepts.** Kong 2.x exposes AI Model Provider, AI Model (capabilities, formats, targets, balancer, access), AI Policy, AI Consumer/Group, and AI Vault as first-class AI entities. Octra does not need Konnect, but the entity separation is useful.

magpie's main features — provider/account pools, routing groups, strategies, quota/cooldown, privacy masking, usage/cost, and coding-agent configuration — need a control plane that can express these relationships without turning every feature into a special case.

Octra remains local-first: one daemon, one SQLite database, one control listener, one data listener. Kong's hybrid control plane, mandatory Redis, vector database, and Lua policy runtime are not requirements.

## Decision

We will model the LLM control plane on AI-native entities and compile them into an immutable local runtime snapshot:

### Entities

- **AI Provider** (`ai_providers`): a reusable upstream provider definition with protocol, base URL, auth reference, capabilities, and non-secret configuration.
- **AI Provider Account** (`ai_provider_accounts`): a static, OAuth, or CLI credential belonging to a provider, with weight, priority, quota, cooldown, pacing, and health state.
- **AI Model** (`ai_models`): a client-facing virtual model. `kind` is `model` or `group`. A model carries capabilities (chat, responses, tools, vision, reasoning, embeddings, ...), formats (OpenAI, Anthropic, Gemini, Responses, native), route paths, and a balancer document.
- **AI Model Target** (`ai_model_targets`): a target inside a model. It references a provider and upstream model, or another AI Model for a nested group. It carries weight, priority, position, optional account pinning, cost overrides, request overrides, and an optional semantic description.
- **AI Policy** (`ai_policies`): a typed policy with a validated configuration schema. Policy types include rate limiting, budget guard, PII redaction, prompt guard, prompt compression, semantic cache, model mapping, parameter override, system prompt, request audit, and think tags.
- **AI Policy Attachment** (`ai_policy_attachments`): binds a policy to a scope: global, provider, model, target, consumer group, consumer, or virtual key. Attachments carry an execution order and an enabled flag.
- **AI Consumer / Consumer Group**: reuse the existing `consumers` and `consumer_groups` tables; an AI consumer is the same identity used by the API and MCP gateways.
- **AI Virtual Key**: reuse the existing `llm_virtual_keys` semantics (hashed key, model/provider/account allowlists, token/cost limits) and make it an AI-consumer credential.
- **AI Usage / Rollup**: reuse `llm_usage` and `llm_usage_rollup`, adding target and policy attribution where needed.
- **AI Config Revision** (`ai_config_revisions`): records a validated configuration generation, actor, diff, status, and rollback target.
- **Coding Agent** (`coding_agents`, `agent_profiles`, `agent_config_backups`): the Claude Code / Codex / OpenCode integration. This is deliberately **not** the same entity as Kong's A2A `AI Agent`; an optional `ai_agents` table may be added later for agent-to-agent traffic.
- **AI MCP Server**: reuse the existing MCP server/tool/session/ACL tables from ADR 0014.

### Compilation

A local compiler loads the entities, validates references and graph shape, and produces an immutable runtime snapshot:

- provider catalog and account pools,
- model plans with nested-group expansion,
- target order, weights, priorities, and account candidates,
- policy chains by scope,
- pricing and usage configuration,
- coding-agent profiles.

Compilation rejects cycles, missing references, unsupported capabilities/formats, and nested groups deeper than the configured maximum. A successful compile creates a config revision and swaps the snapshot atomically. A failed compile leaves the previous snapshot serving. The control API can dry-run, preview, apply, and roll back a revision.

### Policy model

Policies are typed, not arbitrary code. The engine distinguishes two behaviors:

- **Additive constraints** (allowlists, token/cost limits, rate limits, PII, prompt guard) all apply; the strictest result wins.
- **Transformations** (system prompt, model map, parameter override, prompt template) execute in a deterministic order; a more specific scope may override a less specific one.

Scope precedence is `virtual_key > consumer > consumer_group > target > model > provider > global`. Security policies run before transformations; response and stream policies run in reverse order. Policies must be stream-aware: placeholders, tool-call arguments, and usage can span SSE chunks.

### Deployment boundary

The control plane remains local: SQLite is the source of truth, the daemon compiles snapshots, and the desktop/CLI use the local control API. No Konnect, Redis, vector database, or external policy runtime is required. Semantic cache/routing, Redis-backed rate limiting, OIDC, and remote data planes are optional adapters behind feature flags, not core dependencies.

**Not adopted:**

- Kong's Konnect-managed hybrid control plane and AI Gateway 2.x control-plane deployment.
- Kong Lua policies, PDK, or a general-purpose policy language.
- Mandatory Redis/vector DB dependencies.
- A single generic "plugin" table without typed schemas.
- Merging coding-agent configuration with A2A AI Agent traffic.

## Rejected alternatives

- **Keep the current `llm_*` schema and add columns** — cannot express nested groups, target-level policy attachments, capabilities/formats, or config revisions cleanly; the special cases would spread through the runtime.
- **Implement the LLM gateway as an API Gateway plugin chain** — the separate-router decision is recorded in ADR 0013; protocol semantics, streaming, and token accounting deserve a dedicated router.
- **Copy Kong AI Gateway 2.x deployment** — a Konnect control plane and mandatory external services contradict Octra's local-first constitution.
- **One flat model per provider** — loses the virtual-model abstraction and magpie-style failover groups.
- **Treat coding agents as AI Agents** — the two have different lifecycles, credentials, and failure modes; merging them would confuse the control plane and the UI.

## Consequences

**Easier / gained:**

- Provider, model, target, policy, consumer, and key become stable surfaces for the desktop, CLI, and API.
- magpie's routing groups, nested groups, strategies, account pools, privacy masking, and usage/cost fit into the entity model without special cases.
- Policies can be attached at the scope where they belong instead of being scattered across plugin configs and key permissions.
- Config revisions and atomic compilation make rollback and audit possible.
- The model leaves room for future A2A `AI Agent` traffic without conflating it with coding-agent configuration.

**Harder / accepted trade-offs:**

- The schema and runtime migration was completed in Phase 11 without backward compatibility: the legacy `llm_*` model tables and `/admin/llm/*` model APIs were removed rather than kept as compatibility surfaces.
- The compiler and policy engine add code and tests; policy precedence must be deterministic and documented.
- Nested groups need cycle checks, depth limits, and a clear expansion order.
- CRUD grows: providers, accounts, models, targets, policies, attachments, consumers, keys, and revisions all need control-plane endpoints and UI.
- The local control plane is intentionally smaller than Kong's; it will not provide enterprise RBAC, workspaces, or a managed control plane.

**Follow-ups:**

- Add Bedrock SigV4 authentication and more vendor-specific wire adapters when the product needs them; `config_json.query`, `headers`, `auth_header`, `wire`, OAuth, CLI providers, Vertex, and Bedrock Converse already cover broad provider configuration.
- Keep semantic cache/routing and Redis-backed rate limiting optional; do not make them required.
- Add a dedicated `ai_agents` (A2A) design only if that traffic becomes a product requirement.
- Optionally rename the remaining `llm_virtual_keys`, usage, counter, and plugin-state tables in a later cleanup; they are not model control plane.

## Verification

**Implementation impact:** the LLM data plane still compiles an internal `LlmRuntimeData` snapshot, but the control plane is now exclusively AI-native. Phase 1 added `crates/octra-ai-config/`, `migrations/0023_ai_control_plane.sql`, and `crates/octra-storage/src/ai.rs`. Phase 2 added `crates/octra-llm-gateway/src/ai_control.rs` and the `/api/v1/ai/*` control API. Phase 3 added `migrations/0024_ai_revision_snapshot.sql`, revision snapshots/rollback, and transactional replace. Phase 4 added CLI commands, the desktop AI Control page, shared TypeScript types/client methods, and the `/ai-control` route. Phase 5 added `crates/octra-llm-plugin-runtime` `reload_with_extra`, `crates/octra-llm-gateway/src/ai_policy_plugins.rs`, full CRUD CLI, and desktop editors. Phase 6 added `crates/octra-llm-plugin-builtin/src/prompt_guard.rs`, CLI stdin/file `--body` input, and typed desktop forms. Phase 7 added `prompt_compress.rs` and `semantic_cache.rs`, `LlmAction::Respond`, `LlmRequestOutcome`, `on_final_response`, and data-plane cache hits. Phase 8 added `crates/octra-llm-gateway/src/similarity.rs`, the `semantic` routing strategy, `LlmRuntimeData::target_semantic_descriptions`, and `config_json.query` provider parameters. Phase 9 added `StreamAccumulator`/`cached_response` SSE replay and typed forms for accounts, targets, and attachments. Phase 10 added per-target context/capability/pricing metadata (`migrations/0025_ai_target_model_metadata.sql`) and removed the `legacy`/`shadow`/`on` modes and `auto_import_legacy`. Phase 11 added `migrations/0026_drop_legacy_llm_models.sql`, removed the legacy model/account/routing storage and `/admin/llm/*` model endpoints, moved OAuth tokens to `ai_provider_oauth_tokens`, deleted `ai_compat`/import-legacy, and rewired the test harness and desktop pages to AI-native entities. The AI-native OAuth endpoints live at `/api/v1/ai/accounts/{id}/oauth/{start,complete,refresh,status}` and `DELETE /api/v1/ai/accounts/{id}/oauth`.

**Phase 26 evidence:** the AI-native schema, compiler, store, control API, validate/preview/apply/revisions/rollback, revision snapshots, transactional replace, runtime mapping, built-in policy execution for every policy type, semantic target routing with optional embedding vectors and lexical fallback, provider query parameters and protocol presets, Vertex and Bedrock Converse adapters, streaming cache population and SSE replay, AI-native account OAuth with exposed status/expiry, desktop controls and loopback callbacks, cache/routing observability, full CRUD CLI with stdin/file bodies, typed desktop forms for every AI entity, and typed API Gateway/MCP forms are implemented. The legacy model tables and endpoints are gone; virtual keys, usage, counters, plugin state, PII engines, and OAuth tokens remain as non-model state. `cargo test --workspace --exclude octra-desktop` reports 946 passed, 0 failed, 17 ignored; `pnpm typecheck`, `pnpm test` (5 tests), and `pnpm build` pass. The Tauri host crate cannot be compiled in this environment because GTK/WebKit/DBus development packages are missing; the callback helper is covered by scratch-crate unit tests.

**Verification criteria (remaining implementation):**

- A provider, account, model, target, policy, and attachment can be created, validated, and compiled into a snapshot.
- A nested group is expanded deterministically; a cycle or depth overflow is rejected with a clear error.
- Additive policies all apply and the strictest limit wins; transformation policies follow the documented scope order.
- A streamed response passes through PII redaction across chunk boundaries and records usage once at stream close; a cached stream is reconstructed and replayed as SSE.
- A failed compile leaves the previous snapshot serving; a successful compile creates a revision and can be rolled back.
- No legacy model table or `/admin/llm/*` model endpoint remains.
- `cargo test -p octra-llm-gateway -p octra-storage` passes, plus compiler/policy/stream tests.

**Phase 26 status:**

- Implemented: `ai_*` schema, compiler, `AiStore` CRUD/replace/revision snapshots, `/api/v1/ai/*` CRUD, `status`/`validate`/`preview`/`apply`/`revisions`/`rollback`, transactional replace, AI-native runtime mapping, nested group flattening, Allowlist policy mapping, daemon reload after apply/rollback, built-in policy execution for rate limit, budget guard, PII redaction, model map, param override, system prompt, request audit, think tags, PromptGuard, PromptCompress, and SemanticCache, `semantic` target routing with optional embeddings and lexical fallback, all magpie routing strategies including `free`/`drain`/`expiring`, provider `config_json.query` parameters, streaming cache population and SSE replay, full CRUD CLI with `--body` inline/stdin/file input, typed desktop forms for providers, accounts, models, targets, policies, and attachments, typed API Gateway and MCP forms, and a Tauri OAuth loopback callback listener.
- AI target metadata now carries `context_window`, `max_output_tokens`, `capabilities`, and `pricing`, replacing the upstream metadata that used to live in `llm_models`.
- Removed: `llm_providers`, `llm_models`, `llm_routing_groups`, `llm_routes`, `llm_routing_targets`, `llm_provider_accounts`, and `llm_provider_oauth_tokens`; the legacy model control endpoints; `legacy`/`shadow`/`on` modes; `auto_import_legacy`; `ai_compat`; and `ai import-legacy`.
- SemanticCache behavior: exact canonical request keys plus a local lexical-similarity vector (words and trigrams feature-hashed into 256 dimensions), optional OpenAI-compatible embeddings with lexical fallback, TTL and LRU bounds, model/consumer/virtual-key scoping, non-streaming JSON and streaming SSE hits, stream reconstruction through `on_final_response`, and persistence in `llm_plugin_state`.
- Semantic routing behavior: the `semantic` balancer ranks targets by `health + cosine(request text, semantic_description) * 100`. When `llm.semantic_embedding` is configured it uses OpenAI-compatible embeddings with a TTL/LRU cache and 30-second failure cooldown; otherwise it uses the local lexical vector. Descriptions are AI-native; nested groups inherit and leaves may override them. Targets without vectors score by health and the fallback stays last.
- Provider breadth behavior: `config_json.query` appends validated query pairs to every upstream endpoint, and protocol presets map Ollama, vLLM, LM Studio, Groq, OpenRouter, DeepSeek, Together, Fireworks, Claude, Google, Vertex, Bedrock, and OpenAI-compatible endpoints onto six wire protocols with sensible default base URLs. Azure OpenAI works through `auth_header` + `query` + deployment-scoped `base_url` without a new protocol. Vertex reuses the Gemini payload with publisher paths; Bedrock uses the Converse API with a bearer Bedrock API key and a CRC-validating AWS event-stream framer for streaming.
- OAuth behavior: AI-native accounts expose `oauth_status`/`oauth_expires_at`, support start/complete/refresh/status/delete under `/api/v1/ai/accounts/{id}/oauth/*`, and offer desktop controls in the account form. When the redirect URI is a loopback HTTP URL, the Tauri host binds the port, validates `state`, and returns the code automatically; manual entry remains available. Tokens live in `ai_provider_oauth_tokens`, and token exchange/refresh updates the account state.
- Observability behavior: cached responses set `X-Octra-Cache` and request-audit `cache_status`; semantic cache lookups log hit/miss/match kind, and semantic routing logs target scores at debug level.
- Not implemented: Bedrock SigV4 authentication; optional table renames for the remaining non-model `llm_*` auth/telemetry state; and production hardening such as OS Keychain secrets, OAuth-token encryption, desktop daemon lifecycle management, and packaging.
- There is no legacy mode or backward-compatibility path: the AI-native control plane is the only source for model, routing, account, and policy configuration.

# References

[1] Kong, "AI Gateway 2.x concepts." https://developer.konghq.com/ai-gateway/ai-gateway-v2-concepts/
[2] Kong, "AI Gateway architecture." https://developer.konghq.com/ai-gateway/architecture/
[3] Kong, "AI Gateway entities." https://developer.konghq.com/ai-gateway/entities/
[4] yetone, "magpie." https://github.com/yetone/magpie
[5] magpie, "Gateway routing." https://github.com/yetone/magpie/blob/main/docs/subsystems/gateway-routing.md
[6] Octra ADR 0013, "Model the LLM gateway on magpie." `0013-model-the-llm-gateway-on-magpie.md`
[7] Octra ADR 0016, "Redact PII before provider egress." `0016-redact-pii-before-provider-egress.md`
[8] Octra ADR 0017, "Compile runtime snapshots off the hot path." `0017-compile-runtime-snapshots-off-the-hot-path.md`
