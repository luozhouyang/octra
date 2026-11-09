-- Persist the end of the current quota period so a restart after a rollover
-- resets the counter instead of carrying stale usage into the next period.
ALTER TABLE ai_provider_accounts ADD COLUMN quota_reset_at TEXT;
