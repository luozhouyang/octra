-- Speeds up retention pruning and status-filtered event listings.
CREATE INDEX idx_events_status_created ON events(status, created_at);
