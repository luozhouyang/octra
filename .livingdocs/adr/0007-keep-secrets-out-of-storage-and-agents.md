---
type: ADR
title: Keep secrets out of storage and agents
description: Store provider secrets as references, issue virtual keys to agents, and reserve the control token for administrators.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-06T15:44:40Z
---

# 0007. Keep secrets out of storage and agents

## Context

Provider API keys and OAuth tokens are the most valuable secrets in the system, while coding agents are clients that users install and upgrade independently. Agent configuration files are user-owned, may be backed up or synced, and are not a safe place for provider credentials. The control plane is an administrative surface, not a client data-plane credential. OS keychain integration is planned but not implemented, so the current mechanism must limit what is persisted in ordinary tables.

## Decision

We will store static provider secrets as references (`env:` or `file:`) and resolve the value inside the daemon at request time. Agents receive Octra virtual keys, never provider keys; virtual keys are hashed in SQLite and carry model/provider permissions. The control-plane Bearer token is admin-only and is never handed to agents. The control API never returns secret values. OAuth access and refresh tokens are the deliberate exception: the daemon must refresh sessions, so they persist in the dedicated `llm_provider_oauth_tokens` table and inherit the database's file-permission protection. Because OAuth tokens and agent config backups may contain credentials, the SQLite database and its WAL/SHM sidecars are restricted to `0600` on Unix.

Rejected alternatives:
- **Plaintext provider secrets in SQLite** — exposes them to database backups, copies, and any file reader.
- **Putting provider keys in agent configuration** — distributes the highest-value credential to every client and its backup.
- **Using the control token as the agent credential** — grants administrative access to a data-plane client.
- **Encryption at rest now** — would add key-management work without solving the local file-permission boundary; it remains a future hardening step.

## Consequences

**Easier / gained:**
- Static provider credentials are not written into agent configs and are not stored as values in SQLite.
- Keys can be rotated upstream without rewriting agent configurations.
- Authorization is enforced against hashed virtual keys and per-key permissions.

**Harder / accepted trade-offs:**
- Operators must provision `env:` or `file:` references and protect those sources themselves.
- Key rotation is manual for the referenced secret files.
- OAuth tokens are stored in plaintext inside the protected database, so a copied database exposes them; encryption or keychain storage is still pending.
- A local attacker with the same OS permissions can still read process memory or environment; the boundary is OS-level.

**Follow-ups:**
- Add OS keychain-backed secret references without changing the reference-based contract.
- Encrypt or keychain-back the OAuth token table.

## Verification

**Implementation impact:** `crates/octra-llm-gateway/src/`, `crates/octra-storage/src/llm.rs`, `crates/octra-storage/src/lib.rs`, `crates/octra-types/src/auth.rs`.

**Verification criteria:**
- Static provider secret resolution accepts only reference forms and never writes the resolved value to SQLite.
- OAuth tokens persist only in `llm_provider_oauth_tokens`, inherit the database's `0600` protection, and are never returned by the control API.
- Control-plane responses mask or omit secrets and virtual-key material.
- Storage tests assert `0600` on the database and its WAL/SHM sidecars on Unix.
- Data-plane credentials cannot call control-plane endpoints.

# References

[1] Octra repository. Available at: https://github.com/luozhouyang/octra. Accessed on: 2026-11-06.
