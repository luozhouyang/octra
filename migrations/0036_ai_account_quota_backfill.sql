-- Existing AI accounts predate quota_reset_at. Reset their counters into a
-- fresh period so an upgrade cannot carry essentially unbounded stale usage;
-- AccountRuntime persists the next boundary on the following rollover.
UPDATE ai_provider_accounts
   SET quota_used = 0,
       quota_reset_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
 WHERE quota_period IN ('day', 'month');
