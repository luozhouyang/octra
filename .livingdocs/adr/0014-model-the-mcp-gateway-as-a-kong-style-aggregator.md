---
type: ADR
title: Model the MCP gateway as a Kong-style aggregator
description: Front many MCP servers behind one /mcp endpoint with Streamable HTTP ingress, stdio or HTTP upstreams, session binding, namespaced tools, API-key authentication, consumer/group/default tool ACLs, and invocation audit.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-07T12:40:00Z
---

# 0014. Model the MCP gateway as a Kong-style aggregator

## Context

Octra's MCP gateway lets MCP clients use many upstream MCP servers through one local endpoint. The reference is Kong's AI MCP Proxy: Kong can front an existing MCP server, expose a managed MCP listener, aggregate tools from multiple definitions, authenticate callers with its auth plugins, enforce ACLs, and emit observability for MCP traffic.

Kong's proxy has several modes: `passthrough-listener` (proxy an upstream MCP server), `conversion-listener` (convert REST/OpenAPI operations into MCP tools), `conversion-only` (define tools without serving them), and `listener` (aggregate tools by tag). Kong's AI Gateway 2.x also describes an `upstream-server` mode. Its ACL model supports default and per-tool rules, consumer and consumer-group identity, and (in newer versions) CEL expressions.

Octra's scope is narrower: it fronts upstream MCP servers and aggregates their tools. It does not convert arbitrary REST APIs into MCP tools in v0.1. The code already implements the passthrough/aggregation half and extends it with stdio upstreams, which Kong's HTTP-oriented proxy does not cover.

## Decision

We will model the MCP gateway as a Kong-style aggregator with Octra's local-first constraints:

- Expose one Streamable HTTP endpoint at `/mcp` (and `/mcp/`) for MCP clients. POST carries JSON-RPC requests and notifications; GET holds an SSE keepalive channel; DELETE terminates the session.
- Support upstream transports `http` (Streamable HTTP) and `stdio` (one child process per server, newline-delimited JSON-RPC, request-id correlation through oneshot channels, kill-on-drop).
- Implement the MCP JSON-RPC methods `initialize`, `ping`, `tools/list`, and `tools/call`; accept notifications and return `202` when no reply is required. Negotiate protocol versions `2025-06-18`, `2025-03-26`, and `2024-11-05`.
- Create an Octra session on `initialize`, return it in `Mcp-Session-Id`, and require the header on every later request except `initialize`. The session row is persisted so a restart can still answer `DELETE`.
- Namespace every upstream tool as `<server>__<tool>`. `tools/list` returns only enabled tools whose server is enabled and whose ACL allows the caller. `tools/call` parses the namespace, resolves the server and tool, re-checks enablement and ACL, then calls the upstream.
- Authenticate data-plane callers with MCP API keys (`Authorization: Bearer` or `X-API-Key`); keys are stored as a display prefix plus SHA-256 hash. When no keys exist, the loopback-local default remains open.
- Enforce tool ACLs by scope: an explicit consumer row wins, then matching consumer-group rows with deny-wins, then the tool default row. A tool with no rows follows the open-local default. Callers resolve their consumer groups at authentication time.
- Persist one invocation row per tool call, including denials and unknown tools when the owning server can be identified, with request id, status, duration, consumer, and error.
- Manage upstream connections lazily, keyed by the full server definition; retry a retryable failed call once after restarting the connection; drop every live connection on runtime reload.
- Expose control endpoints under `/api/v1/mcp/*` for servers, tools, sessions, invocations, keys, and ACLs, plus a refresh action that re-runs discovery.

**Not adopted in v0.1:**

