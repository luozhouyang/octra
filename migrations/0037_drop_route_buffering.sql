-- Baggage from the pre-Kong plugin model. Body buffering is now decided by
-- body_filter plugins, not by route-wide flags.
ALTER TABLE routes DROP COLUMN request_buffering;
ALTER TABLE routes DROP COLUMN response_buffering;
