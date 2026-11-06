---
type: ADR
title: Use SQLite as the only persistence layer
description: One SQLite database with SQLx migrations, WAL, rollups, and retention; no external database or queue.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-06T15:44:39Z
---

# 0002. Use SQLite as the only persistence layer

## Context

Octra is local-first and single-node, so an installation must work without a database server, a cache server, or an operator. The gateway still needs transactional configuration changes, append-only request and audit logs, LLM usage accounting, and schema evolution across releases. A multi-process split would add operational cost and complicate backup and query semantics for a single-user product.

## Decision

We will use SQLite as the only persistence engine. The control plane uses one database at `$OCTRA_HOME/octra.db`, accessed through SQLx with WAL, foreign keys, and a busy timeout. Schema changes ship as ordered SQLx migrations. Telemetry stays in the same database, with hourly usage rollups and a retention worker keeping it bounded. Plugin-owned state lives in separate per-plugin SQLite files under the plugin storage root (ADR 0006), not in the control database. No PostgreSQL, MySQL, Redis, Elasticsearch, or separate telemetry database is required.

Rejected alternatives:
- **PostgreSQL or MySQL** — requires a server and network configuration that contradicts local-first installation.
- **Redis or another external queue** — adds a second runtime dependency; plugin events use a SQLite outbox instead.
- **A separate telemetry database** — cross-file queries, two backup units, and migration coordination outweigh the write-isolation benefit at this scale.
- **In-memory state only** — loses configuration, audit history, and usage accounting on restart.

## Consequences

**Easier / gained:**
- Install, backup, and restore need no database server; the control database is one file (plus WAL/SHM sidecars).
- Migrations, transactions, and tests run against the same engine as production.
- Query and reporting code can join configuration and telemetry without a second system.

**Harder / accepted trade-offs:**
- SQLite allows one writer at a time; high-volume writes must be batched and kept off the request hot path.
- There is no horizontal scaling path inside the current architecture.
- Large telemetry tables require retention and rollups rather than partitioning.

**Follow-ups:**
- Measure write contention under concurrent LLM and API traffic before introducing any additional store.

## Verification

**Implementation impact:** `crates/octra-storage/src/lib.rs`, `crates/octra-storage/src/maintenance.rs`, `migrations/`, `configs/default.toml`.

**Verification criteria:**
- A fresh daemon creates `octra.db` and applies every migration from an empty database.
- Storage tests cover migrations, retention pruning, usage rollups, and private database permissions.
- The workspace has no PostgreSQL, MySQL, or Redis client dependency.

# References

[1] Octra repository. Available at: https://github.com/luozhouyang/octra. Accessed on: 2026-11-06.