- Kong's `conversion-listener`, `conversion-only`, `listener` tag aggregation, and `upstream-server` modes. Octra does not convert OpenAPI operations into MCP tools.
- Kong auth plugins (Key Auth, OIDC, etc.) as the MCP auth mechanism. Octra uses its own MCP API keys and consumer identity.
- Kong's plugin-based MCP proxy model. Octra's MCP gateway is a first-class router; API and LLM plugin chains do not run on MCP traffic.
- CEL-based ACL expressions. Octra supports consumer, consumer-group, and tool-default scopes.
- Server-initiated notifications, tool-list change notifications, session sharing between gateway instances, and structured output.
- Non-HTTP upstream protocols beyond stdio (for example WebSocket or gRPC).

## Rejected alternatives

- **One endpoint per upstream server** — defeats the purpose of a single local MCP endpoint and pushes server selection to every client.
- **Transparent passthrough only** — cannot aggregate tools, enforce per-tool ACLs, or record a unified invocation log.
- **OpenAPI-to-MCP conversion in v0.1** — a large schema and operation-mapping surface that the product has not committed to; Kong treats it as a separate mode.
- **Kong or an external MCP proxy** — adds a service dependency and a control plane that contradicts local-first installation.
- **No ACLs / open local access only** — leaves no path for multiple local consumers or agents with different tool permissions.

## Consequences

**Easier / gained:**

- One `/mcp` endpoint works for clients that support Streamable HTTP.
- stdio and HTTP upstreams share the same discovery, ACL, and audit model.
- Tool names are stable and collision-free across servers.
- Every call, denial, and failure is observable in the local database.

**Harder / accepted trade-offs:**

- The tool namespace limits server and tool names (the separator is reserved) and requires validation on discovery.
- A session is anchored to the first registered server because `mcp_sessions.server_id` is non-null; a gateway session can span servers but the schema cannot yet express that cleanly.
- The gateway does not push notifications, so GET is only a keepalive channel.
- ACL scopes are limited to consumer, consumer group, and tool default; there is no expression language or per-tool CEL policy.
- stdio child processes are a local code-execution boundary; operators must trust the commands they register.

**Follow-ups:**

- Add CEL-based ACL expressions only if a product requirement justifies a policy language.
- Add a session model that does not require a server anchor.
- Add tool-list change notifications and server-initiated notifications when clients need them.
- Add OpenAPI-to-MCP conversion only if a product requirement justifies the mode.
- Add a reference test that installs a stdio server and an HTTP server and exercises aggregation, ACL denial, and audit rows.

## Verification

**Implementation impact:** `crates/octra-mcp-core/src/`, `crates/octra-mcp-gateway/src/http.rs`, `crates/octra-mcp-gateway/src/stdio.rs`, `crates/octra-mcp-gateway/src/servers.rs`, `crates/octra-mcp-gateway/src/data.rs`, `crates/octra-mcp-gateway/src/auth.rs`, `crates/octra-mcp-gateway/src/control.rs`, `migrations/0004_mcp_gateway.sql`, `migrations/0009_gateway_entities.sql`.

**Verification criteria:**

- `initialize` returns a negotiated protocol version and an `Mcp-Session-Id`.
- Every later request without a valid session header is rejected.
- `tools/list` returns only enabled, ACL-allowed tools with `<server>__<tool>` names.
- `tools/call` routes to the correct upstream and records an invocation row.
- An ACL denial records a denied invocation and returns a JSON-RPC forbidden error.
- A failed connection is retried once after the server is restarted.
- DELETE terminates the session; a later request with the same session fails.
- `cargo test -p octra-mcp-gateway -p octra-mcp-core` passes.

# References

[1] Kong, "AI MCP Proxy plugin." https://developer.konghq.com/plugins/ai-mcp-proxy/
[2] Kong, "Aggregate MCP tools from multiple AI MCP Proxy plugins." https://developer.konghq.com/ai-gateway/v1/mcp/aggregate-mcp-tools/
[3] Kong, "AI Gateway 2.x concepts." https://developer.konghq.com/ai-gateway/ai-gateway-v2-concepts/
[4] Model Context Protocol, "Specification 2025-06-18." https://modelcontextprotocol.io/specification/2025-06-18
[5] Octra repository, `crates/octra-mcp-*`. https://github.com/luozhouyang/octra
