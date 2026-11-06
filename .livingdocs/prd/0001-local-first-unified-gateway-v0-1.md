---
type: PRD
title: Local-first unified gateway v0.1
description: Deliver one local daemon that unifies API, LLM, and MCP gateways with safe agent onboarding, SQLite persistence, and an extensible plugin system.
status: Draft
timestamp: 2026-11-06T16:03:37Z
---

# 0001. Local-first unified gateway v0.1

## Problem / Motivation

Developers who use multiple AI coding agents, HTTP APIs, and MCP servers configure each tool separately. Provider keys are copied into agent configs, model changes require editing every client, and there is no single local place to see or govern the traffic. Server-oriented gateways assume cloud deployment, external databases, and multi-tenant operations, which is the wrong shape for one developer's machine.

**Who asked:** the project maintainer and the target users — developers and AI-agent users who want one local control plane instead of a collection of per-tool proxies and config files.

## Goals

- One local daemon serves API, LLM, and MCP traffic through one authenticated control plane and one SQLite persistence layer.
- Provider, model, routing, and virtual-key governance are centralized; agents receive Octra virtual keys, not provider credentials.
- A developer can point Claude Code, Codex, or OpenCode at Octra from the desktop app and restore the previous configuration safely.
- Request logs, audit logs, LLM usage, and metrics make local traffic observable.
- A first-party plugin API and a sandboxed third-party WASM path make the gateway extensible without forking the daemon.
- The desktop app and CLI manage the same control-plane API.

## Non-goals

- A cloud control plane, remote multi-user administration, a distributed gateway cluster, or Kubernetes deployment.
- PostgreSQL, MySQL, Redis, Elasticsearch, or any required external database, cache, or broker.
- Complete Kong Lua plugin compatibility or a general-purpose agent runtime.
- Unsandboxed third-party native code or Rust dynamic-library plugin loading.
- OS keychain integration and OAuth-token encryption in v0.1; static secret references and `0600` database permissions are the current boundary.
- Desktop-side daemon lifecycle supervision, signed installers, and notarized packages in v0.1.

## Requirements

- **R1 Local runtime.** `pnpm daemon` starts one daemon process with a control listener (`8081` by default) and a data listener (`8080` by default); both bind loopback by default, and every control endpoint requires the Bearer token.
- **R2 API gateway.** The data plane supports host, path, method, and header routing; round-robin and weighted targets; retries and fallback; request/response streaming; API-key authentication; policy authorization; access logs; JSON and Prometheus metrics.
- **R3 LLM gateway.** The gateway accepts OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, and Gemini requests; converts them through the canonical IR; routes `virtual_model_id` through routing groups to provider targets with fallback; and records usage.
- **R4 MCP gateway.** The gateway supports Streamable HTTP and stdio upstreams, tool discovery and aggregation, the `/mcp` JSON-RPC endpoint with sessions, and per-consumer tool ACLs.
- **R5 Agent integration.** Claude Code, Codex, and OpenCode adapters detect installations, render a preview, apply changes atomically, restore the original, and run under a sandbox home without touching real configs.
- **R6 Plugins.** Built-in plugins use the gateway traits; third-party plugins use the WASM component ABI with a manifest, approved capabilities, and a host-owned per-plugin SQLite database; durable events and scheduled jobs are delivered by the daemon.
- **R7 Persistence.** SQLite is the only persistence engine, with SQLx migrations, WAL, foreign keys, a busy timeout, usage rollups, and a retention worker; no external service is required.
- **R8 Console and CLI.** Every desktop page reads the live control plane, and the CLI exposes status, services, routes, reload, and doctor commands over the same API.
- **R9 Security boundary.** Static provider secrets resolve from `env:` or `file:` references at request time; virtual keys are hashed; the control token is admin-only; the database and WAL/SHM sidecars are `0600` on Unix; the control API never returns secret values.
- **Q1 Atomic agent writes.** A crash or I/O failure during an agent config apply leaves either the old file or the new file, never a partial write.
- **Q2 Secret containment.** Static provider secret values do not appear in SQLite rows or control-plane responses, verified by storage and control tests.
- **Q3 Protocol coverage.** Every supported client and provider protocol has conversion fixtures for requests, responses, and streams, and the fixture suite passes.
- **Q4 Local-only startup.** A clean machine with Rust, Node, and pnpm can start the daemon and pass the gateway tests without a database server or broker.
- **Q5 Docs gate.** `living-docs check .livingdocs` passes in the project check script.

## Acceptance criteria

- A fresh `pnpm daemon` creates `$OCTRA_HOME`, applies every migration, and `pnpm cli status` succeeds.
- A request through each gateway reaches a mock upstream, including an SSE response for LLM streaming.
- Applying and restoring an agent config in sandbox mode round-trips byte-identically for Claude Code, Codex, and OpenCode.
- The reference `consumer-quota` WASM plugin installs, serves a request, and retains plugin-owned data across an upgrade.
- A published event with a matching subscriber is delivered and retried, and a scheduled job runs; terminal events are pruned.
- `cargo fmt --all --check`, `cargo clippy --workspace --exclude octra-desktop --all-targets -- -D warnings`, `cargo test --workspace --exclude octra-desktop`, `pnpm typecheck`, `pnpm test`, and `pnpm build` pass.
- Control endpoints reject a missing or invalid Bearer token; the storage tests assert `0600` on the database and sidecars; the control API masks secrets.

## Success metrics

- A developer can configure one supported coding agent to use Octra in ten minutes or less, measured by a timed walkthrough using only the README and desktop console.
- Apply/restore completes 100 consecutive times in the sandbox matrix without a manual config repair or a corrupted file.
- All supported protocol conversions and all three agent adapters have passing fixtures or sandbox tests; the suite reports zero failures.
- A clean-machine startup test needs no external database, cache, or broker process.
- The reference third-party plugin installs and serves traffic in CI.

## Open questions

- What p95 proxy-overhead budget should R2 hold, and which benchmark enforces it? Resolve with an ADR before publishing a performance claim.
- When do OS keychain storage and OAuth-token encryption land? The current boundary is recorded in ADR 0007.
- When does desktop-side daemon supervision replace the current operator-started daemon? The current boundary is recorded in ADR 0003.
- Which protocol adapter or coding agent is next after the current four protocols and three agents?
- Should this PRD be accepted once the v0.1 acceptance criteria are demonstrated, or amended per capability?

## Related

- Constitution: [/constitution.md](/constitution.md)
- ADRs: [/adr/index.md](/adr/index.md)
- Architecture: [/architecture/index.md](/architecture/index.md)
- Issues: none yet.
