---
type: Architecture View
title: Plugin Runtime
description: The API and LLM plugin contracts, phase and scope execution, native and WASM tiers, plugin storage, and durable events and jobs.
kind: flow
timestamp: 2026-11-07T12:40:00Z
---

# Plugin Runtime

Octra has two plugin contracts (API and LLM) and two runtime tiers (native Rust and sandboxed WASM). The host compiles configured instances into chains once per mutation; the data plane performs map lookups and in-memory calls.

```mermaid
flowchart TB
    Manifest[Manifest + config] --> Registry[Plugin registry]
    Config[plugin_instances / LLM plugin config] --> Compile[Compile chains]
    Registry --> Compile
    Compile --> APICHAIN[API pre-auth / post-auth chain]
    Compile --> LLMCHAIN[LLM request / provider chain]
    APICHAIN --> APIPhases["Certificate -> Rewrite -> Access -> Response -> HeaderFilter -> BodyFilter -> Log (certificate dormant until TLS)"]
    LLMCHAIN --> LLMStages["on_request -> on_provider_request -> on_response / on_stream_event -> on_error -> on_log"]
    APIPhases --> Host[Host capabilities]
    LLMStages --> Host
    Host --> Storage[(plugin data.db)]
    Host --> Events[(events + event_deliveries)]
    Host --> Jobs[(plugin_jobs)]
    Registry --> WASM[WASM component runtime]
    Registry --> Native[Native Rust factories]
```

## API plugins

- Phases: the contract uses Kong's seven phases exactly: `certificate`, `rewrite`, `access`, `response`, `header_filter`, `body_filter`, and `log`. `certificate` remains dormant until the API listener terminates TLS. There is no `request`, `upstream_request`, `response_body`, or `error` phase.
- Request bodies are not filtered as chunks. A plugin reads or replaces the complete request body in `access` through `request_body()` / `set_request_body()`, matching `kong.request.get_body()` and `kong.service.request.set_body()`.
- Response bodies use `body_filter`: every body plugin receives the current `chunk` and `eof` on every body event, in priority order. A plugin mutates `chunk` directly (including setting it to `None`). A full-body plugin buffers chunks in its request-scoped plugin context and writes the complete result back at EOF; the host does not pre-buffer and provides no shared full-body cache. Multiple full-body plugins buffer independently.
- `body_filter` runs after response headers are committed. A late failure truncates or aborts the body and cannot return a structured `500`. WASM body filters receive the same chunk/EOF contract and use host `body-append` / `body-take` functions for request-scoped full-body buffering; host instances are serialized per plugin, so the buffer is keyed by request id.
- Route-level `request_buffering` and `response_buffering` are removed.
- Scope precedence: the 12 Kong combinations from consumer + route + service down to global. Consumer and consumer-group instances are deferred until after authentication.
- Ordering: manifest priority descending, then plugin name ascending; `ordering.before` / `ordering.after` can override.
- Fail policy: `fail_open` or `fail_closed` from the manifest; a closed failure returns a gateway error.
- Capabilities: request/response headers, request/response bodies, log, KV, network, secret, storage entities, auth, admin API, events, jobs.
- Built-ins: `correlation-id`, `cors`, `rate-limiting`, `circuit-breaker`, `header-rewrite`, `key-auth`, `basic-auth`, `jwt`, `acl`, `ip-restriction`, `request-size-limiting`, `request-termination`, `request-transformer`, `response-transformer`.

## LLM plugins

- Stages: `on_request`, `on_provider_request`, `on_response`, `on_stream_event`, `on_error`, `on_log`, `on_final_response`.
- Scopes: virtual key, consumer, model, provider, global.
- Built-ins: `budget-guard`, `intent-hint`, `request-audit`, `model-map`, `param-override`, `rate-limit`, `llm-pii-redaction`, `system-prompt`, `think-tags`, `llm-prompt-guard`, `llm-prompt-compress`, `llm-semantic-cache`.
- The plugin sees the canonical IR, response headers, usage, and stream events. It can reject, rewrite, replace stream events, short-circuit with a cached canonical response at the request stage, or add response headers. `on_final_response` runs after every response transform and before protocol encoding; it is read-only and best-effort.

## Runtime tiers

- **Native:** first-party Rust factories registered in the daemon. They have full host access and no WASM overhead.
- **WASM:** third-party components with a WIT ABI, a manifest, approved capabilities, fuel/memory/timeout/body limits, and a host-owned per-plugin SQLite database. The API plugin contract version is 3 (Kong seven phases, chunk/EOF body filtering, and a separate background event/job interface); the LLM plugin contract version is 6.
- **Install/upgrade:** artifacts are stored with a hash, optional Ed25519 signature, manifest, and approved capabilities. A plugin instance is compiled only after the artifact passes layout and signature policy. Uninstall keeps plugin data unless `purge=true`; destructive migrations need explicit approval.

## Storage

Each plugin name gets its own SQLite database under the plugin root. The host owns DDL and applies ordered migrations; plugins use a capability-gated entity/KV API, never raw SQL. The database is bounded by `plugins.max_plugin_db_bytes`.

## Events and jobs

Event and job hooks are a separate background plugin interface; they are not phases of `APIGatewayPlugin`.

- `api.request.completed` is published by the API gateway when a subscriber matches the topic.
- Subscribed events are persisted in `events`; one `event_deliveries` row is created per matching subscriber. Delivery is at-least-once with exponential backoff and bounded attempts.
- Scheduled jobs are declared in the manifest, reconciled into `plugin_jobs` on each tick, and run with the plugin timeout. Terminal events are pruned by retention; pending events are never pruned.
- The control API exposes `/admin/events`, `/admin/events/{id}`, `/admin/jobs`, and `/admin/jobs/{id}/run`.

## Drift instrument

Drift caught by: `crates/api_gateway/octra-api-plugin-runtime` unit tests, `crates/api_gateway/octra-api-plugin-wasm/tests/`, `crates/api_gateway/octra-api-gateway/tests/plugins/`, `crates/octra-llm-plugin-runtime` tests, `crates/octra-llm-plugin-wasm/tests/wasm_e2e.rs`, and the `octra-api-plugin-events` delivery/retry/job tests.
