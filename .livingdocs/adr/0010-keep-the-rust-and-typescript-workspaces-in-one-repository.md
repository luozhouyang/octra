---
type: ADR
title: Keep the Rust and TypeScript workspaces in one repository
description: Use one monorepo with shared control-plane types and clients across the daemon, desktop, and web apps.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-06T15:44:40Z
---

# 0010. Keep the Rust and TypeScript workspaces in one repository

## Context

The daemon, the Tauri desktop app, the browser frontend, the admin CLI, and shared UI code all depend on the same control-plane contract. A contract change that lands in one repository before another creates version skew and duplicate client code. The project is developed by a small team and ships the desktop app and daemon together, so release coordination across repositories would add overhead without a corresponding benefit.

## Decision

We will keep the Rust workspace, the Tauri host, the React apps, and the shared TypeScript packages in one repository. Cargo manages the Rust crates; pnpm manages the TypeScript workspaces. Control-plane types live in `packages/types`, the HTTP client lives in `packages/api-client`, and the desktop and web frontends consume both. CI runs Rust and web jobs from the same commit.

Rejected alternatives:
- **Polyrepo** — requires coordinated releases and allows the daemon and clients to drift between repositories.
- **Generating the client only from an OpenAPI document** — there is no complete spec-first contract yet, and the shared types carry behavior the generated client would not.
- **Duplicating types per app** — guarantees divergence and multiplies review work for every endpoint change.

## Consequences

**Easier / gained:**
- A contract change is one atomic commit reviewed and tested across daemon, client, and UI.
- One checkout contains the code, migrations, examples, and docs needed to reproduce an issue.
- CI can enforce formatting, linting, and tests across both ecosystems together.

**Harder / accepted trade-offs:**
- The repository and CI are larger and slower than a single-language project.
- Workspace tooling must keep Rust and pnpm boundaries coherent.
- Contributors need both Rust and Node toolchains for full verification.

**Follow-ups:**
- Keep the shared packages free of app-specific code; extract a package only when a second consumer needs it.

## Verification

**Implementation impact:** root `Cargo.toml`, `pnpm-workspace.yaml`, `packages/types/`, `packages/api-client/`, `.github/workflows/ci.yml`.

**Verification criteria:**
- `cargo test --workspace --exclude octra-desktop` and `pnpm typecheck && pnpm test` run from one checkout.
- A control-plane type change compiles in the daemon and both frontends without a copied definition.
- CI runs the Rust, WASM, and web jobs on the same commit.

# References

[1] Octra repository. Available at: https://github.com/luozhouyang/octra. Accessed on: 2026-11-06.
