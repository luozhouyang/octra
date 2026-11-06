---
type: ADR
title: Deliver plugin events through a durable outbox
description: Persist subscribed events, deliver them per subscriber with retries, and reconcile scheduled jobs on a tick.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-06T15:44:40Z
---

# 0009. Deliver plugin events through a durable outbox

## Context

Plugins need to react to gateway events and run scheduled work, but the request path must not block on a plugin, and a process crash between publishing and delivery must not silently lose an event. There is no external queue in a local-first installation, and in-memory delivery cannot provide retries or survive a restart.

## Decision

We will persist every subscribed event in SQLite before dispatch, create one delivery row per matching subscriber, and retry failed deliveries with exponential backoff up to a bounded attempt count. Delivery is at-least-once; subscribers must tolerate duplicates. Scheduled jobs are declared by enabled plugins, reconciled into `plugin_jobs` on every tick, and executed with the plugin's timeout. Terminal events are pruned by the retention policy, and the publisher skips the write entirely when no enabled subscriber matches the topic.

Rejected alternatives:
- **Fire-and-forget in-process events** — a slow or crashed subscriber loses the event and can block the publisher.
- **An external queue such as Redis or NATS** — adds a second runtime dependency and contradicts the SQLite-only persistence decision.
- **Synchronous delivery inside the request path** — couples client latency and failure to plugin behavior.

## Consequences

**Easier / gained:**
- Events survive a daemon restart and are retried without operator intervention.
- Plugin failures are isolated from the request path and recorded per delivery.
- No external broker is required for local or single-node deployment.

**Harder / accepted trade-offs:**
- At-least-once delivery means duplicates are possible; plugin handlers must be idempotent.
- The outbox adds database writes for topics that have subscribers, bounded by retention.
- Delivery is eventually consistent rather than immediate.

**Follow-ups:**
- Expose delivery lag and retry counts in the plugin operational view as the event surface grows.

## Verification

**Implementation impact:** `crates/octra-plugin-events/`, `crates/octra-storage/src/events.rs`, `migrations/0021_events.sql`, `migrations/0022_event_retention_index.sql`, `crates/octra-api-plugin-types/src/services.rs`.

**Verification criteria:**
- An event published with a matching subscriber survives a dispatcher restart and is delivered after retry.
- A failing subscriber records attempts, backoff, and a terminal failure without blocking other deliveries.
- Events with no matching subscriber do not create rows.
- Retention deletes only completed or failed events and leaves pending events untouched.

# References

[1] Octra repository. Available at: https://github.com/luozhouyang/octra. Accessed on: 2026-11-06.
