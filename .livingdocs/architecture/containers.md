---
type: Architecture View
title: Containers
description: The desktop, daemon, runtime, plugin, and storage containers and how they talk.
kind: container
timestamp: 2026-11-06T15:46:09Z
---

# Containers

The runnable pieces are a Tauri desktop app and an independent daemon; the daemon owns the control plane, the data plane, the gateway runtime, the plugin runtime, and SQLite.

```mermaid
flowchart TB
    subgraph Desktop[Octra Desktop]
        React[React console]
        Tauri[Tauri host / control client]
        React --> Tauri
    end

    subgraph Daemon[Octra Daemon]
        Control[Control API :8081]
        Data[Data plane :8080]
        Runtime[Gateway runtime]
        Plugins[Plugin runtime]
        Store[(SQLite octra.db)]
        Control --> Runtime
        Control --> Store
        Data --> Runtime
        Runtime --> Plugins
        Runtime --> Store
    end

    Tauri -->|Bearer token| Control
    CLI[Octra CLI] -->|Bearer token| Control
    Agents[Agents / API clients] --> Data
    Data -->|HTTP / SSE| Upstreams[API, LLM, MCP upstreams]
```

## Drift instrument

Drift caught by: inspection of `crates/octra-daemon/src/main.rs` and the Tauri sidecar commands; no generated container graph is wired yet.
