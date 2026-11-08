---
type: Architecture View
title: Reference Models
description: The Kong, magpie, and Kong MCP concepts Octra adopts, the parts it deliberately does not adopt, and the current implementation status of each reference model.
kind: context
timestamp: 2026-11-07T12:40:00Z
---

# Reference Models

Octra is a local-first composite of three primary product references. Kong AI Gateway 2.x is a secondary architecture reference for the LLM control plane: Octra borrows its AI-native entity separation and policy attachments, but not its Konnect/hybrid deployment. This view records what was studied, what was adopted, what was rejected, and where the current code still differs. It is the bridge between the research and the ADRs; the ADRs hold the decisions, and the PRD holds the acceptance status.

```mermaid
flowchart LR
    KongAPI[Kong Gateway: API concepts] --> APIGW[Octra API Gateway]
    Magpie[magpie: local LLM gateway] --> LLMGW[Octra LLM Gateway]
    KongAI[Kong AI Gateway 2.x: AI-native control plane] -. architecture reference .-> LLMGW
    KongMCP[Kong AI MCP Proxy: fronting and aggregation] --> MCPGW[Octra MCP Gateway]
    APIGW --> Shared[Unified control plane, SQLite, plugin runtimes]
    LLMGW --> Shared
    MCPGW --> Shared
```

## API gateway — Kong Gateway

**Adopted**

- `service`, `route`, `upstream`, and `target` entities, with a Service pointing at a direct URL or an Upstream virtual host.
- Route matching on protocol, host, path, method, and headers; route ordering by `regex_priority`, longest path, then id.
- Round-robin, weighted round-robin, and consistent hashing with Kong-style `hash_on` / `hash_fallback` inputs.
- Passive and active health checks driven by the Upstream `healthchecks` document.
- Kong-style plugin phases and the 12-level scope precedence matrix, including consumer groups.
- Kong-shaped Admin API under `/admin` with `{data, next, offset}` list envelopes.
- Consumer, credential, ACL, and API-key concepts; trusted-proxy client IP resolution.

**Not adopted**

- Kong's Nginx data path, Lua plugin ABI/PDK, workspaces, RBAC, Dev Portal, Vitals, and full Admin API compatibility.
- Sticky-session cookies. The `least-connections` and `latency` selectors are implemented with in-flight and latency-EWMA signals.
- A declarative Kong configuration file as the source of truth; SQLite is the source of truth.

**Current status**

| Capability | Status | Evidence / gap |
|---|---|---|
| Service/route/upstream/target CRUD | Implemented | `/admin/*`, `migrations/0009_gateway_entities.sql` |
| Route matching and priority | Implemented | `crates/octra-runtime/src/matcher.rs` |
| Round-robin, weighted, consistent hashing, least-connections, latency | Implemented | `crates/octra-runtime/src/balancer.rs` |
| Active/passive health checks | Implemented | `balancer.rs`, `check_health` loop |
| Plugin phases and 12-level scopes | Implemented | `crates/octra-api-plugin-runtime/src/lib.rs` |
| Kong-shaped Admin API | Implemented | `crates/octra-api-gateway/src/admin/` |
| UI/CLI CRUD for API entities | Implemented | Desktop typed forms and `octra services/routes/upstreams/targets` |

## LLM gateway — yetone/magpie

**Adopted**

- One local endpoint speaking OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, and Gemini.
- Protocol translation through the canonical IR, including streaming and tool calls.
- Virtual models and routing groups with priority rounds, weighted cycles, and fallback.
- Provider accounts with static or OAuth credentials, quota, cooldown, pacing, and EWMA health.
- Virtual-key and consumer policies for allowed models/providers/accounts and token/cost limits.
- Usage, cost, hourly rollups, and retention.
- Opt-in reversible or irreversible privacy masking with local or loopback engines.
- Agent configuration adapters that point Claude Code, Codex, and OpenCode at the gateway.

**Not adopted**

- magpie's signed-in subscription discovery, menu-bar app, remote sharing, Docker/NAS mode, prompt/skill library, and Bun/JavaScript plugin runtime.
- magpie's classifier-based intent routing; Octra ranks on a client-supplied `x-octra-intent` header.

**Current status**

