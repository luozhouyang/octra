# Connecting coding agents

These templates point local coding agents at the Octra LLM data plane. The
gateway must be running (`pnpm daemon`) and must expose at least one LLM route.

## 1. Prepare the gateway

A coding agent sends a model name. That name must resolve to an LLM route's
`virtual_model_id`, so create the full chain first:

```text
provider -> provider model -> routing group -> target -> route
```

Example (use the control token from `$OCTRA_HOME/control.token`, default
`~/.octra/control.token`):

```bash
AUTH="Authorization: Bearer $(cat ~/.octra/control.token)"
CTRL=http://127.0.0.1:8081

curl -sS -X POST "$CTRL/admin/llm/providers" -H "$AUTH" -H 'content-type: application/json' \
  -d '{"name":"openai-main","protocol":"openai","base_url":"https://api.openai.com/v1","secret_ref":"env:OPENAI_API_KEY"}'

curl -sS -X POST "$CTRL/admin/llm/models" -H "$AUTH" -H 'content-type: application/json' \
  -d '{"provider_id":"<provider-id>","model_id":"gpt-5","display_name":"GPT-5"}'

curl -sS -X POST "$CTRL/admin/llm/routing-groups" -H "$AUTH" -H 'content-type: application/json' \
  -d '{"name":"default","strategy":"priority"}'

curl -sS -X POST "$CTRL/admin/llm/routing-groups/<group-id>/targets" -H "$AUTH" -H 'content-type: application/json' \
  -d '{"provider_id":"<provider-id>","model_id":"gpt-5"}'

curl -sS -X POST "$CTRL/admin/llm/routes" -H "$AUTH" -H 'content-type: application/json' \
  -d '{"virtual_model_id":"octra-default","routing_group_id":"<group-id>"}'
```

## 2. Create an agent virtual key

```bash
pnpm cli keys create --name claude-code --consumer <consumer-id> \
  --permissions '{"allowed_models":["octra-default"]}'
```

The response contains `key_value` exactly once. Store it in the agent's
environment or settings, never in git.

Two different credentials exist:

- `control.token` is the admin token for the control plane on port 8081. Never
  give it to an agent.
- `octra-...` virtual key is the data-plane credential for port 8080. Give this
  one to agents.

Upstream provider keys stay on the provider/account as `env:` or `file:` secret
references and never reach the agent.

Creating the first live virtual key closes anonymous local access; every agent
must then send a key.

## 3. Agent templates

### Claude Code

Copy `claude-code/settings.json` into `~/.claude/settings.json` (or merge the
`env` block). It uses the Anthropic wire format (`/v1/messages`).

- `ANTHROPIC_BASE_URL` is the gateway origin, without `/v1`.
- The gateway accepts `Authorization: Bearer` and `x-api-key`.
- Point `ANTHROPIC_MODEL` and the Opus/Sonnet/Haiku variables at routes you
  created. Variable names vary by Claude Code version.

### Codex CLI

Copy `codex/config.toml` into `~/.codex/config.toml`, then export the key:

```bash
export OCTRA_API_KEY=octra-...
```

Codex uses the OpenAI Responses API (`/v1/responses`).

### OpenCode

Copy `opencode/opencode.json` to the project root or your OpenCode config
location, then export `OCTRA_API_KEY`. It uses the OpenAI-compatible
`/v1/chat/completions` endpoint.

## 4. Rate limits and rotation

- `/v1/models` is filtered by the consumer/virtual-key `allowed_models`.
- A 429 response includes `Retry-After` and `RateLimit-Remaining`; agents should
  back off accordingly.
- `count_tokens` and `/v1/models` are not rate-limited in this release.
- Rotate keys without resetting key-level limits:

```bash
pnpm cli keys rotate <key-id> --grace-seconds 86400
```

Update the agent with the new plaintext. The old key stays valid for the grace
period and then expires. Keys in one rotation family share rate-limit counters
and token/cost usage.

## 5. Remote access

The templates assume the daemon listens on loopback. For remote agents, change
`gateway.listen_bind`, put the data plane behind TLS/reverse proxy, and keep the
control plane on loopback.
