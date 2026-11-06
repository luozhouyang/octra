---
type: Architecture View
title: Context
description: Octra's external actors and upstream dependencies.
kind: context
timestamp: 2026-11-06T15:46:09Z
---

# Context

Octra sits between local users and agents on one side and HTTP APIs, LLM providers, and MCP servers on the other; the desktop app and CLI manage one local daemon.

```mermaid
flowchart LR
    Dev[Developer] --> Desk[Octra Desktop]
    Dev --> CLI[Octra CLI]
    Agent[Claude Code / Codex / OpenCode] -->|virtual key| Daemon[Octra Daemon]
    Client[API clients] -->|API key| Daemon
    Desk -->|control API| Daemon
    CLI -->|control API| Daemon
    Daemon -->|atomic config writes| AgentConfigs[Agent config files]
    Daemon -->|HTTP| APIs[HTTP APIs]
    Daemon -->|OpenAI / Anthropic| Providers[LLM providers]
    Daemon -->|stdio / HTTP| MCP[MCP servers]
```

## Drift instrument

Drift caught by: inspection of the listener and adapter wiring in `crates/octra-daemon/src/main.rs`; this view has no generated conformance check yet.