| Capability | Status | Evidence / gap |
|---|---|---|
| Four ingress protocols + IR | Implemented | `crates/octra-llm-protocol`, `crates/octra-llm-core` |
| Vertex and Bedrock Converse upstream adapters | Implemented | `adapter.rs`, `octra-llm-protocol/src/bedrock/`, `data_plane/stream.rs` |
| Streaming, tool calls, usage | Implemented | `crates/octra-llm-gateway/src/data_plane/` |
| Virtual models and routing groups | Implemented | `routing.rs`, `migrations/0003_llm_gateway.sql` |
| Priority/order/manual/weight/rotate/fast/cheap/free/drain/expiring/smart/usage/quota/pace/intent | Implemented | `routing.rs`, `data_plane/mod.rs` |
| Provider accounts, quota, cooldown, pacing | Implemented | `accounts.rs` |
| OAuth refresh | Implemented, plaintext at rest | `credentials/`, `migrations/0017_oauth_credentials.sql` |
| Virtual-key and consumer policies | Implemented, with consumer-policy Admin API, CLI, and desktop editor | `vk_policy.rs`, `runtime.rs`, `/admin/policies` |
| Usage, cost, rollups, retention | Implemented | `usage.rs`, `cost.rs`, `migrations/0014_usage_analytics.sql` |
| PII masking | Implemented, opt-in | `llm-pii-redaction`, `pii/` |
| Agent config adapters | Implemented | `crates/octra-agent-core`, ADR 0008 |
| AI-native LLM control plane | Phase 26 AI-native only: control API + full policy execution + semantic routing + streaming cache + all routing strategies + Vertex/Bedrock adapters + typed forms | `ai_*` schema, compiler, store, `/api/v1/ai/*` CRUD, apply/revisions/rollback, transactional replace, AI-native runtime mapping, per-target model metadata, built-in policy execution for every policy type including PromptGuard, PromptCompress, and SemanticCache, semantic target routing over AI-native descriptions, provider query parameters and presets, Vertex and Bedrock Converse adapters, streaming cache population and SSE replay, OAuth desktop controls and loopback callback, full CLI CRUD with stdin/file bodies, and typed desktop forms for all AI/API/MCP entities exist; the legacy model tables, endpoints, modes, and import path are removed without backward compatibility; see ADR 0018 and [LLM Control Plane](llm-control-plane.md) |

### Secondary architecture reference — Kong AI Gateway 2.x

magpie remains the primary LLM product reference. Kong AI Gateway 2.x contributes the control-plane shape proposed in ADR 0018:

**Adopted (target design):**

- AI Model Provider, AI Model, AI Model Target, AI Policy, and AI Policy Attachment as first-class entities.
- A virtual model that carries capabilities, formats, targets, and a balancer.
- Policies attached to scopes (global, provider, model, target, consumer group, consumer, virtual key) rather than scattered through key permissions and plugin configs.
- Config validation, dry-run, atomic apply, and revision/rollback.
- AI consumers/groups and reusable provider credentials (Octra reuses its existing consumers, virtual keys, and secret references).

**Not adopted:**

- Konnect-managed control plane and hybrid data-plane registration.
- Mandatory Redis, vector database, or Lua policy runtime.
- Kong AI Gateway 2.x's A2A `AI Agent` entity; Octra's coding-agent configuration remains a separate `coding_agents` domain.
- Enterprise workspaces, RBAC, and multi-tenant control-plane semantics.

**Current status:** Phase 26 is AI-native only: control API, full policy execution, all magpie routing strategies, semantic routing with optional embeddings, streaming cache, Vertex and Bedrock Converse adapters, AI-native account OAuth with desktop controls and loopback callbacks, cache/routing observability, provider query parameters and protocol presets, full CRUD CLI, and typed forms for AI, API Gateway, and MCP entities. The `ai_*` schema, compiler, storage store, `/api/v1/ai/*` CRUD, validate/preview/apply/revisions/rollback, revision snapshots, transactional replace, AI-native runtime mapping, per-target context/capability/pricing metadata, built-in policy execution for every policy type including PromptGuard, PromptCompress, and SemanticCache, semantic target routing over AI-native descriptions with embedding vectors and lexical fallback, provider query parameters and presets, streaming cache population and SSE replay, account OAuth endpoints, complete CLI CRUD, and the AI Control page are implemented and tested. The legacy model tables, `/admin/llm/*` model endpoints, `legacy`/`shadow`/`on` modes, and import-legacy path are removed. Bedrock SigV4 and production hardening remain future work. See ADR 0018 and [LLM Control Plane](llm-control-plane.md).

## MCP gateway — Kong AI MCP Proxy

**Adopted**

