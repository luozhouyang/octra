---
type: Architecture View
title: LLM Request Path
description: How an LLM request moves from virtual-key authentication through the canonical IR to a provider and back.
kind: flow
timestamp: 2026-11-06T15:49:51Z
---

# LLM Request Path

An LLM request crosses authentication, protocol normalization, plugin phases, routing, and a provider adapter before the response is translated back; routing and plugins see the canonical IR, not vendor payloads.

```mermaid
flowchart LR
    Client[Client request] --> Auth[Authenticate virtual key]
    Auth --> Adapter[Client protocol adapter]
    Adapter --> IR[Canonical IR]
    IR --> Plugins[LLM plugin chain]
    Plugins --> Route[Virtual model routing]
    Route --> Select[Routing group / target selection]
    Select --> Provider[Provider adapter]
    Provider --> Upstream[Provider API]
    Upstream --> Provider
    Provider --> Response[Canonical IR response / stream]
    Response --> Plugins
    Plugins --> Adapter
    Adapter --> ClientOut[Client response / stream]
    Response --> Usage[(Usage and logs)]
```

`Auth` rejects an unknown or disallowed virtual key before any upstream call. `Plugins` run in configured order and can reject, rewrite, or observe the IR. `Route` resolves `virtual_model_id` to a routing group; `Select` applies priority, weight, and fallback. `Usage` is written from the canonical response so accounting is protocol-independent.

## Drift instrument

Drift caught by: inspection of the LLM data-plane pipeline plus the protocol fixture tests in `crates/octra-llm-protocol/tests/`; no generated flow oracle is wired yet.
