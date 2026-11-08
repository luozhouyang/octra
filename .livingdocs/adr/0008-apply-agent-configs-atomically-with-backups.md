---
type: ADR
title: Apply agent configs atomically with backups
description: Per-agent adapters preview, atomically apply, back up, and restore native config files, with a sandbox mode.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-06T15:44:40Z
---

# 0008. Apply agent configs atomically with backups

## Context

Claude Code, Codex, and OpenCode each own a configuration file in a different format and location, and users expect Octra to point those agents at the local gateway. A careless write can break a working agent setup or lose comments, unknown keys, and credentials already in the file. Some files are JSONC, some are TOML, and some may be symlinked into a dotfiles repository.

## Decision

We will integrate through per-agent adapters that detect the tool, read and normalize its configuration, render a preview, apply changes atomically, and restore a previous version. Writes resolve symlinks, replace the file through a temporary file plus rename, and set `0600` permissions on Unix. The original file is backed up in SQLite before the first change, conflicts are detected with a SHA-256 check, backups are capped at 20 per agent, and a sandbox mode redirects detection and writes to a test home so the real configs are untouched.

Rejected alternatives:
- **Documentation-only setup** — error-prone and impossible to verify or roll back from the desktop app.
- **In-place mutation without a backup** — a failed or partial write can destroy a working agent configuration.
- **Rewriting the whole file from a template** — discards comments, unknown keys, and user customizations.
- **Writing provider keys into agent configs** — couples agent setup to upstream credentials instead of virtual keys.

## Consequences

**Easier / gained:**
- Users can preview, apply, and restore changes from the desktop app or control API.
- Existing comments and unrelated settings survive a Codex TOML or OpenCode JSONC edit.
- Sandbox mode makes the integration testable without touching a real machine.

**Harder / accepted trade-offs:**
- Each supported agent and config version needs an adapter and renderer.
- Backups may contain credentials, so they inherit the database's `0600` protection and a retention cap.
- Conflict detection rejects concurrent changes rather than merging them.
- Applying a config sets the file mode to `0600` on Unix rather than preserving a looser original mode.

**Follow-ups:**
- Add adapters only when an agent exposes a stable configuration contract; avoid inferring one.
- Extend the daemon sandbox integration test to Codex and OpenCode; today it exercises Claude Code end to end while the other two adapters rely on unit render/read tests.

## Verification

**Implementation impact:** `crates/octra-agent-core/`, `crates/octra-storage/src/agents.rs`, `crates/octra-agent-core/src/control.rs`, `apps/desktop/src/app/pages/agents.tsx`.

**Verification criteria:**
- Preview writes nothing; apply is atomic and restore returns byte-identical original content.
- A symlinked config is updated through the link without replacing it.
- The sandbox integration test applies and restores an agent config under a temporary home.

# References

[1] Octra repository. Available at: https://github.com/luozhouyang/octra. Accessed on: 2026-11-06.
