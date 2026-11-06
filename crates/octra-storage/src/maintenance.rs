//! Data retention and usage rollup maintenance.

use chrono::{Duration, Utc};
use octra_core::{Error, Result};

use crate::Storage;
use crate::llm::canonical_timestamp;

/// Retention windows in days. `0` keeps rows forever.
#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    pub llm_usage_days: u32,
    pub request_logs_days: u32,
    pub mcp_invocations_days: u32,
    pub audit_logs_days: u32,
    pub usage_rollup_days: u32,
    /// Completed and failed plugin events. Pending events are never pruned.
    pub plugin_events_days: u32,
    pub batch_size: u32,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PruneReport {
    pub llm_usage: u64,
    pub request_logs: u64,
    pub mcp_invocations: u64,
    pub audit_logs: u64,
    pub usage_rollup: u64,
    pub plugin_events: u64,
}

const ROLLUP_MARKER: &str = "usage_rollup_v1";

impl Storage {
    /// Deletes rows older than the configured windows. Deletion is batched so
    /// a large first pass does not hold one long write transaction.
    pub async fn prune(&self, policy: &RetentionPolicy) -> Result<PruneReport> {
        let llm_usage = self
            .prune_table(
                "llm_usage",
                "created_at",
                policy.llm_usage_days,
                policy.batch_size,
            )
            .await?;
        let request_logs = self
            .prune_table(
                "request_logs",
                "created_at",
                policy.request_logs_days,
                policy.batch_size,
            )
            .await?;
        let audit_logs = self
            .prune_table(
                "audit_logs",
                "created_at",
                policy.audit_logs_days,
                policy.batch_size,
            )
            .await?;
        let mcp_invocations = self
            .prune_table(
                "mcp_invocations",
                "started_at",
                policy.mcp_invocations_days,
                policy.batch_size,
            )
            .await?;
        let usage_rollup = self.prune_rollup(policy.usage_rollup_days).await?;
        let plugin_events = if policy.plugin_events_days == 0 {
            0
        } else {
            let days = i64::from(policy.plugin_events_days);
            let cutoff = Utc::now()
                .timestamp()
                .saturating_sub(days.saturating_mul(86_400));
            self.prune_events(cutoff, policy.batch_size).await?
        };
        if let Err(error) = self.llm.prune_llm_counters(Utc::now().timestamp()).await {
            tracing::debug!(%error, "LLM counter prune failed");
        }
        let report = PruneReport {
            llm_usage,
            request_logs,
            audit_logs,
            mcp_invocations,
            usage_rollup,
            plugin_events,
        };

        // Both are best-effort: a busy checkpoint or stale statistics must not
        // fail the retention pass.
        if let Err(error) = sqlx::query("PRAGMA optimize").execute(self.pool()).await {
            tracing::debug!(%error, "PRAGMA optimize failed");
        }
        if let Err(error) = sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
            .execute(self.pool())
            .await
        {
            tracing::debug!(%error, "wal checkpoint failed");
        }
        Ok(report)
    }

