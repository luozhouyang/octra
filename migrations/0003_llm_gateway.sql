-- LLM gateway: protocol, routing, virtual keys and usage extensions.

PRAGMA foreign_keys = ON;

ALTER TABLE llm_providers ADD COLUMN protocol TEXT NOT NULL DEFAULT 'openai';
ALTER TABLE llm_providers ADD COLUMN auth_type TEXT NOT NULL DEFAULT 'bearer';
ALTER TABLE llm_providers ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
ALTER TABLE llm_providers ADD COLUMN config_json TEXT NOT NULL DEFAULT '{}';

ALTER TABLE llm_models ADD COLUMN display_name TEXT;
ALTER TABLE llm_models ADD COLUMN protocol TEXT;
ALTER TABLE llm_models ADD COLUMN context_window INTEGER;
ALTER TABLE llm_models ADD COLUMN max_output_tokens INTEGER;
ALTER TABLE llm_models ADD COLUMN pricing_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE llm_models ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;

CREATE TABLE IF NOT EXISTS llm_routing_targets (
    id          TEXT PRIMARY KEY,
    group_id    TEXT NOT NULL REFERENCES llm_routing_groups(id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL REFERENCES llm_providers(id) ON DELETE CASCADE,
    model_id    TEXT NOT NULL,
    weight      INTEGER NOT NULL DEFAULT 1,
    priority    INTEGER NOT NULL DEFAULT 0,
    position    INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_llm_routing_targets_group ON llm_routing_targets(group_id);
CREATE INDEX IF NOT EXISTS idx_llm_routing_targets_provider ON llm_routing_targets(provider_id);

ALTER TABLE llm_virtual_keys ADD COLUMN key_prefix TEXT NOT NULL DEFAULT '';
ALTER TABLE llm_virtual_keys ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
ALTER TABLE llm_virtual_keys ADD COLUMN expires_at TEXT;
ALTER TABLE llm_virtual_keys ADD COLUMN last_used_at TEXT;
ALTER TABLE llm_virtual_keys ADD COLUMN permissions_json TEXT NOT NULL DEFAULT '{}';

ALTER TABLE llm_usage ADD COLUMN request_id TEXT NOT NULL DEFAULT '';
ALTER TABLE llm_usage ADD COLUMN api_key_id TEXT;
ALTER TABLE llm_usage ADD COLUMN status_code INTEGER;
ALTER TABLE llm_usage ADD COLUMN latency_ms INTEGER;
ALTER TABLE llm_usage ADD COLUMN error TEXT;
ALTER TABLE llm_usage ADD COLUMN protocol TEXT;

CREATE INDEX IF NOT EXISTS idx_llm_usage_request ON llm_usage(request_id);
CREATE INDEX IF NOT EXISTS idx_llm_usage_api_key ON llm_usage(api_key_id);
