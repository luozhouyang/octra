---
type: ADR
title: Model the LLM gateway on magpie
description: Adopt magpie's local multi-protocol gateway, routing-group strategies, provider-account failover, privacy masking, and usage/cost model, while narrowing the strategy set and excluding magpie's cloud and subscription-discovery features.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-07T12:40:00Z
---

# 0013. Model the LLM gateway on magpie

## Context

Octra's LLM gateway is a local endpoint for coding agents and LLM clients. The reference is yetone/magpie: a local gateway that speaks OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, and Gemini, translates between them, routes through model groups, fails over across provider keys and accounts, masks secrets and personal data before egress, and records usage and cost.

magpie's routing model is broader than Octra needs today. magpie exposes `smart`, `order`, `rotate`, `usage`, `pace`, `manual`, `weight`, `free`, `fast`, `drain`, `cheap`, and `expiring` strategies; it also discovers signed-in subscription accounts, supports remote sharing, and runs a multi-platform app. Octra's scope is a single-user local daemon with explicit provider configuration, so blindly copying the whole surface would add state and failure modes that the product has not committed to.

The code already implements a substantial subset: four ingress protocols, a canonical IR, virtual models and routing groups, provider accounts with static and OAuth credentials, quota/cooldown/pacing, virtual-key and consumer policies, usage/cost accounting, and PII redaction.

## Decision

We will model the LLM gateway on magpie's local-gateway shape and keep the implementation in the existing Rust gateway:

- Serve one local endpoint with OpenAI Chat Completions (`/v1/chat/completions`), OpenAI Responses (`/v1/responses`), Anthropic Messages (`/v1/messages`, `/v1/messages/count_tokens`), and Gemini (`/v1beta/models`, `/v1beta/models/{model}:{action}`). Decode every protocol into the canonical IR from ADR 0004 and encode the response or stream back into the ingress protocol.
- Expose virtual models (`ai_models`) that resolve to a routing group. A group has priority rounds and weighted targets; a request walks the rounds and then the fallback target, bounded by the attempt budget.
- Support the routing strategies that are implemented: `priority` (default rounds), `order`, `manual`, `weight`, `rotate` / `round_robin`, `smart`, `fast`, `cheap`, `free`, `drain`, `expiring`, `usage`, `quota`, `pace`, and `intent`. Ranking uses account quota, quota reset/credential expiry, observed latency, failure EWMA, recent usage, price, and the client intent header. Octra also adds a native `semantic` strategy that ranks AI-native targets by embedding cosine when configured, otherwise local lexical similarity, against `semantic_description`; it is not a magpie strategy.
- Support provider accounts with static `env:` / `file:` secret references and OAuth credentials. The account pool tracks quota, cooldown, pacing, latency EWMA, and failure EWMA; a 429 or quota error puts an account on cooldown and moves to the next candidate.
- Enforce consumer policies (`allowed_models`, `allowed_providers`) and virtual-key permissions (`allowed_models`, `allowed_providers`, `allowed_accounts`, and hourly/daily/weekly/monthly token or cost limits). Virtual-key limits are read from persisted usage so they survive restarts.
- Record usage asynchronously with protocol, status, latency, account, model, finish reason, cache/reasoning tokens, and computed cost. Roll up hourly usage and prune it through the retention policy.
- Run `llm-pii-redaction` as an opt-in LLM plugin (ADR 0016) with reversible or irreversible masking, local or external engines, and fail-closed behavior when configured.
- Maintain agent configuration adapters for Claude Code, Codex, and OpenCode (ADR 0008) so an agent can be pointed at the gateway with a virtual key.

**Not adopted in v0.1:**

- magpie's signed-in subscription discovery, menu-bar app, remote sharing, Docker/NAS mode, library of prompts/skills, and Bun/JavaScript plugin runtime.
- magpie's intent classifier. Octra accepts a client-supplied `x-octra-intent` header and ranks on it; it does not make a separate classifier call.
- Cloud-side PII detection or any PII value leaving the machine.

## Rejected alternatives