    /// Rebuilds the hourly usage rollup from `llm_usage` in one transaction.
    /// This is the backfill path for databases created before the rollup.
    pub async fn rebuild_usage_rollup(&self) -> Result<u64> {
        let started = std::time::Instant::now();
        let mut tx = self.pool().begin().await.map_err(backend)?;
        sqlx::query("DELETE FROM llm_usage_rollup")
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let result = sqlx::query(
            "INSERT INTO llm_usage_rollup (bucket_start, api_key_id, account_id, provider_id,
                 model_id, virtual_model_id, requests, errors, input_tokens, output_tokens,
                 cache_write_tokens, cost_micros)
             SELECT substr(created_at, 1, 13) || ':00:00.000Z',
                 COALESCE(api_key_id, ''), COALESCE(account_id, ''),
                 COALESCE(provider_id, ''), COALESCE(model_id, ''), virtual_model_id,
                 COUNT(*),
                 SUM(CASE WHEN error IS NOT NULL OR COALESCE(status_code, 0) >= 400
                          THEN 1 ELSE 0 END),
                 SUM(input_tokens), SUM(output_tokens),
                 SUM(COALESCE(cache_write_tokens, 0)), SUM(COALESCE(cost_micros, 0))
             FROM llm_usage
             GROUP BY 1, 2, 3, 4, 5, 6",
        )
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        let rows = result.rows_affected();
        tracing::info!(
            rows,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "rebuilt llm usage rollup"
        );
        Ok(rows)
    }

    /// Builds the rollup once on first start. Later starts skip the scan.
    pub async fn ensure_usage_rollup(&self) -> Result<()> {
        if self.get_setting(ROLLUP_MARKER).await?.is_some() {
            return Ok(());
        }
        let rows = self.rebuild_usage_rollup().await?;
        self.set_setting(ROLLUP_MARKER, &rows.to_string()).await?;
        Ok(())
    }

    async fn prune_table(
        &self,
        table: &'static str,
        timestamp: &'static str,
        days: u32,
        batch_size: u32,
    ) -> Result<u64> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = cutoff_timestamp(days);
        let batch = i64::from(batch_size.max(1));
        let sql = format!(
            "DELETE FROM {table} WHERE id IN (
                 SELECT id FROM {table} WHERE {timestamp} < ?1
                 ORDER BY {timestamp} LIMIT ?2
             )"
        );
        let mut total = 0u64;
        loop {
            let result = sqlx::query(&sql)
                .bind(&cutoff)
                .bind(batch)
                .execute(self.pool())
                .await
                .map_err(backend)?;
            let deleted = result.rows_affected();
            total = total.saturating_add(deleted);
            if deleted < batch as u64 {
                break;
            }
        }
        Ok(total)
    }

    async fn prune_rollup(&self, days: u32) -> Result<u64> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = cutoff_timestamp(days);
        let result = sqlx::query("DELETE FROM llm_usage_rollup WHERE bucket_start < ?")
            .bind(&cutoff)
            .execute(self.pool())
            .await
            .map_err(backend)?;
        Ok(result.rows_affected())
    }

    async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(self.pool())
            .await
            .map_err(backend)
    }

    async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET
                 value = excluded.value,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
        )
        .bind(key)
        .bind(value)
        .execute(self.pool())
        .await
        .map_err(backend)?;
        Ok(())
    }
}

fn cutoff_timestamp(days: u32) -> String {
    canonical_timestamp(Utc::now() - Duration::days(i64::from(days)))
}

