---
type: ADR
title: Use Axum and reqwest for the data plane
description: Use Tokio, Axum, hyper, and reqwest with rustls instead of Pingora.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-06T15:44:39Z
---

# 0001. Use Axum and reqwest for the data plane

## Context

The original architecture selected Pingora as the proxy engine. Pingora's TLS and network stack had not been verified across macOS, Windows, and Linux, and the daemon needs one async stack for the control plane, HTTP/SSE proxying, and LLM streaming. The project is a single-node local gateway, so an unverified cross-platform proxy dependency carried more risk than performance upside.

## Decision

We will implement the data plane with Tokio, Axum 0.8, hyper, and reqwest using rustls, and keep Pingora out of the runtime. The API, LLM, and MCP gateway routers, the control plane, and the shared data listener all use this stack. The API gateway owns routing, retries, load balancing, and streaming on it; the LLM gateway owns protocol translation and SSE streaming; the MCP gateway owns Streamable HTTP and stdio upstream transports.

Rejected alternatives:
- **Pingora** — cross-platform TLS and network support was unverified, and it duplicated primitives the rest of the daemon already uses.
- **A hand-written hyper service** — more control, but substantially more code for connection pooling, timeouts, and TLS than reqwest provides.
- **An external reverse proxy (Nginx/Envoy)** — not programmable from the control plane and adds a system dependency that conflicts with local-first installation.

## Consequences

**Easier / gained:**
- One async runtime and HTTP stack across the daemon, gateways, and tests.
- Streaming, retries, and header rewriting are ordinary Rust code covered by integration tests.
- No C/FFI build requirements beyond rustls and reqwest.

**Harder / accepted trade-offs:**
- We own the proxy hot path and must benchmark it before optimizing.
- reqwest's connection pool is less configurable than a purpose-built proxy engine.
- Pingora-specific performance features are unavailable unless an engine is reintroduced later.

**Follow-ups:**
- Benchmark p50/p99 proxy overhead once the data plane has production traffic; only then consider a dedicated engine.

## Verification

**Implementation impact:** `crates/octra-api-gateway/src/lib.rs`, `crates/octra-data-plane/src/`, `crates/octra-daemon/src/main.rs`.

**Verification criteria:**
- The daemon starts and serves both listeners with no Pingora dependency.
- Proxy integration tests cover streaming, retries, header rewriting, and target fallback.
- `cargo tree | grep -i pingora` returns nothing.

# References

[1] Octra repository. Available at: https://github.com/luozhouyang/octra. Accessed on: 2026-11-06.
