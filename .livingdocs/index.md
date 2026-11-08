---
okf_version: "0.1"
---

# Documentation

## Status

- **Constitution:** Draft; not ratified.
- **PRD 0001:** Draft; the v0.1 implementation is feature-complete for the core paths, with the acceptance gaps recorded in the PRD implementation-status table.
- **ADRs:** Proposed; the implementation already follows most of them, but they remain pending acceptance. ADR 0018 has Phase 40 (AI-native only: schema, compiler, control API, rollback, full policy execution, semantic routing with optional embeddings, streaming cache, OAuth controls with loopback callbacks, provider presets, Vertex/Bedrock adapters with optional SigV4, encrypted and rotatable secrets, all routing strategies, observability, full CRUD CLI/UI) implemented; the legacy model tables, endpoints, modes, and import path are removed. Signing/notarization credentials and key retirement are pending.
- **Reference models:** Kong Gateway for the API gateway, yetone/magpie for the LLM gateway (with Kong AI Gateway 2.x as a secondary control-plane reference), and Kong's AI MCP Proxy for the MCP gateway. See [Reference Models](architecture/reference-models.md).
- **LLM control plane:** Phase 35 made `ai_*` the only model control plane: compiler, `AiStore`, `/api/v1/ai/*` CRUD, apply/revisions/rollback, revision snapshots, transactional replace, built-in policy execution for every policy type including PromptGuard, PromptCompress, and SemanticCache, `semantic` target routing with optional embeddings and lexical fallback, all magpie routing strategies, provider query parameters and protocol presets, Vertex and Bedrock Converse adapters with SigV4, streaming cache population/SSE replay, AI-native account OAuth with desktop controls and loopback callbacks, encrypted stored secrets, cache/routing observability, full CRUD CLI, and typed desktop forms for AI/API/MCP/consumer-policy entities. Non-model `llm_*` tables were renamed to domain-neutral names. The legacy model tables and `/admin/llm/*` model endpoints are gone, with no backward compatibility. See [LLM Control Plane](architecture/llm-control-plane.md).
- **API, MCP, and events:** Phase 27 implemented least-connections and latency EWMA target selection; Phase 28 added consumer-group and tool-default MCP ACLs; Phase 29 added event-bus delivery/retry/job tests; Phase 30 added Bedrock SigV4; Phase 31 moved consumer policies to a typed form; Phase 32 added MCP group selection; Phase 34 aligned MCP server `type`/`sources` and server-default ACLs with Kong; Phase 35 added encrypted stored secrets and desktop daemon lifecycle; Phase 36 added key rotation/AAD and race-free KeyAuth lookup; Phase 37 added per-server routes and CEL ACLs; Phase 38 added macOS signing/universal packaging; Phase 39 added OIDC auth strategies; Phase 40 added OpenAPI conversion modes. See [Reference Models](architecture/reference-models.md) and [API Request Path](architecture/api-request-path.md).

## Trail

* [Constitution](constitution.md)
* [ADRs](adr/)
* [Architecture](architecture/)
* [PRDs](prd/)
