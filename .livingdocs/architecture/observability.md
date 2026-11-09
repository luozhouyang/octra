---
type: Architecture View
title: Observability and Retention
description: Access logs, audit logs, LLM usage and cost, plugin metrics, JSON and Prometheus metrics, events, and the retention worker.
kind: container
timestamp: 2026-11-07T12:40:00Z
---

# Observability and Retention

Octra keeps operational history in the same SQLite database as configuration. The request path writes logs asynchronously; control mutations write audit rows; LLM usage is recorded from the canonical response; the retention worker bounds every append-only table.

```mermaid
flowchart LR
    APIData[API data plane] --> ReqLogs[(request_logs)]
    APIData --> Metrics[ApiMetrics JSON/Prometheus]
    Control[Control mutations] --> Audit[(audit_logs)]
    LLMData[LLM data plane] --> Usage[(ai_usage)]
    Usage --> Rollup[(ai_usage_rollup)]
    MCPData[MCP data plane] --> Invocations[(mcp_invocations)]
    Plugins[Plugin runtime] --> PluginMetrics[plugin metrics / latency]
    Events[Event bus] --> EventRows[(events + event_deliveries)]
    Jobs[Job scheduler] --> JobRows[(plugin_jobs)]
    Retention[Retention worker] --> ReqLogs
    Retention --> Audit
    Retention --> Usage
    Retention --> Rollup
    Retention --> Invocations
    Retention --> EventRows
```

## Surfaces

- **Request logs:** method, path, status, latency, route, service, target, consumer, error; queried at `/admin/logs`.
- **Audit logs:** actor, action, target, detail; every control mutation; queried at `/admin/audit-logs`.
- **LLM usage:** request id, virtual model, provider, model, account, API key, protocol, status, latency, input/output/cache/reasoning tokens, finish reason, cost, error. Hourly rollups feed the overview and usage pages.
- **LLM request audit:** content-free request metadata plus `cache_status` (`hit` for cached responses, otherwise null/absent). Semantic cache lookups log `hit`/`miss` with the match kind at debug level; semantic routing logs target scores at debug level; clients also see `X-Octra-Cache: hit|miss`.
- **MCP invocations:** server, tool, consumer, request id, status, duration, error.
- **Metrics:** API request counts/latency/status and upstream errors; JSON at `/admin/metrics`, Prometheus at `/admin/metrics/prometheus`; LLM plugin stage metrics at `/admin/llm/plugins/metrics`; plugin metrics at `/admin/plugin-metrics`.
- **Events and jobs:** outbox rows and delivery attempts at `/admin/events`; aggregate pending/delivered/failed events, pending/failed/retrying deliveries, oldest pending age, and total attempts at `/admin/events/stats`; job schedule and last result at `/admin/jobs`.
- **OTLP:** optional `[telemetry.otlp]` exports API and event metrics as OTLP/HTTP JSON to `{endpoint}/v1/metrics` on a configurable interval. Export failures are logged without stopping the daemon.
- **Tracing:** `octra-observability` initializes `tracing` with pretty or JSON output and an env-filter level.

## Retention

The default policy is enabled with hourly runs and a batch size of 5000:

| Data | Default retention |
|---|---|
| `ai_usage` | 90 days |
| `request_logs` | 30 days |
| `mcp_invocations` | 90 days |
| `audit_logs` | 365 days |
| `ai_usage_rollup` | 400 days |
| completed/failed `events` | 7 days |
| pending events | never pruned |

A window of `0` keeps rows forever. `llm_usage_days` must be `0` or at least 32 so a monthly virtual-key limit can see a full window.

## Known gaps

- The API metrics store is in-memory; a daemon restart resets counters.
- OTLP export is HTTP/JSON only; there is no OTLP/gRPC or protobuf path, and plugin-stage metrics are not exported.
- Reload failures and MCP connection restarts are logged but not counted in a dedicated metric.

## Drift instrument

Drift caught by: `crates/octra-storage/src/maintenance.rs` tests, `crates/octra-storage/src/events.rs`, `crates/octra-api-gateway/src/metrics.rs`, and inspection of the control operations handlers. A future test should assert every append-only table is covered by the retention policy.
