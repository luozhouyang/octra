---
type: ADR
title: Bridge the desktop through an allow-listed control command
description: Keep the control token in the Tauri host, expose one path- and method-allow-listed bridge command, and share one control API client between the desktop and browser modes.
owner: luozhouyang
status: Proposed
timestamp: 2026-11-07T12:40:00Z
---

# 0015. Bridge the desktop through an allow-listed control command

## Context

The desktop console is a Tauri 2 webview plus a React app. The control plane is an authenticated HTTP API on loopback, and the control token is an administrative credential that must not be handed to an agent or exposed to arbitrary web content. The desktop also has a browser-only development mode, where the page talks to the control plane directly with a token from a local settings store.

Putting the token in the webview (localStorage, a global, or a fetched config) would make any script injection or a compromised dependency an administrative compromise. Embedding the gateway in the Tauri process would tie the data plane to the webview lifecycle (ADR 0003). The desktop therefore needs a narrow bridge that holds the token outside the webview and only permits control-plane calls.

## Decision

We will keep the control token in the Tauri host and bridge the console through one allow-listed command:

- The Tauri host reads `OCTRA_CONTROL_TOKEN` or `$OCTRA_HOME/control.token`, resolves the control base URL from `$OCTRA_HOME/config.toml` (`daemon.control_bind`, default `127.0.0.1:8081`), and exposes one generic `daemon_request(method, path, body)` command.
- `daemon_request` accepts only paths starting with `/api/v1/` or `/admin/` and only the methods `GET`, `POST`, `PUT`, and `DELETE`. It attaches the Bearer token and returns the status and parsed JSON body.
- The webview never receives the token in Tauri mode. It uses the shared `ControlApiClient` with a `desktopFetch` adapter that calls `invoke("daemon_request", ...)`.
- The host also exposes one narrow `oauth_wait_callback(redirect_uri, state, timeout_secs)` command. It accepts only loopback `http` redirect URIs with an explicit port, binds that port, accepts one request, validates the returned `state`, returns the authorization code to the webview, and writes a self-contained success/failure page. It does not expose a general socket or IPC surface.
- The host exposes `daemon_status`, `daemon_start`, `daemon_stop`, and `daemon_restart` commands backed by `crates/octra-desktop-daemon`. The manager discovers `OCTRA_DAEMON_BIN`, a bundled `octra-daemon` sidecar, or a development target binary; it only kills a child it started.
- In browser-only mode, the console reads the base URL and token from the local settings store and uses the same `ControlApiClient` with the browser `fetch`.
- The React app is a live console with targeted editors over the control plane: Overview, API Gateway, LLM Gateway, MCP Gateway, Agents, Plugins, and Settings. AI Control, API Gateway, MCP, and consumer policies expose typed-form CRUD; agents, LLM plugin enable/disable, and PII model actions retain their existing mutation surfaces.
- The Tauri host contains no gateway logic. It is a token-holding, path-filtering adapter, the loopback OAuth callback listener, and the daemon lifecycle supervisor.

**Not adopted:**

- Token injection into the webview, a token in `localStorage`, or direct `fetch` from the Tauri webview to the control plane.
- Embedding the daemon in the Tauri process.
- An IPC surface that mirrors every control endpoint one-for-one. The single generic bridge is smaller and easier to audit; the OAuth and lifecycle commands are narrowly scoped exceptions.
- Killing a daemon that the desktop did not start.

## Rejected alternatives

- **Token in the webview** — any XSS or a compromised npm dependency becomes an admin compromise.
- **Embedding the gateway in the Tauri process** — a GPU/WebView crash would stop traffic and headless use would be impossible.
- **One Tauri command per endpoint** — multiplies the allow-list maintenance and makes the bridge easy to drift from the HTTP API.
- **No browser mode** — slows frontend development and prevents a browser UI over a remote local daemon.

## Consequences

**Easier / gained:**

- The administrative token never enters the webview in packaged mode.
- The desktop and browser share one typed client and one control-plane contract.
- The allow-list is small enough to review and test.
- A GUI reload does not interrupt gateway traffic.

**Harder / accepted trade-offs:**

- The bridge is a generic proxy; its path filter is the security boundary and must be tested.
- The browser mode still needs a token in the settings store, so it is a development convenience rather than a hardened deployment.
- The UI is currently an observability console, not a full management console. Operators must use the Admin API or CLI for most CRUD.
- The desktop cannot start, stop, or supervise the daemon yet.

**Follow-ups:**

- Add UI CRUD for the API, LLM, and MCP entities that are currently read-only.
- Add daemon start/stop/health supervision and a version handshake during packaging.
- Add a Tauri bridge test for the path and method allow-list.
- Decide whether browser mode should require a separate read-only or scoped token.

## Verification

**Implementation impact:** `apps/desktop/src-tauri/src/lib.rs`, `apps/desktop/src/lib/control-api.ts`, `apps/desktop/src/lib/store.ts`, `apps/desktop/src/app/App.tsx`, `packages/api-client/src/index.ts`, `packages/types/src/index.ts`.

**Verification criteria:**

- A packaged desktop request to `/admin/services` succeeds through `daemon_request`.
- A request to a path outside `/api/v1/` and `/admin/` is rejected before any network call.
- A method outside the allow-list is rejected.
- The webview cannot read `control.token` directly.
- Browser mode uses the same `ControlApiClient` and works against a local daemon with a configured token.
- Every page renders live health or entity data from the control plane.

# References

[1] Octra ADR 0003, "Run the gateway as an independent daemon." `0003-run-the-gateway-as-an-independent-daemon.md`
[2] Octra ADR 0010, "Keep the Rust and TypeScript workspaces in one repository." `0010-keep-the-rust-and-typescript-workspaces-in-one-repository.md`
[3] Tauri, "Calling Rust from the frontend." https://v2.tauri.app/develop/calling-rust/
[4] Octra repository, `apps/desktop` and `packages/api-client`. https://github.com/luozhouyang/octra
