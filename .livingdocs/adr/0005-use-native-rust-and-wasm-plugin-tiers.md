---
type: ADR
title: Use native Rust and WASM plugin tiers
description: First-party plugins compile into the daemon; third-party plugins run as capability-gated WASM components.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-06T15:44:39Z
---

# 0005. Use native Rust and WASM plugin tiers

## Context

Octra needs first-party plugins that integrate deeply with the gateway and third-party plugins that can be installed without rebuilding or trusting the daemon. Complete Kong Lua compatibility is out of scope. Rust dynamic libraries offer no stable cross-compiler ABI, no crash isolation, and no practical capability boundary, while arbitrary native code would undermine the security model.

## Decision

We will use two plugin tiers. First-party plugins are Rust implementations compiled into the daemon and registered through gateway-specific traits. Third-party plugins are WebAssembly components with a WIT ABI, declared in a manifest, installed as artifacts, and granted only the capabilities the operator approves. The API and LLM gateways have separate plugin contracts and runtimes.

Rejected alternatives:
- **Rust dynamic libraries** — no stable ABI across compilers and platforms, no isolation, and unloading is unsafe in practice.
- **Kong Lua compatibility** — a large compatibility surface with a different execution and configuration model.
- **One generic plugin trait for every subsystem** — forces API, LLM, MCP, and provider plugins into an over-broad context.
- **JSON-over-core-WASM or Extism** — weaker typing and a less explicit capability model than the component model plus WIT.

## Consequences

**Easier / gained:**
- Third-party code runs inside wasmtime with fuel, memory, timeout, and body-size limits.
- The WIT contract is typed and versioned independently of the host's Rust internals.
- Built-in plugins keep full access to gateway internals without WASM overhead.

**Harder / accepted trade-offs:**
- There are two plugin APIs and two authoring paths to maintain.
- WASM plugins pay instantiation and serialization costs and cannot use arbitrary host APIs.
- Plugin ABI versioning becomes a compatibility contract that must be documented and tested.

**Follow-ups:**
- Keep the plugin SDK aligned with the WIT contract as new phases are added.
- Document the API plugin contract version (currently 2, with events and jobs) and the LLM plugin contract version (currently 5) as compatibility boundaries, alongside the WIT version.

## Verification

**Implementation impact:** `crates/octra-api-plugin-types/`, `crates/octra-api-plugin-runtime/`, `crates/octra-api-plugin-wasm/`, `crates/octra-llm-plugin-*`, `crates/octra-plugin-sdk/`, `crates/octra-api-plugin-wasm/wit/plugin.wit`.

**Verification criteria:**
- Built-in plugins register through gateway-specific traits and require no WASM runtime.
- A third-party component built from the example plugin installs and runs with only approved capabilities.
- `scripts/test-wasm.sh` builds the example component and runs the ignored WASM integration tests.

# References

[1] Octra repository. Available at: https://github.com/luozhouyang/octra. Accessed on: 2026-11-06.

## Follow-ups

- Keep the plugin SDK aligned with the WIT contract as new phases are added.
- Document the API plugin contract version (currently 3, Kong seven phases with chunk/EOF body filtering) and the LLM plugin contract version (currently 6) as compatibility boundaries, alongside the WIT version.
