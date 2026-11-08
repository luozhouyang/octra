-- OAuth state for AI-native provider accounts.
ALTER TABLE ai_provider_accounts ADD COLUMN oauth_status TEXT NOT NULL DEFAULT 'none';
ALTER TABLE ai_provider_accounts ADD COLUMN oauth_expires_at TEXT;

CREATE TABLE IF NOT EXISTS ai_provider_oauth_tokens (
    account_id    TEXT PRIMARY KEY
                  REFERENCES ai_provider_accounts(id) ON DELETE CASCADE,
    access_token  TEXT NOT NULL,
    refresh_token TEXT,
    token_type    TEXT NOT NULL DEFAULT 'Bearer',
    scope         TEXT NOT NULL DEFAULT '',
    expires_at    TEXT,
    extra_json    TEXT NOT NULL DEFAULT '{}',
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_ai_oauth_tokens_expires
    ON ai_provider_oauth_tokens(expires_at);

-- No backward compatibility: legacy model/routing/account tables are removed.
DROP TABLE IF EXISTS llm_provider_oauth_tokens;
DROP TABLE IF EXISTS llm_routing_targets;
DROP TABLE IF EXISTS llm_routes;
DROP TABLE IF EXISTS llm_routing_groups;
DROP TABLE IF EXISTS llm_models;
DROP TABLE IF EXISTS llm_provider_accounts;
DROP TABLE IF EXISTS llm_providers;
