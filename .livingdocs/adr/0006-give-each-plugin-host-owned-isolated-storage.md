---
type: ADR
title: Give each plugin host-owned isolated storage
description: Each plugin gets its own SQLite database with host-applied migrations and a capability-gated data API.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-06T15:44:40Z
---

# 0006. Give each plugin host-owned isolated storage

## Context

Untrusted plugin code must not reach the host database, run arbitrary DDL, or read another plugin's data. Plugins still need durable state for quotas, counters, caches, and configuration. SQLite has no schema namespace that would make a shared database safe, and letting plugins issue SQL would turn every host function into an injection and authorization problem.

## Decision

We will give each plugin name its own SQLite database managed by the host. The host owns DDL: it applies the artifact's ordered SQL migrations, records the applied version, and refuses downgrades. Plugins reach state only through a capability-gated API: entity CRUD, atomic increments, and a key-value store; there is no raw SQL surface. Uninstalling keeps data unless `purge=true`, and migrations that can destroy data require explicit approval before upgrade.

Rejected alternatives:
- **A shared schema with per-plugin namespaces** — SQLite lacks schemas and the isolation would be advisory rather than physical.
- **Plugin-supplied DDL at runtime** — untrusted code would control the host's schema and migration state.
- **A raw SQL host function** — impossible to authorize safely and impossible to keep portable across plugin databases.
- **One database per plugin instance** — fragments data and multiplies files for no isolation benefit over one database per plugin.

## Consequences

**Easier / gained:**
- Plugin data is physically isolated and can be backed up or purged independently.
- The host can audit, cap, and validate every state operation.
- Migrations remain explicit SQL files owned by the artifact author and applied by the host.

**Harder / accepted trade-offs:**
- The runtime data API is intentionally narrower than SQL; complex queries need host-side or client-side composition.
- The host must implement and test entity, increment, and KV semantics.
- Each plugin adds a database file and a migration lifecycle to manage.

**Follow-ups:**
- Provide export and inspection tooling for plugin-owned databases as the ecosystem grows.

## Verification

**Implementation impact:** `crates/octra-plugin-storage/`, `crates/api_gateway/octra-api-plugin-wasm/src/`, `crates/api_gateway/octra-api-plugin-wasm/wit/plugin.wit`, plugin manifests with `entities` and `migrations`.

**Verification criteria:**
- A plugin cannot open the host database or issue DDL; only host functions are exposed.
- Installing, upgrading, and uninstalling preserve or purge plugin data according to the documented rules.
- Plugin storage tests cover migrations, entity CRUD, atomic increments, KV operations, and size limits.

# References

[1] Octra repository. Available at: https://github.com/luozhouyang/octra. Accessed on: 2026-11-06.
