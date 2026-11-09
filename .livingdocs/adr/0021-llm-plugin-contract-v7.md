---
type: ADR
title: LLM plugin contract v7
description: Fix usage ownership, response short-circuit provenance, response headers, streaming completion, WASM parity, and per-plugin key-value storage in one breaking contract revision.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-09T14:50:00Z
---

# 0021. LLM plugin contract v7

## Context

Contract v6 is the LLM plugin boundary used by native built-ins and third-party WASM components. The native trait is more expressive than the WASM ABI, and several behaviors are ambiguous or silently lossy:

1. **Usage has two owners.** `LlmResponseContext` exposes both `response.usage` and a separate `usage: &mut Usage`. The runtime clones `Usage`, passes the clone to the chain, and then writes it back to `response.usage` in `apply_response_chain`. A response plugin that mutates `cx.response.usage` sees its write replaced; a plugin that mutates `cx.usage` works, but the duplication is a trap.
2. **Every `Respond` is logged as a cache hit.** `LlmAction::Respond` is used by the semantic cache and by request short-circuits. The data plane sets `cache_status = "hit"` and zeroes usage for every `Respond`, so synthetic or termination responses are mislabeled in logs, metrics, and audit.
3. **Response headers are request-stage only.** `LlmRequestContext.headers` can attach a header before the upstream call, but `on_response`, which knows the actual usage and finish reason, cannot. Cost and rate-limit headers cannot be emitted accurately for non-streaming responses.
4. **Streaming has no mutable completion hook.** `on_final_response` is deliberately read-only and runs after encoding. A plugin that must buffer across events and flush a transformed answer before the terminal event has to buffer inside `on_stream_event`, intercept `Finish` itself, and hope its flush ordering matches every protocol encoder. The built-ins do this; third-party plugins cannot do it reliably.
5. **WASM lags native.** WIT has six stages and neither `stream-end` nor `final-response`; a WASM plugin cannot replicate native cache/observability behavior. WIT has no `restores_sensitive_content`, so a WASM plugin that restores PII cannot participate in the cache-safety check. WIT stage names do not match manifest phase names (`provider-request` vs `upstream_request`, `stream-event` vs `response_body`), so a manifest that declares `response_body` silently suppresses `on_stream_event`.
6. **Version surfaces disagree.** `LLM_PLUGIN_API_VERSION` is `6`, `PLUGIN_API_VERSION` is `3`, and LLM WASM manifests use `api_version: "2"`. A third-party author cannot infer the supported contract from the manifest.
7. **Host capabilities are absent for third-party plugins.** Native built-ins use host-owned storage and counters; WASM plugins have only `log` and `now-unix`. A third-party semantic cache, rate limiter, or budget guard cannot be written.

These changes all cross the trait, the context structs, the WIT world, or the manifest. Fixing them one at a time would force several incompatible contract bumps. This ADR defines one v7 revision.

## Decision

### D1. One usage owner: `response.usage`

Remove `usage` from `LlmResponseContext`. `ChatResponse::usage` is the single mutable value. The runtime stops cloning usage and writing it back. `LlmFinalResponseContext` continues to expose the response read-only.

```rust
pub struct LlmResponseContext<'a> {
    pub meta: &'a LlmRequestMeta<'a>,
    pub request_model: &'a str,
    pub response: &'a mut ChatResponse,
    pub response_headers: &'a mut LlmResponseHeaders,
    pub state: &'a mut Value,
}
```

`apply_response_chain` passes `&mut response` only. Streaming accumulation updates `response.usage` before `on_stream_end` and `on_final_response`.

### D2. `Respond` carries its provenance

```rust
pub enum LlmResponseSource {
    /// Served from a response cache; usage is not billed.
    Cache,
    /// Produced by a plugin without an upstream call.
    Synthetic,
    /// Produced to terminate the request early, for example a policy denial.
    Termination,
}

pub enum LlmAction {
    Continue,
    Respond {
        response: Box<ChatResponse>,
        source: LlmResponseSource,
    },
    Reject(GatewayError),
}
```

`LlmRequestOutcome::Respond` carries the same source. The data plane behavior is:

| Source | `cache_status` | Usage billing | `provider_id` |
|---|---|---|---|
| `Cache` | `hit` | zeroed | none |
| `Synthetic` | none | from `response.usage` | none |
| `Termination` | none | from `response.usage` | none |

`LlmLogEvent` gains `response_source: Option<&str>` (`"cache"`, `"synthetic"`, `"termination"`). `request_audit` includes it. Only `Cache` sets `cache_status = "hit"` and zeroes usage.

### D3. Response headers are writable at request and response stages

Rename `LlmRequestContext.headers` to `response_headers` and add the same sink to `LlmResponseContext`:

```rust
pub struct LlmRequestContext<'a> {
    pub meta: &'a LlmRequestMeta<'a>,
    pub response_headers: &'a mut LlmResponseHeaders,
    pub request: &'a mut ChatRequest,
    pub state: &'a mut Value,
}

pub struct LlmResponseContext<'a> {
    pub meta: &'a LlmRequestMeta<'a>,
    pub request_model: &'a str,
    pub response: &'a mut ChatResponse,
    pub response_headers: &'a mut LlmResponseHeaders,
    pub state: &'a mut Value,
}
```

`on_response` runs only for non-streaming responses, before protocol encoding, so its headers reach the client. For streaming responses, headers are committed before the stream starts; `on_request` is the only header hook. This limitation is documented rather than hidden.

`LlmErrorContext` does not gain a header sink. Headers added in `on_request` still apply to error responses because the host attaches the sink after the data plane produces any response.

### D4. Mutable stream completion: `on_stream_end`

Add a stream-completion hook that runs once, after the upstream stream ends and before the terminal event is emitted. It can append non-terminal events to flush buffered content. It cannot fail the request or change the status code.

```rust
pub struct LlmStreamEndContext<'a> {
    pub meta: &'a LlmRequestMeta<'a>,
    /// Accumulated canonical response so far, including usage from a terminal
    /// `Usage` event. Read-only: the hook contributes through `events`.
    pub response: &'a ChatResponse,
    /// Events appended before the held terminal events are emitted.
    pub events: &'a mut Vec<StreamEvent>,
    pub state: &'a mut Value,
}

pub enum LlmStreamEndAction {
    Continue,
    Append(Vec<StreamEvent>),
}

pub type LlmStreamEndResult = Result<LlmStreamEndAction, PluginError>;
```

Ordering:

1. The upstream body ends normally.
2. A terminal `Usage` event, if present, is applied to the accumulated `ChatResponse`.
3. A terminal `Finish` event is held, not encoded.
4. The chain runs `on_stream_end` in order. Each plugin may append events.
5. Appended events are encoded through the normal response encoder and accumulated. Terminal events in the appended list are rejected by the host.
6. The held `Usage` event, if it was withheld, is emitted, then `Finish`.
7. `on_final_response` runs read-only for cache population.
8. `on_log` runs read-only.

`response` is the accumulation before the hook chain starts; `events` also contains events appended by earlier plugins in the chain. The hook is best-effort: a plugin error or timeout is logged and ignored. It is not called when the upstream stream aborts or errors; those paths run `on_error` and do not produce a final response. This keeps the status line and headers already committed.

### D5. WASM parity and stage gating removal

The WIT stage enum gains the two missing stages:

```wit
enum stage {
    request,
    provider-request,
    response,
    stream-event,
    stream-end,
    error,
    final-response,
    log,
}
```

The host no longer filters LLM stages through the API `PluginPhase` vocabulary. Every LLM stage is dispatched; the SDK's default method returns `continue`. This removes the `response_body`/`stream-event` and `upstream_request`/`provider-request` mismatch and the silent no-op failure mode. `PluginManifest::phases` remains an API-gateway field and is ignored by the LLM runtime. If stage gating becomes a performance problem, a later contract can add an explicit stage bitmask; it is not worth preserving the stale vocabulary in v7.

The WIT plugin interface gains the sensitive-content marker as a function, not a manifest flag, so native and WASM use the same runtime contract:

```wit
interface plugin-api {
    configure: func(config-json: string) -> result<_, string>;
    restores-sensitive-content: func() -> bool;
    on-stage: func(stage: stage, invocation-json: string) -> result<outcome, string>;
}
```

The WASM adapter calls `restores-sensitive-content` once at configure and caches the result. Native plugins keep `LlmPlugin::restores_sensitive_content`.

The WIT outcome gains a source field for request-stage responses:

```wit
record outcome {
    action: string, // continue | respond | reject | replace | ignore | append
    source: option<string>, // cache | synthetic | termination
    payload-json: option<string>,
    state-json: string,
    error-json: option<string>,
    headers: list<tuple<string, string>>,
}
```

- `response` stages merge `outcome.headers`.
- `stream-end` uses `action = "append"` and `payload-json = {"events": [...]}`; `headers` is ignored.
- `final-response` and `log` require `action = "continue"`; other actions are ignored.
- `source` is required for `respond` and ignored otherwise.

### D6. Host key-value storage for third-party plugins

The LLM WASM host gains a per-plugin key-value API, backed by the same host-owned per-plugin SQLite database as the API plugin runtime (ADR 0006). It is available only when the manifest declares `Capability::KeyValue`; otherwise the host functions return an error.

