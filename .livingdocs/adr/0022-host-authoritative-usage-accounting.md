---
type: ADR
title: Host-authoritative usage accounting
description: Capture billing usage before the LLM plugin chain, keep usage and cost host-owned, and expose the computed cost to the log stage without adding new hooks.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-10T00:20:00Z
---

# 0022. Host-authoritative usage accounting

## Context

Contract v7 lets a plugin mutate `ChatResponse::usage` in `on_response` and replace `StreamEvent::Usage` events in `on_stream_event`. The data plane then used those post-plugin values for billing, account quota, virtual-key limits, and cost. A plugin could therefore change the bill or the client-visible usage, intentionally or by accident.

Two related gaps surfaced:

- `LlmLogEvent` had no host-computed cost, so a plugin could not audit or settle cost without duplicating pricing logic.
- The suite had no test proving that a plugin cannot change billing usage.

The host already owns the ledger: `UsageTracker` records tokens, `cost::compute` applies pricing, `ai_usage` persists the record, `account.consume` updates quota, and virtual-key limits read the same aggregates. The plugin layer should not be able to alter that path.

## Decision

### D1. Billing usage is captured before the plugin chain

- **Non-streaming:** after the upstream response is decoded and before `on_response`, the host clones `response.usage` into `billing_usage`.
- **Streaming:** the host scans the raw decoded events before `on_stream_events` and stores the last `StreamEvent::Usage` in `billing_usage`.

`billing_usage` is the only input to billing, quota, virtual-key accounting, and cost.

### D2. Plugin mutations do not change the bill or the client-visible usage

- Non-streaming: after the response chain, the host restores `response.usage = billing_usage` before protocol encoding and before `on_final_response`.
- Streaming: the host filters every post-plugin `StreamEvent::Usage` and emits the captured `billing_usage` as the authoritative `Usage` event before the terminal event.
- A plugin can still read usage and may store its own view in `state`, but its mutations cannot change accounting or the usage the client receives.

### D3. Cost is host-computed and exposed at the log stage

`LlmLogEvent` gains an additive field:

```rust
pub cost_micros: Option<u64>,
```

It is computed from `billing_usage` and the routed target's pricing:

- non-streaming: in the attempt success path;
- streaming: in `record_stream`;
- cache short-circuit: `Some(0)`;
- synthetic/termination short-circuit: `None` unless pricing is available;
- errors: `None`.

`request_audit` includes `cost_micros` in its payload. The WIT log payload includes it additively.

### D4. No new hooks and no ABI change

- No `on_response_headers`, no provider-stage header sink, no `HeaderTiming`, no buffered stream mode.
- The eight v7 stages remain: `on_request`, `on_provider_request`, `on_response`, `on_stream_event`, `on_stream_end`, `on_error`, `on_final_response`, `on_log`.
- `LLM_PLUGIN_API_VERSION` remains `7`; WIT remains `octra:llm-plugin@2.0.0`.
- `LlmLogEvent.cost_micros` is an additive JSON field for WASM components. Native built-ins are rebuilt in-tree. No hook signature, context struct, or WIT world changes.

### D5. A disconnect still produces exactly one log event

A stream response body owns a drop guard. If the client disconnects before the stream reaches its normal finish or failure path, the guard:

- records the billing usage and a `499 client_disconnected` usage row;
- runs the `on_log` chain once with that event;
- does not run `on_final_response` (there is no complete response) and does not emit an in-band error.

`on_log` is therefore guaranteed exactly once for every stream, including disconnects. This is a runtime guarantee, not a new hook.

Streaming usage/cost is not exposed as an HTTP header. Exact HTTP headers cannot be sent after the stream body starts, and buffering the stream was rejected as an unacceptable latency cost. Clients get protocol-native usage where the protocol supports it, and the server ledger carries the authoritative cost keyed by request id.

## Consequences

Easier:

- Billing, quota, and virtual-key limits cannot be changed by a plugin.
- One source of truth for usage and cost; plugins do not duplicate pricing.
- Cost is available to audit/settlement plugins at the log stage.
- The v7 hook lifecycle remains stable; no new stages, no buffered path.

Harder:

- A plugin that intentionally rewrites client-visible usage can no longer do so. It must express display changes through another field or its own state.
- `LlmLogEvent` gains a field; native plugin source must be rebuilt. WASM components that ignore the field continue to work.
- Streaming exact cost is not an HTTP header. Clients that require it must use protocol usage or query the usage ledger.

## Verification

- A plugin sets `response.usage` to 999; the non-streaming response and `ai_usage` still report the upstream 11/7.
- A plugin replaces a streaming `Usage` event with 999; the SSE body and `ai_usage` still report the upstream 3/4.
- `on_log` receives the billing usage and a positive `cost_micros` when pricing is configured.
- Cache log events report `cost_micros = Some(0)`.
- Dropping a streaming response without reading the body records a `499 client_disconnected` usage row and runs `on_log` once.
- The full workspace test suite, the WASM end-to-end test, and the daemon lifecycle test pass.

## References

- [ADR 0021 — LLM plugin contract v7](0021-llm-plugin-contract-v7.md)
- `crates/llm_gateway/octra-llm-gateway/src/data_plane/mod.rs`
- `crates/llm_gateway/octra-llm-gateway/src/data_plane/stream.rs`
- `crates/llm_gateway/octra-llm-gateway/src/usage.rs`
- `crates/llm_gateway/octra-llm-gateway/src/cost.rs`
- `crates/llm_gateway/octra-llm-plugin-types/src/lib.rs`