- **Pairwise protocol conversion** — the O(N²) adapter problem recorded in ADR 0004.
- **Transparent passthrough only** — cannot route across protocols, apply LLM plugins, or account for usage uniformly.
- **Copying magpie's full routing matrix** — adds strategies without the account/subscription model that gives them meaning, and expands the test surface before the core v0.1 path is accepted.
- **Storing provider keys in agent configs** — violates ADR 0007.
- **Building a cloud control plane** — contradicts the local-first constitution.

## Consequences

**Easier / gained:**

- One local endpoint works with OpenAI-, Anthropic-, Gemini-, and Responses-shaped clients.
- Routing, failover, quota, and usage are protocol-independent because they operate on the IR.
- Virtual keys can be rotated without rewriting agent configs.
- PII masking and cost accounting are part of the same request path rather than a separate proxy.

**Harder / accepted trade-offs:**

- The IR must evolve to represent provider features without leaking vendor details.
- Provider accounts, OAuth refresh, cooldowns, pacing, and quota counters add state that must be tested across reloads and restarts.
- Streaming usage is recorded when the stream closes, so a dropped stream can undercount tokens.

**Follow-ups:**

- If the AI-native entity model is adopted, see ADR 0018 for the provider/model/target/policy migration path; this record remains the data-plane and magpie-parity decision.
- Add a postgres-free concurrency test for quota and cooldown counters under parallel requests.
- Encrypt or keychain-back OAuth tokens (ADR 0007 follow-up).

## Verification

**Implementation impact:** `crates/llm_gateway/octra-llm-core/`, `crates/llm_gateway/octra-llm-protocol/`, `crates/llm_gateway/octra-llm-gateway/src/routing.rs`, `crates/llm_gateway/octra-llm-gateway/src/accounts.rs`, `crates/llm_gateway/octra-llm-gateway/src/credentials/`, `crates/llm_gateway/octra-llm-gateway/src/vk_policy.rs`, `crates/llm_gateway/octra-llm-gateway/src/data_plane/`, `crates/llm_gateway/octra-llm-gateway/src/usage.rs`, `crates/llm_gateway/octra-llm-gateway/src/cost.rs`, `crates/llm_gateway/octra-llm-plugin-builtin/src/redaction.rs`, `migrations/0003_llm_gateway.sql`, `migrations/0007_llm_accounts.sql`, `migrations/0014_usage_analytics.sql`, `migrations/0016_pii_engines.sql`, `migrations/0017_oauth_credentials.sql`, `migrations/0018_llm_virtual_key_family.sql`, `migrations/0019_llm_counters.sql`.

**Verification criteria:**

- Each ingress protocol decodes to the IR, routes through a virtual model, and encodes the response back in the ingress protocol.
- A routing group with priority rounds and weighted targets tries the expected order and falls back on a retryable upstream failure.
- A 429 or quota error cools down the account and selects the next candidate.
- Virtual-key token and cost limits reject before the upstream call.
- Consumer policies reject a forbidden model or provider before the upstream call.
- Usage and cost rows are written asynchronously and survive a restart.
- PII masking is reversible in responses and streams and fails closed when the engine is unavailable and fail-closed is configured.
- `cargo test -p octra-llm-gateway -p octra-llm-protocol -p octra-llm-plugin-builtin` passes.

# References

[1] yetone, "magpie." https://github.com/yetone/magpie
[2] magpie, "Gateway routing." https://github.com/yetone/magpie/blob/main/docs/subsystems/gateway-routing.md
[3] magpie, "Reference." https://github.com/yetone/magpie/blob/main/docs/reference.md
[4] Octra ADR 0004, "Normalize LLM protocols through a canonical IR." `0004-normalize-llm-protocols-through-a-canonical-ir.md`
[5] Octra ADR 0007, "Keep secrets out of storage and agents." `0007-keep-secrets-out-of-storage-and-agents.md`
[6] Octra repository, `crates/llm_gateway/octra-llm-*`. https://github.com/luozhouyang/octra