```wit
interface host {
    log: func(level: log-level, message: string);
    now-unix: func() -> s64;
    kv-get: func(key: string) -> result<option<string>, string>;
    kv-put: func(key: string, value: string, ttl-seconds: option<u32>) -> result<_, string>;
    kv-delete: func(key: string) -> result<bool, string>;
    kv-incr: func(key: string, delta: s64, ttl-seconds: option<u32>) -> result<s64, string>;
}
```

- Keys and values are UTF-8 strings; values are JSON when the plugin needs structure.
- `kv-incr` is atomic and returns the new value.
- `ttl-seconds` is optional; expired entries are invisible and purged lazily.
- The host enforces `plugins.max_plugin_db_bytes` and per-value size limits.
- Network, secrets, entities, events, jobs, and admin routes remain native-only in v7. A third-party cache, rate limiter, or budget guard can be written with KV; a third-party PII engine that calls an HTTP service cannot.

### D7. `on_log` is read-only

`on_log` runs last and its mutations are already ignored by both hosts. Make the contract honest:

```rust
pub struct LlmLogContext<'a> {
    pub meta: &'a LlmRequestMeta<'a>,
    pub event: &'a LlmLogEvent<'a>,
    pub state: &'a Value,
}

async fn on_log(&self, _cx: &LlmLogContext<'_>) {}
```

`budget_guard` loses its `*cx.state = Value::Null` cleanup; the request state is dropped at the end of the request anyway, and `on_log` is the final stage. The settlement itself remains in `on_log`.

### D8. Versioning and migration

| Surface | v6 | v7 |
|---|---|---|
| `LLM_PLUGIN_API_VERSION` | `6` | `7` |
| WIT package | `octra:llm-plugin@1.0.0` | `octra:llm-plugin@2.0.0` |
| LLM manifest `api_version` | `"2"` (inconsistent) | `"7"` |
| API plugin contract | `3` | `3` (unchanged) |
| LLM trait stages | 7 | 8 (`stream-end`; `final-response` was native-only) |
| WIT stages | 6 | 8 (`stream-end`, `final-response`) |

v7 is a hard break. The runtime rejects a WASM component whose manifest `api_version` is not `"7"` and a native factory whose `api_version()` is not `7`, with an error naming the found and required versions. v6 WASM components must be rebuilt against the v7 SDK; there is no compatibility shim. The repository has accepted hard contract breaks before (ADR 0012 → 0019); maintaining two LLM ABIs would be more expensive than the migration.

The LLM manifest `api_version` is the LLM contract, not the shared API `PLUGIN_API_VERSION`. The two constants stay separate because the plugin kinds evolve independently. `PluginManifest` continues to carry one string for both; the LLM runtime validates it against `LLM_PLUGIN_API_VERSION`.

## Rust surface after v7

```rust
#[async_trait]
pub trait LlmPlugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;

    fn restores_sensitive_content(&self) -> bool {
        false
    }

    async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult;
    async fn on_provider_request(&self, cx: &mut LlmProviderRequestContext<'_>) -> LlmPluginResult;
    async fn on_response(&self, cx: &mut LlmResponseContext<'_>) -> LlmPluginResult;
    async fn on_stream_event(&self, cx: &mut LlmStreamContext<'_>) -> LlmStreamResult;

    async fn on_stream_end(&self, _cx: &mut LlmStreamEndContext<'_>) -> LlmStreamEndResult {
        Ok(LlmStreamEndAction::Continue)
    }

    async fn on_error(&self, cx: &mut LlmErrorContext<'_>) -> LlmPluginResult;

    async fn on_final_response(&self, _cx: &mut LlmFinalResponseContext<'_>) {}

    async fn on_log(&self, _cx: &LlmLogContext<'_>) {}
}
```

All methods keep default `Continue`/no-op bodies; the signatures above are abbreviated for readability. `on_request`, `on_provider_request`, `on_response`, `on_stream_event`, and `on_error` keep defaults exactly as in v6.

## Runtime changes

- `apply_response_chain` passes the response and header sink; no usage clone/writeback.
- `LlmRequestOutcome::Respond` carries `LlmResponseSource`; the data plane branches on it for `cache_status`, usage zeroing, and log fields.
- The non-streaming path applies response headers from `on_request` and `on_response` before encoding.
- The streaming path holds terminal events, applies `Usage`, runs `on_stream_end`, encodes appended events, then emits the held terminal events.
- `on_final_response` stays read-only and runs after stream-end; `on_log` is read-only and runs last.
- The metrics stage table gains `stream-end`; all eight stages use the same stage enum as WIT.
- The WASM host removes `PluginPhase` filtering for LLM and adds the KV host functions behind `Capability::KeyValue`.
- The cache-safety check combines the native trait method and the WASM `restores-sensitive-content` result.

## Built-in migration

