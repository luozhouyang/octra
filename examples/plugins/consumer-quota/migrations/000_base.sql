-- 000_base: per-consumer fixed-window quota counters.
-- Applied once by the host during install/upgrade; never edited after publish.

CREATE TABLE IF NOT EXISTS quota_usage (
    consumer_id  TEXT    NOT NULL,
    window_start INTEGER NOT NULL,
    count        INTEGER NOT NULL DEFAULT 0,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (consumer_id, window_start)
);

CREATE INDEX IF NOT EXISTS idx_quota_usage_window
    ON quota_usage(window_start);
