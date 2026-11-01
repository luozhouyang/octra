-- LLM usage: account attribution, cache/reasoning tokens and cost.

ALTER TABLE llm_usage ADD COLUMN account_id TEXT;
ALTER TABLE llm_usage ADD COLUMN account_name TEXT;
ALTER TABLE llm_usage ADD COLUMN cache_read_tokens INTEGER;
ALTER TABLE llm_usage ADD COLUMN cache_write_tokens INTEGER;
ALTER TABLE llm_usage ADD COLUMN reasoning_tokens INTEGER;
ALTER TABLE llm_usage ADD COLUMN cost_micros INTEGER;
ALTER TABLE llm_usage ADD COLUMN cost_currency TEXT;
ALTER TABLE llm_usage ADD COLUMN finish_reason TEXT;

CREATE INDEX IF NOT EXISTS idx_llm_usage_account ON llm_usage(account_id);
