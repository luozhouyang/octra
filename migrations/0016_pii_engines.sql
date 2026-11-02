-- PII detection engines managed by the LLM gateway.
--
-- External engines are user-managed services; managed engines are spawned by
-- the daemon. This migration only defines configuration and ownership, not
-- process state.

CREATE TABLE IF NOT EXISTS llm_pii_engines (
    id             TEXT PRIMARY KEY,
    name           TEXT NOT NULL UNIQUE,
    mode           TEXT NOT NULL DEFAULT 'external',
    transport      TEXT NOT NULL DEFAULT 'http_loopback',
    endpoint       TEXT,
    command        TEXT,
    args_json      TEXT NOT NULL DEFAULT '[]',
    env_allow_json TEXT NOT NULL DEFAULT '[]',
    model_dir      TEXT,
    config_json    TEXT NOT NULL DEFAULT '{}',
    enabled        INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_llm_pii_engines_enabled
    ON llm_pii_engines(enabled);
