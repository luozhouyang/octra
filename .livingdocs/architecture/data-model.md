---
type: Architecture View
title: Data Model
description: The core persisted entities and foreign-key relationships in the SQLite database.
kind: data-model
timestamp: 2026-11-06T15:46:09Z
---

# Data Model

The persisted entity map for `$OCTRA_HOME/octra.db`; `migrations/` is the authority for columns and constraints, and this view names the relationships a reader needs before changing the schema.

```mermaid
erDiagram
    SERVICES ||--o{ ROUTES : exposes
    SERVICES ||--o{ UPSTREAMS : routes_to
    SERVICES o|--o{ PLUGIN_INSTANCES : scopes
    ROUTES o|--o{ PLUGIN_INSTANCES : scopes
    CONSUMERS o|--o{ PLUGIN_INSTANCES : scopes
    UPSTREAMS ||--o{ TARGETS : contains
    CONSUMERS ||--o{ CREDENTIALS : owns
    CONSUMERS o|--o{ POLICIES : scopes
    CONSUMERS }o--o{ CONSUMER_GROUPS : joins
    CONSUMERS o|--o{ LLM_VIRTUAL_KEYS : owns
    LLM_PROVIDERS ||--o{ LLM_MODELS : offers
    LLM_PROVIDERS ||--o{ LLM_PROVIDER_ACCOUNTS : holds
    LLM_PROVIDERS ||--o{ LLM_ROUTING_TARGETS : serves
    LLM_ROUTING_GROUPS ||--o{ LLM_ROUTING_TARGETS : selects
    LLM_ROUTING_GROUPS o|--o{ LLM_ROUTES : aliases
    LLM_VIRTUAL_KEYS ||--o{ LLM_USAGE : accounts
    MCP_SERVERS ||--o{ MCP_TOOLS : exposes
    MCP_SERVERS ||--o{ MCP_SESSIONS : runs
    MCP_TOOLS ||--o{ MCP_TOOL_ACLS : grants
    CONSUMERS ||--o{ MCP_TOOL_ACLS : receives
    AGENTS ||--o{ AGENT_PROFILES : configures
    AGENTS ||--o{ AGENT_CONFIG_BACKUPS : backs_up
    PLUGINS ||--o{ PLUGIN_CONFIGS : configures
    EVENTS ||--o{ EVENT_DELIVERIES : delivers
```

**Three planes.** Identity and governance (`consumers`, `credentials`, `consumer_groups`, `policies`, `llm_virtual_keys`) are separate from routing (`services`/`routes`/`upstreams`/`targets`, `llm_routes`/`llm_routing_groups`/`llm_routing_targets`/`llm_providers`/`llm_models`) and from accounting (`llm_usage`, `request_logs`, `audit_logs`, `events`/`event_deliveries`). A foreign key between planes is rare by design; they meet in accounting rows.

**Nullable scope.** `policies.consumer_id` is null for a global policy. `plugin_instances.service_id`, `route_id`, and `consumer_id` are nullable scope selectors. `llm_routes.routing_group_id` is nullable, so a route can exist before a group is attached.

**Soft accounting references.** `llm_usage` records `api_key_id`, `virtual_model_id`, `provider_id`, `account_id`, and `model_id` as historical labels rather than enforced foreign keys, so deleting configuration does not erase usage history.

**Cascades.** Configuration children cascade with their parent (`upstreams` -> `targets`, `plugins` -> `plugin_configs`, `mcp_servers` -> `mcp_tools`, `events` -> `event_deliveries`). Agent config backups are retained independently of the `agents` table and capped at 20 per agent; audit logs are bounded by the retention worker. Neither is removed by a configuration parent cascade.

**Standalone tables.** `request_logs`, `audit_logs`, `mcp_invocations`, `plugin_jobs`, and `settings` have no enforcement foreign keys in the current schema; they are append-only or operational records.

## Drift instrument

Drift caught by: SQLx migrations are the only DDL source, and `cargo test -p octra-storage` applies every migration to a fresh database; schema changes must update this view in the same change.
