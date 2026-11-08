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

Provider API keys and OAuth tokens are the most valuable secrets in the system, while coding agents are clients that users install and upgrade independently. Agent configuration files are user-owned, may be backed up or synced, and are not a safe place for provider credentials. The control plane is an administrative surface, not a client data-plane credential. A copied database must not expose reusable tokens or passwords even when the attacker has the database file but not the OS keychain.

## Decision

We will store static provider secrets as references (`env:` or `file:`) and resolve the value inside the daemon at request time. Agents receive Octra virtual keys, never provider keys; virtual keys are hashed in SQLite and carry model/provider permissions. The control-plane Bearer token is admin-only and is never handed to agents. The control API never returns secret values. Values that must survive restarts are sealed at rest with AES-256-GCM under a 32-byte master key: OAuth access/refresh/extra values, KeyAuth keys, BasicAuth passwords, and JWT secrets. Sealed values use `enc:v2:<key_id>:<base64>` with AAD `table|row_id|column`; `enc:v1:` values remain readable and are upgraded on startup. The master key set resolves from `OCTRA_MASTER_KEY`, an existing `master.key`, or the OS keychain (`dev.octra` / `master-key`); if the database exists but no key is available, startup fails instead of generating a new key. `octra secrets rotate` and `POST /admin/secrets/rotate` generate a new active key and re-encrypt every secret row transactionally while old keys remain available for decryption. `octra secrets retire` and `POST /admin/secrets/retire` remove keys that are no longer referenced by any v2 ciphertext; retirement refuses while v1 rows remain and always keeps the active key. The SQLite database and its WAL/SHM sidecars remain `0600` on Unix as defense in depth.

Rejected alternatives:
- **Plaintext provider secrets in SQLite** — exposes them to database backups, copies, and any file reader.
- **Putting provider keys in agent configuration** — distributes the highest-value credential to every client and its backup.
- **Using the control token as the agent credential** — grants administrative access to a data-plane client.
- **Plaintext OAuth tokens and credential passwords at rest** — a copied database would expose reusable sessions.

## Consequences

**Easier / gained:**
- Static provider credentials are not written into agent configs and are not stored as values in SQLite.
- Keys can be rotated upstream without rewriting agent configurations.
- Authorization is enforced against hashed virtual keys and per-key permissions.

**Harder / accepted trade-offs:**
- Operators must provision `env:` or `file:` references and protect those sources themselves.
- Key rotation is manual for referenced secret files.
- The fallback `master.key` sits next to the database; backups that include both expose secrets. Prefer the OS keychain or `OCTRA_MASTER_KEY` for portable backups.
- The master key is per OS user/installation, not per database; there is no key rotation or re-encryption workflow yet.
- A local attacker with the same OS permissions can still read process memory or environment; the boundary is OS-level.

**Follow-ups:**
- Add a dry-run/confirmation flow and an audit report before irreversible retirement.
- Add a future ciphertext format with per-column AAD everywhere and audit the key rotation procedure.

## Verification

**Implementation impact:** `crates/octra-secrets/`, `crates/octra-llm-gateway/src/`, `crates/octra-storage/src/llm.rs`, `crates/octra-storage/src/api/`, `crates/octra-storage/src/lib.rs`, `crates/octra-types/src/auth.rs`.

**Verification criteria:**
- Static provider secret resolution accepts only reference forms and never writes the resolved value to SQLite.
- OAuth tokens, KeyAuth keys, BasicAuth passwords, and JWT secrets are sealed as `enc:v1:` values; reads return plaintext to callers.
- Legacy plaintext is readable and backfilled idempotently; an existing database with an unavailable or wrong master key fails startup.
- Control-plane responses mask or omit secrets and virtual-key material.
- Storage tests assert `0600` on the database and its WAL/SHM sidecars on Unix.
- Data-plane credentials cannot call control-plane endpoints.

# References

[1] Octra repository. Available at: https://github.com/luozhouyang/octra. Accessed on: 2026-11-06.
