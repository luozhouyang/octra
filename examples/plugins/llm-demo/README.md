# LLM Gateway plugin example

A minimal third-party LLM plugin written as a WebAssembly component. It shows
the artifact layout, the WIT contract, and the JSON envelope the host exchanges
with every component.

## Build

```bash
cargo component build --release --manifest-path examples/plugins/llm-demo/Cargo.toml
```

The component is written to `target/wasm32-wasip1/release/llm_demo.wasm`. An
installable artifact is a directory containing:

```text
llm-demo/
├── plugin.json
└── plugin.wasm
```

`plugin.json` must declare `"kind": "llm"` and `"runtime": "wasm"`. LLM
artifacts may not declare entities, migrations or admin routes; those belong to
API Gateway plugins.

## Install

```bash
AUTH="Authorization: Bearer $(cat ~/.octra/control.token)"
CTRL=http://127.0.0.1:8081

# 1. Upload the artifact directory.
curl -sS -X POST "$CTRL/admin/plugin-artifacts" \
  -H "$AUTH" -H 'content-type: application/json' \
  -d '{"path":"/absolute/path/to/llm-demo"}'

# 2. Install it (use the returned artifact id).
curl -sS -X POST "$CTRL/admin/plugins/llm-demo/install" \
  -H "$AUTH" -H 'content-type: application/json' \
  -d '{"artifact_id":"par_..."}'

# 3. Configure one instance (global scope shown).
curl -sS -X POST "$CTRL/admin/llm/plugins/plg_llm-demo/configs" \
  -H "$AUTH" -H 'content-type: application/json' \
  -d '{"scope":"global","config":{"reject_models":["blocked-model"]}}'
```

Install, upgrade and uninstall activate the component without restarting the
daemon. A failed reload or catalog write is rolled back, and the compiled
component cache is evicted on uninstall.

## Configuration

The reference plugin accepts:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `reject_models` | array of strings | `[]` | Requests for these models are rejected with 429 |
| `header_name` | string | `x-llm-demo` | Response header added on success |
| `header_value` | string | `active` | Value of that header |

`configure(config-json)` is called once per component instance before the first
stage. Returning an error from `configure` fails the instance creation and is
reported by `/admin/llm/plugins` diagnostics.

## Contract

The WIT file in `wit/llm-plugin.wit` is the ABI. The host imports `host`
(`log`, `now-unix`) and the component exports `plugin-api`:

```wit
configure: func(config-json: string) -> result<_, string>;
on-stage: func(call: invocation) -> result<outcome, string>;
```

Once per request the host reuses the same component instance across stages;
the instance is dropped on the log stage (or after five idle minutes). State
returned in `state-json` is passed back unchanged to the next stage.

`invocation` fields:

| Field | Content |
|---|---|
| `stage` | `request`, `provider-request`, `response`, `stream-event`, `error` or `log` |
| `request-id` | Stable id for one client request |
| `scope` / `scope-id` | Binding scope (`global`, `consumer`, `virtual_key`, `model`, `provider`) |
| `config-json` | The instance configuration |
| `meta-json` | Client identity: request id, ingress protocol, consumer, virtual key, groups, client IP, user agent, intent |
| `payload-json` | Stage payload (see below) |
| `state-json` | State from the previous stage |

`outcome` fields:

| Field | Meaning |
|---|---|
| `action` | `continue`, `reject`, `replace` or `ignore` |
| `payload-json` | Mutated payload; `null` leaves it unchanged |
| `state-json` | State for the next stage |
| `error-json` | Serialized `GatewayError`; required for `reject` |
| `headers` | Response headers, only applied on the request stage |

### Payloads

| Stage | `payload-json` | Mutation handed back |
|---|---|---|
| `request` | `{"request": ChatRequest}` | `{"request": ChatRequest}` |
| `provider-request` | `{"provider": {...}, "request": ChatRequest}` | `{"request": ChatRequest}` |
| `response` | `{"response": ChatResponse, "usage": Usage}` | `{"response": ..., "usage": ...}` |
| `stream-event` | `{"event": StreamEvent}` | `{"event": ...}`, or `replace` with `{"events": [...]}` |
| `error` | `{"provider_id": ..., "account_id": ..., "error": GatewayError}` | `{"error": GatewayError}` |
| `log` | Content-free event summary | Not consumed |

A rejection carries a `GatewayError` such as:

```json
{"status": 429, "kind": "rate_limited", "code": "model_blocked", "message": "..."}
```

### Response headers

The request stage may add response headers. The host validates names and
values, rejects hop-by-hop, body-framing and host-reserved headers, and caps the
count and value size. A rejected header is dropped with a warning; it never
fails the request.

## Component policy

The component runs with fuel, a memory limit and a wall-clock timeout from the
manifest. LLM components currently have no network or plugin-storage access;
only `log` and `now-unix` are available. The host sandboxes WASI and treats a
trap as a plugin failure, applying the manifest `fail_policy`. Request-stage
plugins that declare `request_body` are forced to fail closed so a half-rewritten
request is never routed.

## Test

```bash
cargo component build --release --manifest-path examples/plugins/llm-demo/Cargo.toml
cargo test -p octra-llm-plugin-wasm --test wasm_e2e -- --ignored
```

The test loads the built component through the real host, checks header
injection, the JSON rejection envelope and per-request state.
