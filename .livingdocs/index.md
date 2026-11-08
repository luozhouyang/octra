---
okf_version: "0.1"
---

# Documentation

## Status

- **Constitution:** Draft; not ratified.
- **PRD 0001:** Draft; the v0.1 implementation is feature-complete for the core paths, with the acceptance gaps recorded in the PRD implementation-status table.
- **ADRs:** Proposed; the implementation already follows most of them, but they remain pending acceptance. ADR 0018 has Phase 26 (AI-native only: schema, compiler, control API, rollback, full policy execution, semantic routing with optional embeddings, streaming cache, OAuth controls with loopback callbacks, provider presets, Vertex/Bedrock adapters, all routing strategies, observability, full CRUD CLI/UI) implemented; the legacy model tables, endpoints, modes, and import path are removed. Bedrock SigV4 and production hardening are pending.
- **Reference models:** Kong Gateway for the API gateway, yetone/magpie for the LLM gateway (with Kong AI Gateway 2.x as a secondary control-plane reference), and Kong's AI MCP Proxy for the MCP gateway. See [Reference Models](architecture/reference-models.md).
- **LLM control plane:** Phase 26 made `ai_*` the only model control plane: compiler, `AiStore`, `/api/v1/ai/*` CRUD, apply/revisions/rollback, revision snapshots, transactional replace, built-in policy execution for every policy type including PromptGuard, PromptCompress, and SemanticCache, `semantic` target routing with optional embeddings and lexical fallback, all magpie routing strategies, provider query parameters and protocol presets, Vertex and Bedrock Converse adapters, streaming cache population/SSE replay, AI-native account OAuth with desktop controls and loopback callbacks, cache/routing observability, full CRUD CLI, and typed desktop forms for AI/API/MCP entities. The legacy model tables and `/admin/llm/*` model endpoints are gone, with no backward compatibility. See [LLM Control Plane](architecture/llm-control-plane.md).

## Trail

* [Constitution](constitution.md)
* [ADRs](adr/)
* [Architecture](architecture/)
* [PRDs](prd/)
