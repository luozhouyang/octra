-- Request access logs for the API data plane.
-- Audit rows are written to the existing audit_logs table.

CREATE TABLE IF NOT EXISTS request_logs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id  TEXT NOT NULL,
    method      TEXT NOT NULL,
    path        TEXT NOT NULL,
    status      INTEGER NOT NULL,
    latency_ms  INTEGER NOT NULL DEFAULT 0,
    route_id    TEXT,
    service_id  TEXT,
    target_url  TEXT,
    consumer_id TEXT,
    error       TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_request_logs_created ON request_logs(created_at);
CREATE INDEX IF NOT EXISTS idx_request_logs_request ON request_logs(request_id);
CREATE INDEX IF NOT EXISTS idx_request_logs_route ON request_logs(route_id);
