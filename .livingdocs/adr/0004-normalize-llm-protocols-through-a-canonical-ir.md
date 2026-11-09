---
type: ADR
title: Normalize LLM protocols through a canonical IR
description: Translate client and provider protocols through one canonical request, response, and stream representation.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-06T15:44:39Z
---

# 0004. Normalize LLM protocols through a canonical IR

## Context

Clients speak OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, and Gemini, while providers expose their own protocols and feature sets. Converting every client protocol directly to every provider protocol grows quadratically, duplicates normalization logic, and makes routing, plugins, and usage accounting protocol-specific. Streaming and tool calling must survive conversion.

## Decision

We will normalize every supported client protocol into a canonical request, response, and stream representation in `octra-llm-core`. Protocol adapters in `octra-llm-protocol` translate client protocols into that IR and provider adapters translate the IR into upstream protocols. Routing, plugin phases, usage accounting, and policy checks operate on the IR, not on a vendor payload.

Rejected alternatives:
- **Pairwise protocol conversion** — O(N²) adapters and inconsistent semantics as protocols and providers grow.
- **Transparent passthrough only** — preserves fidelity for one provider, but cannot route across protocols, apply plugins, or account for usage uniformly.
- **A separate proxy per client** — duplicates the gateway and splits configuration, authentication, and observability.

## Consequences

**Easier / gained:**
- Adding a protocol or provider costs one adapter instead of a conversion matrix.
- Plugins and routing see one stable representation across all clients.
- Protocol compatibility can be tested with shared fixtures.

**Harder / accepted trade-offs:**
- The IR must evolve to represent provider features without leaking vendor details.
- Some provider-specific fields may be dropped or approximated during conversion.
- Protocol fidelity requires an explicit compatibility matrix and fixture tests.

**Follow-ups:**
- Expand the compatibility matrix as new protocols and provider features are supported.

## Verification

**Implementation impact:** `crates/llm_gateway/octra-llm-core/src/`, `crates/llm_gateway/octra-llm-protocol/src/`, `crates/llm_gateway/octra-llm-gateway/src/`.

**Verification criteria:**
- OpenAI and Anthropic request, response, and SSE conversions round-trip through the IR in fixture tests.
- The LLM gateway routes on `virtual_model_id` and records usage without inspecting vendor-specific payloads.
- New protocol support adds an adapter module rather than branching throughout the gateway.

# References

[1] Octra repository. Available at: https://github.com/luozhouyang/octra. Accessed on: 2026-11-06.