- Fronting upstream MCP servers behind one managed endpoint; the `passthrough-listener` / `listener` half of Kong's model.
- Streamable HTTP ingress at `/mcp` with `initialize`, `ping`, `tools/list`, `tools/call`, and session binding.
- Tool aggregation with a stable `<server>__<tool>` namespace.
- API-key authentication and per-consumer, per-group, and per-tool-default ACLs.
- Invocation audit rows for calls, denials, and unknown tools.

**Not adopted**

- Kong's `conversion-listener`, `conversion-only`, tag-based `listener` aggregation, and `upstream-server` modes; Octra does not convert OpenAPI operations into MCP tools.
- Kong auth plugins (Key Auth, OIDC, etc.) as the MCP authentication mechanism; Octra uses its own MCP keys.
- Kong's plugin-based MCP proxy model; Octra's MCP gateway is a first-class router, and API/LLM plugin chains do not run on MCP traffic.
- CEL-based ACL expressions.
- MCP protocol revision `2025-11-25`; Octra negotiates `2025-06-18`, `2025-03-26`, and `2024-11-05`.
- Server-initiated notifications, tool-list change notifications, session sharing, and non-HTTP upstreams beyond stdio.

**Current status**

| Capability | Status | Evidence / gap |
|---|---|---|
| Single `/mcp` endpoint | Implemented | `crates/octra-mcp-gateway/src/data.rs` |
| Streamable HTTP + stdio upstreams | Implemented | `http.rs`, `stdio.rs` |
| Sessions and DELETE | Implemented | `data.rs`, `mcp_sessions` |
| Tool discovery and aggregation | Implemented | `servers.rs`, `mcp_tools` |
| `<server>__<tool>` namespace | Implemented | `octra-mcp-core/src/protocol.rs` |
| MCP API keys | Implemented | `auth.rs`, `mcp_api_keys` |
| Consumer/group/default tool ACL | Implemented | `mcp_tool_acls`, `acl_allows_scoped` |
| CEL ACL | Not implemented | Scope rules only; no expression language |
| Invocation audit | Implemented | `mcp_invocations` |
| OpenAPI-to-MCP conversion | Not implemented | Deliberately out of v0.1 |

## Desktop and control plane

The UI is a live control-plane console with targeted editors. Overview, API Gateway, LLM Gateway, MCP Gateway, Agents, Plugins, and Settings all read live data. AI Control, API Gateway, and MCP expose typed-form CRUD; the LLM page edits consumer policies; agents, LLM plugin enable/disable, and PII model actions retain their mutation surfaces. The Tauri host holds the control token, exposes one path- and method-allow-listed bridge command, and adds a narrow loopback OAuth callback listener (ADR 0015).

## Sources

The external documentation below was reviewed on 2026-11-07. Kong references reflect the current Kong developer documentation; magpie references reflect the repository main branch at that time. Treat the linked pages, not this summary, as the authority for upstream behavior.

- Kong Gateway proxying: https://developer.konghq.com/gateway/traffic-control/proxying/
- Kong Gateway routes: https://developer.konghq.com/gateway/entities/route/
- Kong Gateway services: https://developer.konghq.com/gateway/entities/service/
- Kong Gateway upstreams: https://developer.konghq.com/gateway/entities/upstream/
- Kong Gateway targets: https://developer.konghq.com/gateway/entities/target/
- Kong load balancing: https://developer.konghq.com/gateway/traffic-control/load-balancing-reference/
- Kong health checks: https://developer.konghq.com/gateway/traffic-control/health-checks-circuit-breakers/
- Kong plugins: https://developer.konghq.com/gateway/entities/plugin/
- Kong AI Gateway 2.x concepts: https://developer.konghq.com/ai-gateway/ai-gateway-v2-concepts/
- Kong AI Gateway architecture: https://developer.konghq.com/ai-gateway/architecture/
- Kong AI MCP Proxy: https://developer.konghq.com/plugins/ai-mcp-proxy/
- Kong MCP aggregation: https://developer.konghq.com/ai-gateway/v1/mcp/aggregate-mcp-tools/
- magpie: https://github.com/yetone/magpie
- magpie routing: https://github.com/yetone/magpie/blob/main/docs/subsystems/gateway-routing.md
- MCP specification 2025-06-18: https://modelcontextprotocol.io/specification/2025-06-18

## Drift instrument

Drift caught by: comparing this table with the code paths named in each row, the ADR verification criteria, and the PRD implementation-status table. There is no generated reference-conformance test yet; a future fixture should assert the adopted/not-adopted boundaries (for example, that CEL ACLs remain rejected while consumer/group/default scopes are accepted).
