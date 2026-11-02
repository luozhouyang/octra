-- Hourly LLM usage rollups and composite indexes for time-window queries.
--
-- Dimensions use '' instead of NULL because SQLite treats NULL values as
-- distinct in UNIQUE constraints, which would defeat the UPSERT below.

CREATE TABLE IF NOT EXISTS llm_usage_rollup (
    bucket_start       TEXT    NOT NULL,
    api_key_id         TEXT    NOT NULL DEFAULT '',
    account_id         TEXT    NOT NULL DEFAULT '',
    provider_id        TEXT    NOT NULL DEFAULT '',
    model_id           TEXT    NOT NULL DEFAULT '',
    virtual_model_id   TEXT    NOT NULL DEFAULT '',
    requests           INTEGER NOT NULL DEFAULT 0,
    errors             INTEGER NOT NULL DEFAULT 0,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    cost_micros        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket_start, api_key_id, account_id, provider_id, model_id, virtual_model_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_rollup_api_key
    ON llm_usage_rollup(api_key_id, bucket_start);
CREATE INDEX IF NOT EXISTS idx_rollup_account
    ON llm_usage_rollup(account_id, bucket_start);
CREATE INDEX IF NOT EXISTS idx_rollup_vmodel
    ON llm_usage_rollup(virtual_model_id, bucket_start);
CREATE INDEX IF NOT EXISTS idx_rollup_provider
    ON llm_usage_rollup(provider_id, bucket_start);
CREATE INDEX IF NOT EXISTS idx_rollup_model
    ON llm_usage_rollup(model_id, bucket_start);

CREATE INDEX IF NOT EXISTS idx_llm_usage_api_key_created
    ON llm_usage(api_key_id, created_at);
CREATE INDEX IF NOT EXISTS idx_llm_usage_account_created
    ON llm_usage(account_id, created_at);
CREATE INDEX IF NOT EXISTS idx_llm_usage_vmodel_created
    ON llm_usage(virtual_model_id, created_at);

-- The composite indexes above cover these single-column indexes.
DROP INDEX IF EXISTS idx_llm_usage_api_key;
DROP INDEX IF EXISTS idx_llm_usage_account;
