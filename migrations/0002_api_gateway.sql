-- API Gateway extensions.
-- Adds per-service policies, route ordering/rewrite, target toggles, and
-- hashed API credentials. SQLite ALTER TABLE keeps existing rows valid.

ALTER TABLE services ADD COLUMN connect_timeout_ms INTEGER NOT NULL DEFAULT 10000;
ALTER TABLE services ADD COLUMN request_timeout_ms INTEGER NOT NULL DEFAULT 60000;
ALTER TABLE services ADD COLUMN retries INTEGER NOT NULL DEFAULT 0;
ALTER TABLE services ADD COLUMN require_auth INTEGER NOT NULL DEFAULT 1;

ALTER TABLE routes ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
ALTER TABLE routes ADD COLUMN strip_prefix INTEGER NOT NULL DEFAULT 0;

ALTER TABLE targets ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;

ALTER TABLE credentials ADD COLUMN name TEXT NOT NULL DEFAULT '';
ALTER TABLE credentials ADD COLUMN key_prefix TEXT;
ALTER TABLE credentials ADD COLUMN key_hash TEXT;
ALTER TABLE credentials ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
ALTER TABLE credentials ADD COLUMN expires_at TEXT;
ALTER TABLE credentials ADD COLUMN last_used_at TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_credentials_key_hash
    ON credentials(key_hash)
    WHERE key_hash IS NOT NULL;
