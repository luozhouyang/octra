-- Shared counters for LLM rate limiting and budget guards.
-- Rows are short-lived; retention maintenance removes expired windows.

CREATE TABLE IF NOT EXISTS llm_counters (
    key          TEXT NOT NULL,
    window_start INTEGER NOT NULL,
    value        INTEGER NOT NULL,
    window_end   INTEGER NOT NULL,
    PRIMARY KEY (key, window_start)
);

CREATE INDEX IF NOT EXISTS idx_llm_counters_end ON llm_counters(window_end);

CREATE TABLE IF NOT EXISTS llm_budget_windows (
    key        TEXT PRIMARY KEY,
    ends_at_ms INTEGER NOT NULL,
    requests   INTEGER NOT NULL,
    tokens     INTEGER NOT NULL
);