fn backend(error: sqlx::Error) -> Error {
    Error::Storage(format!("sqlite: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmUsageInput;
    use sqlx::Row;

    fn policy(days: u32, batch_size: u32) -> RetentionPolicy {
        RetentionPolicy {
            llm_usage_days: days,
            request_logs_days: days,
            mcp_invocations_days: days,
            audit_logs_days: days,
            usage_rollup_days: days,
            plugin_events_days: days,
            batch_size,
        }
    }

    fn days_ago(days: i64) -> String {
        canonical_timestamp(Utc::now() - Duration::days(days))
    }

    async fn insert_usage(storage: &Storage, request_id: &str, created_at: &str) {
        storage
            .llm
            .insert_usage(&LlmUsageInput {
                request_id: request_id.into(),
                virtual_model_id: "octra-test".into(),
                input_tokens: 10,
                output_tokens: 5,
                created_at: Some(created_at.into()),
                ..LlmUsageInput::default()
            })
            .await
            .unwrap();
    }

    async fn row_count(storage: &Storage, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(storage.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn prunes_rows_past_retention_in_batches() {
        let storage = Storage::connect_in_memory().await.unwrap();
        let old = days_ago(40);
        let new = days_ago(1);

        insert_usage(&storage, "old-usage", &old).await;
        insert_usage(&storage, "new-usage", &new).await;
        for (request_id, timestamp) in [("old-log", &old), ("new-log", &new)] {
            sqlx::query(
                "INSERT INTO request_logs (request_id, method, path, status, created_at)
                 VALUES (?, 'GET', '/', 200, ?)",
            )
            .bind(request_id)
            .bind(timestamp)
            .execute(storage.pool())
            .await
            .unwrap();
        }
        for (target, timestamp) in [("old-audit", &old), ("new-audit", &new)] {
            sqlx::query(
                "INSERT INTO audit_logs (actor, action, target, detail, created_at)
                 VALUES ('test', 'test', ?, '{}', ?)",
            )
            .bind(target)
            .bind(timestamp)
            .execute(storage.pool())
            .await
            .unwrap();
        }
        for (tool_id, timestamp) in [("old-invocation", &old), ("new-invocation", &new)] {
            sqlx::query(
                "INSERT INTO mcp_invocations (server_id, tool_id, status, started_at)
                 VALUES ('server', ?, 'ok', ?)",
            )
            .bind(tool_id)
            .bind(timestamp)
            .execute(storage.pool())
            .await
            .unwrap();
        }

        let now = Utc::now().timestamp();
        let old_seconds = now - 40 * 86_400;
        let new_seconds = now - 86_400;
        for (id, status, created_at) in [
            ("old-completed", "completed", old_seconds),
            ("old-failed", "failed", old_seconds),
            ("old-pending", "pending", old_seconds),
            ("new-completed", "completed", new_seconds),
        ] {
            sqlx::query(
                "INSERT INTO events
                    (id, topic, source, payload_json, status, attempts, max_attempts,
                     next_attempt_at, created_at)
                 VALUES (?, 'test.topic', 'test', '{}', ?, 0, 5, 0, ?)",
            )
            .bind(id)
            .bind(status)
            .bind(created_at)
            .execute(storage.pool())
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO event_deliveries (event_id, subscriber_id, plugin_name,
                 plugin_instance_id, status, attempts, max_attempts, next_attempt_at)
             VALUES ('old-completed', 'sub', 'plugin', 'instance', 'delivered', 1, 5, 0)",
        )
        .execute(storage.pool())
        .await
        .unwrap();

        let report = storage.prune(&policy(30, 1)).await.unwrap();
        assert_eq!(report.llm_usage, 1);
        assert_eq!(report.request_logs, 1);
        assert_eq!(report.audit_logs, 1);
        assert_eq!(report.mcp_invocations, 1);
        assert_eq!(report.usage_rollup, 1);
        assert_eq!(report.plugin_events, 2);

        assert_eq!(row_count(&storage, "llm_usage").await, 1);
        assert_eq!(row_count(&storage, "request_logs").await, 1);
        assert_eq!(row_count(&storage, "audit_logs").await, 1);
        assert_eq!(row_count(&storage, "mcp_invocations").await, 1);
        assert_eq!(row_count(&storage, "llm_usage_rollup").await, 1);
        assert_eq!(row_count(&storage, "events").await, 2);
        assert_eq!(row_count(&storage, "event_deliveries").await, 0);
    }

    #[tokio::test]
    async fn zero_days_keeps_everything() {
        let storage = Storage::connect_in_memory().await.unwrap();
        insert_usage(&storage, "old-usage", &days_ago(400)).await;
        sqlx::query(
            "INSERT INTO events
                (id, topic, source, payload_json, status, attempts, max_attempts,
                 next_attempt_at, created_at)
             VALUES ('old-event', 'test.topic', 'test', '{}', 'completed', 0, 5, 0, ?)",
        )
        .bind(Utc::now().timestamp() - 400 * 86_400)
        .execute(storage.pool())
        .await
        .unwrap();

        let report = storage.prune(&policy(0, 100)).await.unwrap();
        assert_eq!(report, PruneReport::default());
        assert_eq!(row_count(&storage, "llm_usage").await, 1);
        assert_eq!(row_count(&storage, "llm_usage_rollup").await, 1);
        assert_eq!(row_count(&storage, "events").await, 1);
    }

    #[tokio::test]
    async fn rebuild_matches_raw_usage() {
        let storage = Storage::connect_in_memory().await.unwrap();
        insert_usage(&storage, "one", "2026-10-01T05:12:00.000Z").await;
        insert_usage(&storage, "two", "2026-10-01T05:42:00.000Z").await;
        insert_usage(&storage, "three", "2026-10-01T06:01:00.000Z").await;

        sqlx::query("DELETE FROM llm_usage_rollup")
            .execute(storage.pool())
            .await
            .unwrap();
        let rebuilt = storage.rebuild_usage_rollup().await.unwrap();
        assert_eq!(rebuilt, 2);

        let raw = sqlx::query(
            "SELECT COUNT(*) AS requests,
                 COALESCE(SUM(input_tokens + output_tokens + COALESCE(cache_write_tokens, 0)), 0)
                     AS tokens
             FROM llm_usage",
        )
        .fetch_one(storage.pool())
        .await
        .unwrap();
        let rollup = sqlx::query(
            "SELECT COALESCE(SUM(requests), 0) AS requests,
                 COALESCE(SUM(input_tokens + output_tokens + cache_write_tokens), 0) AS tokens
             FROM llm_usage_rollup",
        )
        .fetch_one(storage.pool())
        .await
        .unwrap();
        assert_eq!(
            raw.get::<i64, _>("requests"),
            rollup.get::<i64, _>("requests")
        );
        assert_eq!(raw.get::<i64, _>("tokens"), rollup.get::<i64, _>("tokens"));
        assert_eq!(raw.get::<i64, _>("tokens"), 45);
    }
}
