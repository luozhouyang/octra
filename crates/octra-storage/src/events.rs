//! Durable event outbox and plugin job schedule repositories.

use octra_core::{Error, Result};
use serde::Serialize;
use sqlx::Row;

use super::Storage;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventRecord {
    pub id: String,
    pub topic: String,
    pub source: String,
    pub payload_json: String,
    pub status: String,
    pub attempts: i64,
    pub max_attempts: i64,
    pub next_attempt_at: i64,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub completed_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventDeliveryRecord {
    pub event_id: String,
    pub subscriber_id: String,
    pub plugin_name: String,
    pub plugin_instance_id: String,
    pub status: String,
    pub attempts: i64,
    pub max_attempts: i64,
    pub next_attempt_at: i64,
    pub last_error: Option<String>,
    pub delivered_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobRecord {
    pub id: String,
    pub plugin_name: String,
    pub plugin_instance_id: String,
    pub job_name: String,
    pub schedule: String,
    pub enabled: bool,
    pub next_run_at: i64,
    pub last_run_at: Option<i64>,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    pub attempts: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub topic: String,
    pub source: String,
    pub payload_json: String,
    pub max_attempts: i64,
}

#[derive(Debug, Clone)]
pub struct NewJob {
    pub id: String,
    pub plugin_name: String,
    pub plugin_instance_id: String,
    pub job_name: String,
    pub schedule: String,
    pub next_run_at: i64,
}

const EVENT_COLUMNS: &str = "id, topic, source, payload_json, status, attempts, max_attempts, \
     next_attempt_at, last_error, created_at, completed_at";

const DELIVERY_COLUMNS: &str = "event_id, subscriber_id, plugin_name, plugin_instance_id, \
     status, attempts, max_attempts, next_attempt_at, last_error, delivered_at";

const JOB_COLUMNS: &str = "id, plugin_name, plugin_instance_id, job_name, schedule, enabled, \
     next_run_at, last_run_at, last_status, last_error, attempts, created_at, updated_at";

fn event_from_row(row: &sqlx::sqlite::SqliteRow) -> EventRecord {
    EventRecord {
        id: row.get("id"),
        topic: row.get("topic"),
        source: row.get("source"),
        payload_json: row.get("payload_json"),
        status: row.get("status"),
        attempts: row.get("attempts"),
        max_attempts: row.get("max_attempts"),
        next_attempt_at: row.get("next_attempt_at"),
        last_error: row.get("last_error"),
        created_at: row.get("created_at"),
        completed_at: row.get("completed_at"),
    }
}

fn delivery_from_row(row: &sqlx::sqlite::SqliteRow) -> EventDeliveryRecord {
    EventDeliveryRecord {
        event_id: row.get("event_id"),
        subscriber_id: row.get("subscriber_id"),
        plugin_name: row.get("plugin_name"),
        plugin_instance_id: row.get("plugin_instance_id"),
        status: row.get("status"),
        attempts: row.get("attempts"),
        max_attempts: row.get("max_attempts"),
        next_attempt_at: row.get("next_attempt_at"),
        last_error: row.get("last_error"),
        delivered_at: row.get("delivered_at"),
    }
}

fn job_from_row(row: &sqlx::sqlite::SqliteRow) -> JobRecord {
    JobRecord {
        id: row.get("id"),
        plugin_name: row.get("plugin_name"),
        plugin_instance_id: row.get("plugin_instance_id"),
        job_name: row.get("job_name"),
        schedule: row.get("schedule"),
        enabled: row.get::<i64, _>("enabled") != 0,
        next_run_at: row.get("next_run_at"),
        last_run_at: row.get("last_run_at"),
        last_status: row.get("last_status"),
        last_error: row.get("last_error"),
        attempts: row.get("attempts"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn map_error(context: &str, error: sqlx::Error) -> Error {
    Error::Storage(format!("{context}: {error}"))
}

impl Storage {
    pub async fn insert_event(&self, event: &NewEvent) -> Result<EventRecord> {
        let id = format!("evt_{}", uuid::Uuid::new_v4().simple());
        let now = chrono::Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO events
                (id, topic, source, payload_json, status, attempts, max_attempts,
                 next_attempt_at, created_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', 0, ?5, ?6, ?6)",
        )
        .bind(&id)
        .bind(&event.topic)
        .bind(&event.source)
        .bind(&event.payload_json)
        .bind(event.max_attempts.max(1))
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("cannot insert event", error))?;

        self.get_event(&id)
            .await?
            .ok_or_else(|| Error::Storage("event vanished after insert".to_string()))
    }

    pub async fn get_event(&self, id: &str) -> Result<Option<EventRecord>> {
        let row = sqlx::query(&format!("SELECT {EVENT_COLUMNS} FROM events WHERE id = ?1"))
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(|error| map_error("cannot load event", error))?;
        Ok(row.as_ref().map(event_from_row))
    }

    pub async fn list_events(&self, limit: u32, status: Option<&str>) -> Result<Vec<EventRecord>> {
        let rows = sqlx::query(&format!(
            "SELECT {EVENT_COLUMNS} FROM events
              WHERE (?1 IS NULL OR status = ?1)
              ORDER BY created_at DESC
              LIMIT ?2"
        ))
        .bind(status)
        .bind(i64::from(limit.clamp(1, 500)))
        .fetch_all(self.pool())
        .await
        .map_err(|error| map_error("cannot list events", error))?;
        Ok(rows.iter().map(event_from_row).collect())
    }

    pub async fn list_due_events(&self, now: i64, limit: u32) -> Result<Vec<EventRecord>> {
        let rows = sqlx::query(&format!(
            "SELECT {EVENT_COLUMNS} FROM events
              WHERE status = 'pending' AND next_attempt_at <= ?1
              ORDER BY created_at ASC
              LIMIT ?2"
        ))
        .bind(now)
        .bind(i64::from(limit.clamp(1, 500)))
        .fetch_all(self.pool())
        .await
        .map_err(|error| map_error("cannot list due events", error))?;
        Ok(rows.iter().map(event_from_row).collect())
    }

    /// Adds a delivery row unless this subscriber already has one for the
    /// event. Returns true when a new row was inserted.
    pub async fn ensure_event_delivery(
        &self,
        event_id: &str,
        subscriber_id: &str,
        plugin_name: &str,
        plugin_instance_id: &str,
        max_attempts: i64,
        now: i64,
    ) -> Result<bool> {
        let result = sqlx::query(
            "INSERT OR IGNORE INTO event_deliveries
                (event_id, subscriber_id, plugin_name, plugin_instance_id,
                 status, attempts, max_attempts, next_attempt_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', 0, ?5, ?6)",
        )
        .bind(event_id)
        .bind(subscriber_id)
        .bind(plugin_name)
        .bind(plugin_instance_id)
        .bind(max_attempts.max(1))
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("cannot insert event delivery", error))?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn list_event_deliveries(&self, event_id: &str) -> Result<Vec<EventDeliveryRecord>> {
        let rows = sqlx::query(&format!(
            "SELECT {DELIVERY_COLUMNS} FROM event_deliveries
              WHERE event_id = ?1 ORDER BY subscriber_id ASC"
        ))
        .bind(event_id)
        .fetch_all(self.pool())
        .await
        .map_err(|error| map_error("cannot list event deliveries", error))?;
        Ok(rows.iter().map(delivery_from_row).collect())
    }

    pub async fn list_due_event_deliveries(
        &self,
        event_id: &str,
        now: i64,
        limit: u32,
    ) -> Result<Vec<EventDeliveryRecord>> {
        let rows = sqlx::query(&format!(
            "SELECT {DELIVERY_COLUMNS} FROM event_deliveries
              WHERE event_id = ?1 AND status = 'pending' AND next_attempt_at <= ?2
              ORDER BY subscriber_id ASC
              LIMIT ?3"
        ))
        .bind(event_id)
        .bind(now)
        .bind(i64::from(limit.clamp(1, 500)))
        .fetch_all(self.pool())
        .await
        .map_err(|error| map_error("cannot list due deliveries", error))?;
        Ok(rows.iter().map(delivery_from_row).collect())
    }

    pub async fn mark_event_delivery_delivered(
        &self,
        event_id: &str,
        subscriber_id: &str,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE event_deliveries
                SET status = 'delivered', attempts = attempts + 1,
                    delivered_at = ?3, last_error = NULL
              WHERE event_id = ?1 AND subscriber_id = ?2",
        )
        .bind(event_id)
        .bind(subscriber_id)
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("cannot mark delivery delivered", error))?;
        Ok(())
    }

    pub async fn mark_event_delivery_retry(
        &self,
        event_id: &str,
        subscriber_id: &str,
        next_attempt_at: i64,
        error: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE event_deliveries
                SET status = 'pending', attempts = attempts + 1,
                    next_attempt_at = ?3, last_error = ?4
              WHERE event_id = ?1 AND subscriber_id = ?2",
        )
        .bind(event_id)
        .bind(subscriber_id)
        .bind(next_attempt_at)
        .bind(error)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("cannot schedule delivery retry", error))?;
        Ok(())
    }

    pub async fn mark_event_delivery_failed(
        &self,
        event_id: &str,
        subscriber_id: &str,
        error: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE event_deliveries
                SET status = 'failed', attempts = attempts + 1, last_error = ?3
              WHERE event_id = ?1 AND subscriber_id = ?2",
        )
        .bind(event_id)
        .bind(subscriber_id)
        .bind(error)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("cannot mark delivery failed", error))?;
        Ok(())
    }

    /// Recomputes the event status from its deliveries. Pending deliveries
    /// keep the event pending; otherwise any failed delivery marks it failed.
    pub async fn refresh_event_status(&self, event_id: &str, now: i64) -> Result<()> {
        let row = sqlx::query(
            "SELECT
                COUNT(*) AS total,
                COALESCE(SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END), 0) AS pending,
                COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0) AS failed
               FROM event_deliveries WHERE event_id = ?1",
        )
        .bind(event_id)
        .fetch_one(self.pool())
        .await
        .map_err(|error| map_error("cannot summarize deliveries", error))?;
        let pending: i64 = row.get("pending");
        let failed: i64 = row.get("failed");
        let total: i64 = row.get("total");

        if pending > 0 {
            sqlx::query("UPDATE events SET status = 'pending' WHERE id = ?1")
                .bind(event_id)
                .execute(self.pool())
                .await
                .map_err(|error| map_error("cannot keep event pending", error))?;
            return Ok(());
        }

        let status = if total > 0 && failed > 0 {
            "failed"
        } else {
            "completed"
        };
        sqlx::query(
            "UPDATE events SET status = ?2, completed_at = ?3, last_error = (
                SELECT last_error FROM event_deliveries
                 WHERE event_id = ?1 AND last_error IS NOT NULL
                 ORDER BY subscriber_id ASC LIMIT 1
             ) WHERE id = ?1",
        )
        .bind(event_id)
        .bind(status)
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("cannot finish event", error))?;
        Ok(())
    }

    /// Marks an event complete when it has no eligible subscribers.
    pub async fn complete_event_without_deliveries(&self, event_id: &str, now: i64) -> Result<()> {
        sqlx::query(
            "UPDATE events SET status = 'completed', completed_at = ?2
              WHERE id = ?1 AND status = 'pending'",
        )
        .bind(event_id)
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("cannot complete event", error))?;
        Ok(())
    }

    /// Moves a pending event to the earliest pending delivery retry so the
    /// dispatcher does not revisit it on every tick.
    pub async fn reschedule_pending_event(&self, event_id: &str, now: i64) -> Result<()> {
        sqlx::query(
            "UPDATE events
                SET next_attempt_at = COALESCE((
                    SELECT MIN(next_attempt_at) FROM event_deliveries
                     WHERE event_id = ?1 AND status = 'pending'
                ), ?2)
              WHERE id = ?1 AND status = 'pending'",
        )
        .bind(event_id)
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("cannot reschedule event", error))?;
        Ok(())
    }

    /// Deletes completed or failed events older than `before`.
    pub async fn prune_events(&self, before: i64) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM events
              WHERE status IN ('completed', 'failed') AND created_at < ?1",
        )
        .bind(before)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("cannot prune events", error))?;
        Ok(result.rows_affected())
    }

    /// Reconciles the persisted job rows with the currently enabled plugin
    /// declarations. Removed jobs are disabled but retained for history.
    pub async fn sync_jobs(&self, jobs: &[NewJob], now: i64) -> Result<()> {
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| map_error("cannot begin job sync", error))?;

        sqlx::query("UPDATE plugin_jobs SET enabled = 0, updated_at = ?1 WHERE enabled = 1")
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_error("cannot disable stale jobs", error))?;

        for job in jobs {
            sqlx::query(
                "INSERT INTO plugin_jobs
                    (id, plugin_name, plugin_instance_id, job_name, schedule, enabled,
                     next_run_at, attempts, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, 0, ?7, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                    plugin_name = excluded.plugin_name,
                    plugin_instance_id = excluded.plugin_instance_id,
                    job_name = excluded.job_name,
                    schedule = excluded.schedule,
                    enabled = 1,
                    updated_at = excluded.updated_at",
            )
            .bind(&job.id)
            .bind(&job.plugin_name)
            .bind(&job.plugin_instance_id)
            .bind(&job.job_name)
            .bind(&job.schedule)
            .bind(job.next_run_at)
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(|error| map_error("cannot upsert job", error))?;
        }

        transaction
            .commit()
            .await
            .map_err(|error| map_error("cannot commit job sync", error))?;
        Ok(())
    }

    pub async fn list_jobs(&self) -> Result<Vec<JobRecord>> {
        let rows = sqlx::query(&format!(
            "SELECT {JOB_COLUMNS} FROM plugin_jobs ORDER BY plugin_name ASC, job_name ASC"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|error| map_error("cannot list jobs", error))?;
        Ok(rows.iter().map(job_from_row).collect())
    }

    pub async fn get_job(&self, id: &str) -> Result<Option<JobRecord>> {
        let row = sqlx::query(&format!(
            "SELECT {JOB_COLUMNS} FROM plugin_jobs WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| map_error("cannot load job", error))?;
        Ok(row.as_ref().map(job_from_row))
    }

    pub async fn list_due_jobs(&self, now: i64, limit: u32) -> Result<Vec<JobRecord>> {
        let rows = sqlx::query(&format!(
            "SELECT {JOB_COLUMNS} FROM plugin_jobs
              WHERE enabled = 1 AND next_run_at <= ?1
              ORDER BY next_run_at ASC
              LIMIT ?2"
        ))
        .bind(now)
        .bind(i64::from(limit.clamp(1, 500)))
        .fetch_all(self.pool())
        .await
        .map_err(|error| map_error("cannot list due jobs", error))?;
        Ok(rows.iter().map(job_from_row).collect())
    }

    pub async fn mark_job_result(
        &self,
        id: &str,
        status: &str,
        error: Option<&str>,
        ran_at: i64,
        next_run_at: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE plugin_jobs
                SET last_run_at = ?2, last_status = ?3, last_error = ?4,
                    attempts = attempts + 1, next_run_at = ?5, updated_at = ?2
              WHERE id = ?1",
        )
        .bind(id)
        .bind(ran_at)
        .bind(status)
        .bind(error)
        .bind(next_run_at)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("cannot record job result", error))?;
        Ok(())
    }

    pub async fn trigger_job(&self, id: &str, now: i64) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE plugin_jobs SET next_run_at = ?2, updated_at = ?2
              WHERE id = ?1 AND enabled = 1",
        )
        .bind(id)
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(|error| map_error("cannot trigger job", error))?;
        Ok(result.rows_affected() > 0)
    }
}
