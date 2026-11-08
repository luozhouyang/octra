---
type: ADR
title: Redact PII before provider egress
description: Run opt-in, reversible or irreversible PII masking in the LLM request path, using local rules and optional local or loopback engines, and fail closed when the configured engine is unavailable.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-07T12:40:00Z
---

# 0016. Redact PII before provider egress

## Context

Coding-agent prompts routinely include `.env` files, logs, configuration, and source code. Those prompts may contain API keys, tokens, private keys, emails, card numbers, IDs, addresses, and other personal data. magpie's privacy masking is the reference: values are replaced with placeholders before a request leaves the machine and restored in the reply, including streamed text and tool-call arguments. Masking is off by default and can be configured to mask, warn, or block.

Octra already has the pieces: a built-in `llm-pii-redaction` plugin, a host-managed PII engine registry, local model download/status management, and a settings page for the local model. The missing piece is a recorded boundary: when masking runs, what it covers, what is persisted, and what happens when the engine is unavailable.

## Decision

We will make PII redaction an opt-in LLM gateway capability with a fail-closed default when an engine is configured:

- `llm-pii-redaction` runs in the LLM request, provider-request, response, and stream stages. It can mask the system prompt, assistant messages, tool results, and (when configured) tool definitions.
- Two modes are supported: `reversible` (placeholder-to-value map kept in memory for the request and restored in the response and stream) and `irreversible` (values stay masked).
- Detection combines deterministic rules (secrets, private keys, tokens, emails, card/ID checksums, and user-supplied patterns) with an optional local or loopback PII engine. Engine definitions live in `llm_pii_engines`; the host can spawn a managed engine or call an external loopback endpoint.
- The placeholder map is kept in memory only. The usage and audit rows record counts and kinds, not the original values or the placeholder map.
- When a detector engine is configured and unavailable, the default is to reject the request (`fail_closed: true` / `on_unavailable: reject`). An operator can explicitly choose `rules_only` to continue with deterministic rules.
- Masking is off until a plugin instance is configured. The settings page exposes the local model and its readiness; the plugin configuration remains an explicit control-plane action.
- The redaction plugin must not send PII to a cloud detector. External engines are loopback or local; a remote endpoint is not part of the v0.1 boundary.

**Not adopted:**

- Always-on masking without an operator switch.
- Cloud-side PII detection or a vendor-hosted masking service.
- Persisting the placeholder map, original values, or a reversible mapping in SQLite.
- Using PII masking as a substitute for secret references; provider secrets are still references resolved at request time (ADR 0007).

## Rejected alternatives

- **Vendor-side redaction** — the data has already left the machine, which is the boundary Octra is trying to protect.
- **Regex-only masking** — misses names, addresses, and context-dependent values; the optional local model exists for that reason.
- **Storing the mapping for replay** — turns the database into a plaintext PII store.
- **Fail-open on detector failure** — sends the request as written when the operator expected masking; the default must be fail-closed.

## Consequences

**Easier / gained:**

- Operators can keep secrets and personal data out of provider requests without changing agent configuration.
- The same masking path covers JSON responses, streaming text, and tool-call arguments.
- Local-only engines keep the privacy boundary on the machine.
- Fail-closed behavior is explicit and testable.

**Harder / accepted trade-offs:**

- A model that sees `{{API_KEY_...}}` cannot reason about the value itself; operators need a "never mask" escape hatch.
- Reversible masking requires careful handling of streaming chunks and tool-call argument fragments.
- Local model download and engine lifecycle add disk, CPU, and startup concerns.
- The local model and engine status are visible in Settings, but engine CRUD is not yet a first-class UI flow.
- Masking can produce false positives and false negatives; the operator owns the rule set.

**Follow-ups:**

- Add a control-plane CRUD surface for PII engines and a desktop editor.
- Add tests for placeholders split across stream chunks and inside tool-call arguments.
- Document the supported rule kinds and the "never mask" behavior.
- Consider a request preview that shows the masked payload before it is sent.

## Verification

**Implementation impact:** `crates/octra-llm-plugin-builtin/src/redaction.rs`, `crates/octra-pii-types/src/`, `crates/octra-llm-gateway/src/pii/`, `crates/octra-llm-gateway/src/control.rs`, `migrations/0016_pii_engines.sql`, `apps/desktop/src/app/pages/settings.tsx`.

**Verification criteria:**

- A configured reversible plugin replaces a value in the request and restores it in a non-streamed response.
- The same value is restored when the placeholder is split across two SSE chunks.
- An irreversible plugin leaves the value masked in the response.
- An unavailable engine with fail-closed configuration rejects the request before egress.
- The placeholder map is not written to SQLite or the audit log.
- `cargo test -p octra-llm-plugin-builtin -p octra-llm-gateway` passes.

# References

[1] magpie, "Privacy masking." https://github.com/yetone/magpie#privacy-masking
[2] Octra ADR 0007, "Keep secrets out of storage and agents." `0007-keep-secrets-out-of-storage-and-agents.md`
[3] Octra repository, `crates/octra-llm-plugin-builtin/src/redaction.rs` and `crates/octra-llm-gateway/src/pii/`. https://github.com/luozhouyang/octra
