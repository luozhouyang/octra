-- Durable event outbox and plugin scheduled jobs.
--
-- Events are written before dispatch so a crash between publish and delivery
-- does not lose them. Deliveries are tracked per subscriber for at-least-once
-- semantics and bounded retries.

CREATE TABLE events (
    id              TEXT PRIMARY KEY,
    topic           TEXT NOT NULL,
    source          TEXT NOT NULL,
    payload_json    TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'completed', 'failed')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    max_attempts    INTEGER NOT NULL DEFAULT 5,
    next_attempt_at INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    created_at      INTEGER NOT NULL,
    completed_at    INTEGER
);

CREATE INDEX idx_events_due ON events(status, next_attempt_at);
CREATE INDEX idx_events_topic_created ON events(topic, created_at DESC);

CREATE TABLE event_deliveries (
    event_id           TEXT NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    subscriber_id      TEXT NOT NULL,
    plugin_name        TEXT NOT NULL,
    plugin_instance_id TEXT NOT NULL,
    status             TEXT NOT NULL DEFAULT 'pending'
                       CHECK (status IN ('pending', 'delivered', 'failed')),
    attempts           INTEGER NOT NULL DEFAULT 0,
    max_attempts       INTEGER NOT NULL DEFAULT 5,
    next_attempt_at    INTEGER NOT NULL DEFAULT 0,
    last_error         TEXT,
    delivered_at       INTEGER,
    PRIMARY KEY (event_id, subscriber_id)
);

CREATE INDEX idx_event_deliveries_due
    ON event_deliveries(status, next_attempt_at);
CREATE INDEX idx_event_deliveries_instance
    ON event_deliveries(plugin_name, plugin_instance_id);

CREATE TABLE plugin_jobs (
    id                 TEXT PRIMARY KEY,
    plugin_name        TEXT NOT NULL,
    plugin_instance_id TEXT NOT NULL,
    job_name           TEXT NOT NULL,
    schedule           TEXT NOT NULL,
    enabled            INTEGER NOT NULL DEFAULT 1,
    next_run_at        INTEGER NOT NULL DEFAULT 0,
    last_run_at        INTEGER,
    last_status        TEXT,
    last_error         TEXT,
    attempts           INTEGER NOT NULL DEFAULT 0,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL,
    UNIQUE (plugin_name, plugin_instance_id, job_name)
);

CREATE INDEX idx_plugin_jobs_due
    ON plugin_jobs(enabled, next_run_at);
