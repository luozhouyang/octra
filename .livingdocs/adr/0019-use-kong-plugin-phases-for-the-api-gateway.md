---
type: ADR
title: Use Kong plugin phases for the API gateway
description: Adopt Kong's seven API plugin phases and its chunk/EOF response body_filter, with plugin-local full-body buffering and no request-body filter.
owner: luozhouyang
status: Proposed
supersedes: 0012
timestamp: 2026-11-09T10:02:01Z
---

# 0019. Use Kong plugin phases for the API gateway

## Context

Octra's API gateway plugin contract drifted from Kong: it exposed `request`, `upstream_request`, `response_body`, and `error` phases, two body-buffering booleans, and full-body hooks. A full-body plugin forced the whole chain to buffer, so streaming plugins could not process chunks. Kong instead defines seven phases and a single response-body filter that receives a chunk and an EOF flag; plugins that need a full body buffer it in their own request context.

## Decision

The API gateway plugin contract will use exactly Kong's phases and body model. This is a hard break: the old phases, body-buffering flags, and compatibility adapters are removed, and the API plugin contract version is bumped.

### Phases

The phases, in order, are `certificate`, `rewrite`, `access`, `response`, `header_filter`, `body_filter`, and `log`.

- `certificate` is declared for Kong parity but is not invoked until the API listener terminates TLS. External TLS termination never invokes it.
- `rewrite` runs before `access` with the request head.
- `access` authenticates the caller, resolves the consumer, permits service-request changes, and reads or writes the complete request body.
- `response` runs when upstream response headers arrive and may replace the response.
- `header_filter` may change status and headers.
- `body_filter` processes each response-body chunk and the EOF event.
- `log` is observational and runs after the response completes.

There is no plugin `error` phase. Gateway and upstream errors are represented as responses for `response`/`header_filter` and recorded by `log`. There are no separate `request`, `upstream_request`, `response_body`, or `error` phases.

Route matching remains before the plugin chain (matcher-first). Moving `rewrite` before route matching is a separate routing change and is not part of this decision.

### Request body

Kong has no request-body filter. The host buffers the complete request body before the `access` phase, subject to `gateway.max_body_bytes`. A plugin reads it with `request_body()` and replaces it with `set_request_body()`, which updates `Content-Length` before the upstream request. Authentication plugins such as `key-auth` with `key_in_body` use the same cached body. Request bodies are never delivered as chunk/EOF events.

### Response body

The only response-body hook is `body_filter`, whose context has a mutable `chunk: Option<Bytes>` and a read-only `eof: bool`, directly mirroring `ngx.arg[1]` and `ngx.arg[2]`. Every body plugin is called for every chunk and once for EOF, in plugin priority order. The last data chunk carries `eof = true`; an empty body produces one `chunk = None, eof = true` event. A plugin may set `chunk` to `None` to suppress output, replace it, or emit the full body at EOF.

Plugins that need a complete body buffer chunks in their request-scoped plugin context and write the result back to `chunk` at EOF. Each plugin has its own context buffer; multiple full-body plugins buffer independently. The host does not pre-buffer the response globally and does not provide a shared complete-body cache. A plugin may deliberately share data through the context's shared namespace, but the framework does not make buffering decisions for it.

Because `body_filter` runs after response headers are committed, a body-filter failure is a late error: the host truncates or aborts the body and records the failure. It cannot convert the response to a structured `500`. A chain with any `body_filter` plugin removes `Content-Length` after `header_filter` and sends a chunked response.

### Plugin context and state

Every request gets a request-scoped plugin context with a shared namespace and a per-plugin namespace. The context persists from `rewrite`/`access` through `body_filter` and `log`. It is never stored on the shared plugin instance; concurrent requests cannot observe each other's buffers.

`on_event` and `on_job` are not request lifecycle phases. They move to a separate background plugin interface; `ApiGatewayPlugin` only exposes the seven phases.

### Removed configuration

The route-level `request_buffering` and `response_buffering` flags are removed. Body behavior is determined by the Kong phase and by each plugin's own buffering, not by a route-wide buffering switch.

### LLM plugins

The LLM plugin contract is unchanged: `on_request`, `on_provider_request`, `on_response`, `on_stream_event`, `on_error`, `on_final_response`, and `on_log`, with consumer, virtual-key, model, provider, and global scopes. It operates on the canonical IR, not HTTP body chunks.

## Rejected alternatives

- Keeping the old Octra phases alongside Kong phases: produces two incompatible lifecycles and ambiguous body behavior.
- A host-wide "needs full body" decision: unnecessarily blocks streaming plugins and does not match Kong.
- A shared mutable full-body buffer: creates ordering ambiguity and plugin coupling; Kong leaves buffering to plugins.
- A request-body chunk filter: not part of Kong's API gateway model.
- Returning structured `500` from a late `body_filter`: impossible after headers are committed; holding every response until EOF would remove streaming.

## Consequences

Easier:

- One phase vocabulary matches Kong, so plugin behavior and ordering are easier to reason about.
- Streaming plugins run per chunk even when another plugin later needs the full body.
- Full-body plugins have a single, explicit pattern: buffer in the request context and emit at EOF.
- Request-body plugins use one cached full-body API, matching `kong.request.get_body`.

Harder:

- A full-body plugin is a barrier; downstream plugins cannot see output before EOF.
- Multiple full-body plugins each buffer their input.
- Response-body failures are late failures and cannot change status.
- Existing native and WASM plugins must be rewritten; there is no compatibility mode.
- `certificate` remains dormant until TLS termination exists.

## Verification

- A streaming body plugin before a full-body plugin receives one call per chunk.
- A full-body plugin receives one EOF call and emits a transformed complete body.
- A full-body plugin before a streaming plugin causes the latter to receive one chunk with `eof = true`.
- Empty bodies produce one EOF event.
- Request-body plugins read once through `request_body()` and changes reach the upstream.
- `key_in_body` authentication works without a request body filter.
- A `body_filter` failure after commit is recorded as a late error.
- `Content-Length` is absent when a body filter is configured.
- Native and WASM plugins use the same phase semantics; the API plugin contract version is bumped.
