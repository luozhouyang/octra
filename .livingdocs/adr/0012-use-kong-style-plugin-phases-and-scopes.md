---
type: ADR
title: Use Kong-style plugin phases and scopes
description: Run API plugins through Kong-style phases and 12-level scope precedence, and run LLM plugins through a typed lifecycle with provider, model, key, and consumer scopes.
owner: luozhouyang
status: Superseded
superseded_by: 0019
timestamp: 2026-11-07T12:40:00Z
---

> **SUPERSEDED — do not act on this record.** Replaced by [0019](0019-use-kong-plugin-phases-for-the-api-gateway.md). Run `living-docs read` for what is in force.

# 0012. Use Kong-style plugin phases and scopes

## Context

Octra has two plugin surfaces. The API gateway needs request and response hooks around an HTTP proxy. The LLM gateway needs typed stages around a canonical IR, provider selection, streaming, and usage accounting. The reference model for the API surface is Kong's plugin system: plugins run in defined phases and can be configured globally, on a Service, on a Route, on a Consumer, or on a Consumer Group. Kong resolves multiple instances of the same plugin by scope specificity, and its phases include rewrite, access, header_filter, body_filter, response, and log.

Octra also has two runtime tiers (ADR 0005): trusted in-process Rust plugins and sandboxed WASM components. The phase and scope model must work for both.

## Decision

We will use Kong-style phases and scope precedence for API plugins, and a separate typed lifecycle for LLM plugins.

**API plugin phases:** the contract declares `certificate`, `rewrite`, `request`, `upstream_request`, `response`, `response_body`, `error`, and `log`. The current HTTP data plane executes `request`, `upstream_request`, `response`, `response_body`, `error`, and `log`; `certificate` and `rewrite` are reserved for a future TLS/proxy phase and are not invoked yet. The request path runs pre-auth and post-auth chains; consumer and consumer-group scoped instances are deferred until after authentication so the most specific instance can replace the global one.

**Scope precedence:** the 12 Kong combinations are implemented, from most to least specific: consumer + route + service, consumer group + route + service, consumer + route, consumer + service, consumer group + route, consumer group + service, route + service, consumer, consumer group, route, service, global. Within the same specificity, manifest priority (descending) and plugin name (ascending) order the chain; a configured `before` / `after` constraint can override priority.

**Filtering:** each instance declares the protocols it supports. A plugin that fails can fail open or fail closed according to its manifest, except authentication plugins, which always fail closed. Plugins receive only the capabilities they declare: request/response headers or bodies, log, KV, network, secret, storage entities, auth, admin API, events, and jobs.

**Built-in API plugins:** `correlation-id`, `cors`, `rate-limiting`, `circuit-breaker`, `header-rewrite`, `key-auth`, `basic-auth`, `jwt`, `acl`, `ip-restriction`, `request-size-limiting`, `request-termination`, `request-transformer`, and `response-transformer`.

**LLM plugin lifecycle:** `on_request`, `on_provider_request`, `on_response`, `on_stream_event`, `on_error`, and `on_log`. LLM scopes run in this precedence order: consumer, virtual key, model, provider, global. The chain receives the canonical IR rather than an HTTP body.

**Runtime tiers:** built-in plugins are native Rust; third-party API and LLM plugins are WASM components with a WIT ABI, a manifest, approved capabilities, and the plugin-owned storage boundary from ADR 0006.

**Not adopted:**

- Kong Lua plugin compatibility and the Kong plugin Development Kit (PDK). Octra's built-ins are Rust and its third-party ABI is WIT.
- A single generic plugin trait across API, LLM, and MCP. Each surface has a different context and failure model.
- A plugin marketplace or remote plugin repository in v0.1.

## Rejected alternatives

- **Kong Lua compatibility** — a different execution model, a large PDK surface, and no sandbox for untrusted Lua.
- **One generic plugin trait** — forces API, LLM, MCP, and provider plugins into an over-broad context and makes capability review meaningless.
- **Raw SQL or unrestricted host access for plugins** — violates ADR 0006 and the WASM capability boundary.
- **Only global plugins** — loses Kong's most useful operator feature and makes per-route policy impossible.

## Consequences

**Easier / gained:**

- Operators can reason about plugins with Kong's phase and scope vocabulary.
- The same phase model works for native and WASM plugins.
- Consumer and consumer-group scoping are available without special cases in each plugin.
- Failed plugins have an explicit fail policy instead of an implicit one.

**Harder / accepted trade-offs:**

- There are two plugin contracts and two authoring paths to maintain.
- Phase names are Kong-inspired but not identical; a Kong plugin cannot be reused directly.
- The 12-level precedence matrix needs dedicated tests to avoid subtle regressions.
- WASM plugin instantiation, fuel, memory, timeout, and body limits add overhead.

**Follow-ups:**

- Add a documented phase mapping table between Kong phases and Octra phases.
- Add a precedence-matrix test fixture covering all 12 combinations.
- Add an effective-chain view for API plugins, matching the LLM `effective` endpoint.
- Add operational metrics for plugin failures by phase and fail policy.

## Verification

**Implementation impact:** `crates/api_gateway/octra-api-plugin-types/src/lib.rs`, `crates/api_gateway/octra-api-plugin-runtime/src/lib.rs`, `crates/api_gateway/octra-api-plugin-builtin/src/`, `crates/api_gateway/octra-api-plugin-wasm/`, `crates/octra-llm-plugin-types/src/lib.rs`, `crates/octra-llm-plugin-runtime/src/`, `crates/octra-llm-plugin-builtin/src/`, `crates/octra-llm-plugin-wasm/`.

**Verification criteria:**

- A plugin configured at consumer + route + service wins over route + service, consumer, route, service, and global instances of the same plugin.
- A consumer-group-scoped plugin applies only to members of that group.
- The request, response, stream, error, and log stages run in the documented order.
- A failing plugin follows its fail-open or fail-closed policy.
- WASM plugins run with only the capabilities listed in the approved manifest.
- `cargo test -p octra-api-plugin-runtime -p octra-llm-plugin-runtime -p octra-api-plugin-wasm -p octra-llm-plugin-wasm` passes.

# References

[1] Kong Gateway, "Plugins." https://developer.konghq.com/gateway/entities/plugin/
[2] Kong Gateway, "Plugin precedence." https://developer.konghq.com/gateway/entities/plugin/#plugin-precedence
[3] Octra ADR 0005, "Use native Rust and WASM plugin tiers." `0005-use-native-rust-and-wasm-plugin-tiers.md`
[4] Octra ADR 0006, "Give each plugin host-owned isolated storage." `0006-give-each-plugin-host-owned-isolated-storage.md`
[5] Octra repository, `crates/api_gateway/octra-api-plugin-*` and `crates/octra-llm-plugin-*`. https://github.com/luozhouyang/octra
