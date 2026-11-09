---
type: ADR
title: Model the API gateway on Kong
description: Adopt Kong's service, route, upstream, and target model together with Kong-style route priority, load balancing, health checks, and Admin API shape, without Kong's Nginx or Lua runtime.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-07T12:40:00Z
---

# 0011. Model the API gateway on Kong

## Context

Octra's API gateway is the general-purpose HTTP data plane: it proxies arbitrary APIs, while the LLM and MCP gateways own their protocol-specific paths. The reference model is Kong Gateway, because its Service / Route / Upstream / Target vocabulary and plugin scopes are familiar to operators and map cleanly onto a local reverse proxy.

Kong's model has four properties Octra needs:

- **Routing.** Routes match on protocol, host, path, method, and headers; Kong orders matching routes by priority and uses the highest-priority match.
- **Upstreams.** A Service points at either a direct URL or an Upstream virtual host; an Upstream owns Targets, weights, algorithms, and health checks.
- **Load balancing and health.** Kong supports round-robin, consistent hashing, least-connections, latency, and sticky sessions, with active and passive health checks.
- **Admin API shape.** Kong exposes CRUD endpoints with a `{data, next, offset}` list envelope and entity foreign keys.

Octra is single-node, runs on Tokio/Axum/reqwest, and must not depend on Nginx, Lua, or a Kong control plane. The code already uses Kong's entity names and exposes a Kong-shaped Admin API under `/admin`; this record makes that decision explicit and records the boundaries.

## Decision

We will model the API gateway on Kong's concepts and keep the implementation in Rust:

- Persist `services`, `routes`, `upstreams`, `targets`, `consumers`, `consumer_groups`, credentials, ACLs, and plugin instances with Kong-compatible field names where practical.
- Match routes on protocol, host, path (prefix or regex), method, and headers. Order matches by `regex_priority` descending, then longest path descending, then route id ascending.
- Resolve a Service to a direct target or to an Upstream; select healthy Targets with round-robin, weighted round-robin, consistent hashing, least-connections, or latency EWMA. Consistent hashing follows Kong's wheel and supports `hash_on` / `hash_fallback` inputs.
- Apply passive and active health checks from the Upstream `healthchecks` document. Passive checks observe live traffic; active checks probe Targets on their configured intervals.
- Retry transport failures only for idempotent methods, bounded by the Service retry count and the total attempt budget. A non-idempotent request is attempted once.
- Expose a Kong-shaped Admin API at `/admin` (`/admin/services`, `/admin/routes`, `/admin/upstreams`, `/admin/targets`, `/admin/consumers`, `/admin/consumer_groups`, `/admin/plugins`, auth credentials, ACLs, schemas, status, metrics, logs, audit logs, reload) with `{data, next, offset}` list envelopes. The daemon's Bearer token and audit middleware remain in front of it.
- Keep the public data listener and control listener separate; the API gateway is the fallback handler on the data listener, behind the LLM and MCP path routers.

**Not adopted:**

- Kong's Nginx data path, Lua plugin ABI, declarative `kong.conf` runtime, and full Admin API compatibility. Octra's Admin API is shape-compatible, not a drop-in Kong replacement.
- Kong Enterprise features (workspaces, RBAC, Dev Portal, Vitals) and multi-node control planes.
- Sticky sessions for cookie-hashed upstreams: the first successful response sets the configured cookie to the selected target id, and subsequent requests with that cookie pin to it until the target becomes ineligible.

## Rejected alternatives

- **Nginx/Envoy sidecar** — adds an external process and configuration language, contradicts the single local daemon, and cannot be driven atomically from the SQLite control plane.
- **A hand-written router with no Kong vocabulary** — would force every operator and fixture to learn a new entity model without a product benefit.
- **Full Kong Admin API compatibility** — the compatibility surface is large, Kong's auth and workspace semantics do not match a local single-user daemon, and compatibility would constrain schema evolution.
- **Pingora or a purpose-built proxy engine** — recorded in ADR 0001; cross-platform TLS and the one-runtime constraint favored Axum and reqwest.

## Consequences

**Easier / gained:**

- Operators can reuse Kong concepts and existing mental models for services, routes, upstreams, and targets.
- The same entity model supports the desktop console, the CLI, and test fixtures.
- Load balancing and health checking are ordinary Rust code with deterministic unit and integration tests.

**Harder / accepted trade-offs:**

- The API is only Kong-shaped; operators cannot paste a full Kong declarative config and expect every field to be honored.
- `least-connections` and `latency` use process-local in-flight and latency-EWMA signals. The counters reset on a runtime reload, and the in-flight guard covers response headers rather than a fully streamed body.
- Active health checks add background traffic and must be documented as opt-in.
- The Admin API is a second public surface that needs its own compatibility tests.

**Follow-ups:**

- Add a benchmark for proxy overhead (the PRD open question) before publishing performance claims.
- Add UI and CLI CRUD for services, routes, upstreams, and targets; today they list and inspect but do not mutate these entities.
- Add a conformance fixture set for the Kong-shaped Admin API.

## Verification

**Implementation impact:** `crates/octra-api-core/src/lib.rs`, `crates/octra-runtime/src/matcher.rs`, `crates/octra-runtime/src/balancer.rs`, `crates/octra-api-gateway/src/data.rs`, `crates/octra-api-gateway/src/admin/`, `migrations/0009_gateway_entities.sql`, `migrations/0011_consumer_groups.sql`, `migrations/0012_service_tags.sql`, `migrations/0013_upstream_hash_fields.sql`.

**Verification criteria:**

- A request matching a route reaches the correct service and target, including host, path, method, header, and regex matches.
- Route ordering follows `regex_priority`, longest path, then id.
- Round-robin, weighted round-robin, and consistent hashing distribute requests as configured.
- Passive health checks remove an unhealthy target; active probes restore it only when configured.
- Non-idempotent methods are not retried.
- `/admin` CRUD and list envelopes match the documented shape.
- `cargo test -p octra-api-gateway -p octra-runtime` passes.

# References

[1] Kong Gateway, "Proxying with Kong Gateway." https://developer.konghq.com/gateway/traffic-control/proxying/
[2] Kong Gateway, "Routes." https://developer.konghq.com/gateway/entities/route/
[3] Kong Gateway, "Gateway Services." https://developer.konghq.com/gateway/entities/service/
[4] Kong Gateway, "Upstreams." https://developer.konghq.com/gateway/entities/upstream/
[5] Kong Gateway, "Targets." https://developer.konghq.com/gateway/entities/target/
[6] Kong Gateway, "Load balancing reference." https://developer.konghq.com/gateway/traffic-control/load-balancing-reference/
[7] Kong Gateway, "Health checks and circuit breakers." https://developer.konghq.com/gateway/traffic-control/health-checks-circuit-breakers/
[8] Octra repository, `crates/octra-api-gateway`, `crates/octra-runtime`, and `migrations/`. https://github.com/luozhouyang/octra
