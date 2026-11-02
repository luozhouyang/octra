-- OAuth credentials for provider accounts.
--
-- Tokens live in their own table so account list/detail queries never read
-- them. Tokens are stored as plaintext for now; backups of the workspace
-- database therefore contain live credentials.

ALTER TABLE llm_provider_accounts ADD COLUMN credential_kind TEXT NOT NULL DEFAULT 'static';
ALTER TABLE llm_provider_accounts ADD COLUMN oauth_status TEXT NOT NULL DEFAULT 'none';
ALTER TABLE llm_provider_accounts ADD COLUMN oauth_expires_at TEXT;

CREATE TABLE IF NOT EXISTS llm_provider_oauth_tokens (
    account_id    TEXT PRIMARY KEY
                  REFERENCES llm_provider_accounts(id) ON DELETE CASCADE,
    access_token  TEXT NOT NULL,
    refresh_token TEXT,
    token_type    TEXT NOT NULL DEFAULT 'Bearer',
    scope         TEXT NOT NULL DEFAULT '',
    expires_at    TEXT,
    extra_json    TEXT NOT NULL DEFAULT '{}',
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_llm_oauth_tokens_expires
    ON llm_provider_oauth_tokens(expires_at);