| Plugin | v7 change |
|---|---|
| `semantic_cache` | `Respond { response, source: Cache }`; reads `response.usage`; `on_final_response` unchanged. |
| `think_tags` | Move the `Finish` flush to `on_stream_end`; keep `on_stream_event` for normal text. |
| `pii_redaction` | Move the `Finish` flush to `on_stream_end`; keep `on_error` restoration. |
| `budget_guard` | `on_log` becomes read-only; remove the state clear; settlement unchanged. |
| `request_audit` | Add `response_source` to the audit payload. |
| `rate_limit`, `model_map`, `system_prompt`, `param_override`, `intent_hint`, `prompt_guard`, `prompt_compress` | Rename `cx.headers` to `cx.response_headers`; no behavior change. |
| `llm-demo` WASM example | WIT 2.0.0, manifest `api_version: "7"`, add `final-response`/`stream-end`/sensitive/KV coverage. |

No API-gateway plugin changes; the API contract remains v3.

## Rejected alternatives

- **Keep `response.usage` and `usage` and document the precedence.** This preserves a silent data-loss trap and forces every author to learn which field wins.
- **Put the response source in `ChatResponse.extra`.** It is untyped, protocol-visible, and cannot drive logging reliably.
- **Make `on_final_response` mutable instead of adding `on_stream_end`.** It runs after encoding; mutating there cannot change bytes already sent, and making it mutable would encourage a hook that appears to work but does not.
- **Keep `PluginPhase` gating and map legacy names.** The mapping is ambiguous (`response_body` could mean stream events or buffered body), and a wrong map silently disables a plugin, which is the current failure mode.
- **Add network/secrets/entities to v7.** Those need capability policy, quotas, secret injection, and audit design. KV is the minimum that unlocks cache/rate-limit/budget; the rest is a later contract.
- **Keep v6 compatibility through an adapter.** It doubles the host and WIT surface and keeps the broken usage/Respond semantics alive. The repository already treats these contracts as hard breaks.

## Consequences

Easier:

- One usage source; response plugins cannot silently lose their write.
- Truthful cache/synthetic/termination logs and metrics.
- Cost and rate-limit headers can use the actual usage for non-streaming responses.
- Third-party plugins can flush buffered stream transforms at the correct point.
- Native and WASM expose the same eight stages and the same sensitive-content marker.
- Third-party cache/rate-limit/budget plugins are possible with KV.

Harder:

- Every LLM WASM plugin must be rebuilt; v6 artifacts stop loading.
- The streaming runtime becomes more complex: terminal events are held, usage is applied early, appended events are encoded before the held tail.
- Response headers remain unavailable at stream completion; the limitation is explicit.
- KV adds storage quotas, TTL cleanup, and a per-plugin persistence surface that must be included in backups and deletion.
- `on_log` read-only may break a plugin that relied on mutating state there; the only built-in that did is `budget_guard`, and the write was unnecessary.

## Verification

- Native unit test: a response plugin mutates `response.usage`; the billed usage and the log agree.
- Native unit test: `Respond { source: Cache }` sets `cache_status = "hit"` and zero usage; `Synthetic` and `Termination` do not.
- Native unit test: `on_response` adds a header derived from usage; it appears on the JSON response and is absent from an SSE response (headers committed earlier).
- Native unit test: `on_stream_end` appends a text delta; it is encoded before `Finish`, and the accumulated response includes it.
- Native unit test: an aborted stream never calls `on_stream_end`; `on_error` runs.
- Native unit test: `on_log` cannot mutate state (compile-time) and still observes the final event.
- WASM end-to-end test: a component implements `final-response`, `stream-end`, `restores-sensitive-content`, and `kv-incr`; cache population, flush ordering, cache disable, and counter increments are observable.
- Version test: a manifest with `api_version: "6"` is rejected with a clear error.
- Regression: the API gateway suite, the B2 internal-upstream tests, and the CORS preflight tests continue to pass.
- `cargo fmt`, `cargo clippy`, and the affected crate tests pass.

## References

- [ADR 0005 — Use native Rust and WASM plugin tiers](0005-use-native-rust-and-wasm-plugin-tiers.md)
- [ADR 0006 — Give each plugin host-owned isolated storage](0006-give-each-plugin-host-owned-isolated-storage.md)
- [ADR 0019 — Use Kong plugin phases for the API gateway](0019-use-kong-plugin-phases-for-the-api-gateway.md)
- [ADR 0020 — Route LLM traffic through the API gateway as an in-process upstream](0020-route-llm-traffic-through-the-api-gateway.md)
- `crates/llm_gateway/octra-llm-plugin-types/src/lib.rs`
- `crates/llm_gateway/octra-llm-plugin-runtime/src/lib.rs`
- `crates/llm_gateway/octra-llm-plugin-wasm/`
- `wit/llm-plugin/`
- `packages/octra-llm-plugin-sdk/`
