---
type: ADR
title: Compile runtime snapshots off the hot path
description: Compile control-plane configuration into immutable API and LLM snapshots, cache MCP server connections, and swap or invalidate them on mutation so requests do not query SQLite for routing decisions.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-07T12:40:00Z
---

# 0017. Compile runtime snapshots off the hot path

## Context

Octra's data plane is latency-sensitive and must keep serving while the control plane mutates configuration. Querying SQLite for services, routes, upstreams, providers, models, routing groups, policies, virtual keys, and plugin chains on every request would add parsing, locking, and database contention to the hot path. It would also make a request observe a partially applied configuration if a mutation spans several tables.

The API gateway already compiles services, routes, upstreams, targets, consumers, credentials, plugins, and ACLs into a runtime snapshot. The LLM gateway compiles providers, models, routing plans, virtual-key policies, consumer policies, accounts, and plugin chains. The MCP gateway caches live server connections and invalidates them on reload. The daemon already has reload hooks and a health-check loop.

## Decision

We will keep request routing and authorization on immutable in-memory snapshots compiled from SQLite:

- **API snapshot.** `GatewayRuntime::bootstrap` loads services, routes, upstreams, targets, consumers, credentials, plugins, and ACLs, compiles the route matcher, target pools, health state, and auth maps, and stores the result behind an `Arc`. `reload()` builds a new snapshot and swaps it atomically. In-flight requests keep the snapshot they started with.
- **LLM snapshot.** `LlmRuntime::bootstrap` loads providers, models, routing groups, routes, virtual keys, policies, accounts, and plugin configurations. `RoutePlan` compiles priority rounds and weighted cycles; the account pool keeps live quota/cooldown/pacing state across snapshot swaps. `reload()` rebuilds the snapshot and preserves live counters.
- **MCP runtime.** Server definitions are read for discovery and control operations; live upstream clients are cached by the full server definition and invalidated on reload or definition change. Tool calls use the cached connection, not a per-request database query.
- **Mutations.** Control handlers write to SQLite, then reload the affected runtime before returning. A reload failure is logged; the database remains the source of truth and the next request or reload retries.
- **Plugin chains.** API and LLM plugin chains are compiled from configured instances once per mutation, not per request. Consumer-scoped API instances are resolved in the post-auth pass.
- **Hot path.** The API data plane never queries SQLite for routing or authorization. The LLM data plane resolves routing and authorization from the snapshot, but it reads persisted usage for virtual-key limit enforcement and writes usage asynchronously. MCP stores session and invocation rows, but its tool-call routing uses the cached server definition.

**Not adopted:**

- Querying SQLite per request for routing or policy decisions.
- A global configuration lock held across a request.
- Restarting the daemon to apply configuration changes.
- Distributed cache invalidation; Octra is single-node.

## Rejected alternatives

- **Database per request** — adds latency and contention and makes configuration consistency dependent on transaction timing.
- **A RwLock around live configuration** — keeps requests correct but serializes readers with writers and invites lock contention on the hot path.
- **Restart on change** — interrupts traffic and makes the desktop/CLI experience unusable.
- **A separate configuration service** — contradicts the single-daemon, SQLite-only boundary.

## Consequences

**Easier / gained:**

- Request routing and policy checks are in-memory map lookups and compiled matchers.
- A mutation is atomic from a request's point of view: it sees either the old snapshot or the new one.
- In-flight requests are not disturbed by a reload.
- The API and LLM runtimes can be tested without a live database in the hot path.

**Harder / accepted trade-offs:**

- A reload compiles the whole snapshot; large configurations make the control mutation slower.
- Live counters (account quota, cooldown, health) must be preserved across snapshot swaps; losing them would reset governance state.
- A reload failure can leave the database and the runtime temporarily out of sync; the UI must surface reload errors.
- MCP connection invalidation can drop a connection while a call is in flight; the call fails and the retry path reconnects.

**Follow-ups:**

- Benchmark bootstrap and reload time with a large configuration and record the budget.
- Add a reload-generation or version field to health/status so clients can detect staleness.
- Add a metrics counter for reload failures and MCP connection restarts.
- Document which mutations trigger a full reload versus a scoped invalidation.

## Verification

**Implementation impact:** `crates/octra-runtime/src/lib.rs`, `crates/octra-runtime/src/matcher.rs`, `crates/octra-runtime/src/balancer.rs`, `crates/llm_gateway/octra-llm-gateway/src/runtime.rs`, `crates/llm_gateway/octra-llm-gateway/src/routing.rs`, `crates/llm_gateway/octra-llm-gateway/src/accounts.rs`, `crates/octra-mcp-gateway/src/servers.rs`, `crates/octra-daemon/src/main.rs`.

**Verification criteria:**

- A route mutation is visible to the next API request without a restart, and an in-flight request keeps the old route.
- A provider/route mutation is visible to the next LLM request without losing account quota or cooldown state.
- An MCP server definition change opens a fresh connection; an unchanged definition reuses the cached one.
- A failed reload returns an error from the control mutation and leaves the previous snapshot serving.
- `cargo test -p octra-runtime -p octra-llm-gateway -p octra-mcp-gateway` passes.

# References

[1] Octra ADR 0002, "Use SQLite as the only persistence layer." `0002-use-sqlite-as-the-only-persistence-layer.md`
[2] Octra ADR 0003, "Run the gateway as an independent daemon." `0003-run-the-gateway-as-an-independent-daemon.md`
[3] Octra repository, `crates/octra-runtime`, `crates/llm_gateway/octra-llm-gateway/src/runtime.rs`, `crates/octra-mcp-gateway/src/servers.rs`. https://github.com/luozhouyang/octra
