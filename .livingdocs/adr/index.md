# ADRs

All records remain **Proposed** until the v0.1 acceptance gaps in the PRD are closed and the maintainer accepts them. The implementation already follows these decisions; see the PRD implementation-status table for evidence and gaps.

## Active

* [0001 — Use Axum and reqwest for the data plane](0001-use-axum-and-reqwest-for-the-data-plane.md) - Proposed
* [0002 — Use SQLite as the only persistence layer](0002-use-sqlite-as-the-only-persistence-layer.md) - Proposed
* [0003 — Run the gateway as an independent daemon](0003-run-the-gateway-as-an-independent-daemon.md) - Proposed
* [0004 — Normalize LLM protocols through a canonical IR](0004-normalize-llm-protocols-through-a-canonical-ir.md) - Proposed
* [0005 — Use native Rust and WASM plugin tiers](0005-use-native-rust-and-wasm-plugin-tiers.md) - Proposed
* [0006 — Give each plugin host-owned isolated storage](0006-give-each-plugin-host-owned-isolated-storage.md) - Proposed
* [0007 — Keep secrets out of storage and agents](0007-keep-secrets-out-of-storage-and-agents.md) - Proposed
* [0008 — Apply agent configs atomically with backups](0008-apply-agent-configs-atomically-with-backups.md) - Proposed
* [0009 — Deliver plugin events through a durable outbox](0009-deliver-plugin-events-through-a-durable-outbox.md) - Proposed
* [0010 — Keep the Rust and TypeScript workspaces in one repository](0010-keep-the-rust-and-typescript-workspaces-in-one-repository.md) - Proposed
* [0011 — Model the API gateway on Kong](0011-model-the-api-gateway-on-kong.md) - Proposed
* [0013 — Model the LLM gateway on magpie](0013-model-the-llm-gateway-on-magpie.md) - Proposed
* [0014 — Model the MCP gateway as a Kong-style aggregator](0014-model-the-mcp-gateway-as-a-kong-style-aggregator.md) - Proposed
* [0015 — Bridge the desktop through an allow-listed control command](0015-bridge-the-desktop-through-an-allow-listed-control-command.md) - Proposed
* [0016 — Redact PII before provider egress](0016-redact-pii-before-provider-egress.md) - Proposed
* [0017 — Compile runtime snapshots off the hot path](0017-compile-runtime-snapshots-off-the-hot-path.md) - Proposed
* [0018 — Model the LLM control plane on AI-native entities](0018-model-the-llm-control-plane-on-ai-native-entities.md) - Proposed
* [0019 — Use Kong plugin phases for the API gateway](0019-use-kong-plugin-phases-for-the-api-gateway.md) - Proposed
* [0020 — Route LLM traffic through the API gateway as an in-process upstream](0020-route-llm-traffic-through-the-api-gateway.md) - Proposed

## Superseded

_History only. Do not act on these records — run `living-docs read` for what is in force._

* [0012 — Use Kong-style plugin phases and scopes](0012-use-kong-style-plugin-phases-and-scopes.md) - Superseded by [0019](0019-use-kong-plugin-phases-for-the-api-gateway.md)
