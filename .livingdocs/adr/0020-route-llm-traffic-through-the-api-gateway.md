---
type: ADR
title: Route LLM traffic through the API gateway as an in-process upstream
description: Make the supported OpenAI, Anthropic, and Gemini endpoint paths ordinary API gateway routes to an in-process LLM upstream, remove the separate public LLM listener, and skip global API plugins for the internal service.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-09T22:20:00Z
---

# 0020. Route LLM traffic through the API gateway as an in-process upstream

## Context

The LLM gateway originally owned `/v1` and `/v1beta` on the public data listener, alongside the API gateway's catch-all. That made HTTP edge concerns such as CORS, IP restriction, request-size limiting, and correlation IDs unavailable to LLM traffic unless they were reimplemented as LLM plugins. Those concerns only need the HTTP method, URI, headers, and client address; they do not need the canonical LLM IR.

The LLM gateway also needs to stay in the same process as the API gateway: it is local-first, there is no reason to expose a second listener, and a loopback hop would add latency, another port, and another bypass surface.

## Decision

The supported OpenAI, Anthropic, and Gemini endpoint paths are ordinary API gateway routes. They are exact patterns, not broad `/v1` or `/v1beta` prefixes: `/v1/chat/completions`, `/v1/responses`, `/v1/messages`, `/v1/messages/count_tokens`, `/v1/models`, `/v1beta/models`, and the Gemini `:generateContent` / `:streamGenerateContent` / `:countTokens` actions. They all point to one API gateway service named `llm-gateway`, tagged `llm:internal` and `managed_by:llm`.

The API gateway data plane treats a `llm:internal` service as an in-process upstream: it skips endpoint selection, load balancing, retries, HTTP timeouts, and sticky sessions, and calls the LLM router directly through an `InternalUpstream` handle. The LLM router is no longer mounted on the public listener.

The API gateway skips globally scoped plugin instances for a `llm:internal` service. Policies such as CORS, IP restriction, correlation ID, and request-size limiting are attached at service scope through the existing plugin API; they are not provisioned from a new LLM-specific configuration section. The daemon only auto-provisions the service and the two route prefixes.

LLM virtual-key authentication remains in the LLM gateway. API authentication plugins must not be attached to the `llm-gateway` service; otherwise the two authentication models would conflict.

The API gateway forwards the resolved client IP and request ID to the LLM router through request extensions. The LLM gateway uses the forwarded request ID when present so the API request log, LLM audit, and response header agree.

## Consequences

Easier:

- One HTTP front door; CORS/IP restriction/correlation ID work for LLM traffic without LLM-side code.
- No second listener, no internal port, no public LLM bypass path.
- API gateway service/route/plugin concepts are reused unchanged; there is no LLM-specific "edge" entity or configuration section.
- The LLM plugin contract is untouched.

Harder:

- The API gateway buffers the request body before `access`; the LLM router reads it again. JSON LLM requests are small enough for this to be acceptable in v1.
- Global API plugins no longer apply to LLM routes. Operators must attach policies at service scope; this is documented and tested.
- API request metrics and LLM usage metrics describe different layers and must be correlated by request ID.
- A `body_filter` plugin on the internal service would break SSE; the data plane rejects that configuration.

## Verification

- The `llm-gateway` service and the explicit OpenAI/Anthropic/Gemini endpoint routes are provisioned before the API snapshot compiles; legacy managed prefix routes are removed.
- API gateway route matching resolves both prefixes; OPTIONS preflight reaches the API `access` phase.
- `InternalUpstream` receives the original method, path, headers, and body.
- A service-scoped CORS plugin adds response headers to JSON, error, and SSE responses; a global plugin is skipped.
- The LLM virtual-key error response passes through the API response phases.
- The API request ID, the forwarded extension, the LLM response header, and the LLM audit record agree.
