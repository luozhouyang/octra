-- LLM provider accounts: multiple credentials per provider with optional quota.

CREATE TABLE IF NOT EXISTS llm_provider_accounts (
    id             TEXT PRIMARY KEY,
    provider_id    TEXT NOT NULL REFERENCES llm_providers(id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    secret_ref     TEXT,
    weight         INTEGER NOT NULL DEFAULT 1,
    priority       INTEGER NOT NULL DEFAULT 0,
    enabled        INTEGER NOT NULL DEFAULT 1,
    quota_limit    INTEGER,
    quota_unit     TEXT NOT NULL DEFAULT 'requests',
    quota_period   TEXT NOT NULL DEFAULT 'none',
    quota_used     INTEGER NOT NULL DEFAULT 0,
    quota_reset_at TEXT,
    cooldown_until TEXT,
    last_error     TEXT,
    config_json    TEXT NOT NULL DEFAULT '{}',
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (provider_id, name)
);
CREATE INDEX IF NOT EXISTS idx_llm_provider_accounts_provider
    ON llm_provider_accounts(provider_id);

ALTER TABLE llm_routing_targets ADD COLUMN account_id TEXT
    REFERENCES llm_provider_accounts(id) ON DELETE SET NULL;
