//! Request access logs and control-plane audit rows.

use chrono::{DateTime, Utc};
use octra_core::{Error, Result};
use serde::Serialize;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRequestLog {
    pub request_id: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub latency_ms: u64,
    pub route_id: Option<String>,
    pub service_id: Option<String>,
    pub target_url: Option<String>,
    pub consumer_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestLog {
    pub id: i64,
    pub request_id: String,
    pub method: String,
    pub path: String,
    pub status: i64,
    pub latency_ms: i64,
    pub route_id: Option<String>,
    pub service_id: Option<String>,
    pub target_url: Option<String>,
    pub consumer_id: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditLog {
    pub id: i64,
    pub actor: Option<String>,
    pub action: String,
    pub target: Option<String>,
    pub detail: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| Error::Storage(format!("invalid timestamp '{value}': {error}")))
}

fn request_log_from_row(row: &SqliteRow) -> Result<RequestLog> {
    let created_at: String = row.get("created_at");
    Ok(RequestLog {
        id: row.get("id"),
        request_id: row.get("request_id"),
        method: row.get("method"),
        path: row.get("path"),
        status: row.get("status"),
        latency_ms: row.get("latency_ms"),
        route_id: row.get("route_id"),
        service_id: row.get("service_id"),
        target_url: row.get("target_url"),
        consumer_id: row.get("consumer_id"),
        error: row.get("error"),
        created_at: parse_timestamp(&created_at)?,
    })
}

fn audit_log_from_row(row: &SqliteRow) -> Result<AuditLog> {
    let created_at: String = row.get("created_at");
    let detail: String = row.get("detail");
    Ok(AuditLog {
        id: row.get("id"),
        actor: row.get("actor"),
        action: row.get("action"),
        target: row.get("target"),
        detail: serde_json::from_str(&detail).unwrap_or(serde_json::Value::Null),
        created_at: parse_timestamp(&created_at)?,
    })
}

#[derive(Clone)]
pub struct LogStore {
    pool: sqlx::SqlitePool,
}

impl LogStore {
    pub(crate) fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }

    fn pool(&self) -> &sqlx::SqlitePool {
        &self.pool
    }

    pub async fn insert_request_log(&self, log: &NewRequestLog) -> Result<()> {
        sqlx::query(
            "INSERT INTO request_logs
                (request_id, method, path, status, latency_ms, route_id, service_id,
                 target_url, consumer_id, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(&log.request_id)
        .bind(&log.method)
        .bind(&log.path)
        .bind(i64::from(log.status))
        .bind(i64::try_from(log.latency_ms).unwrap_or(i64::MAX))
        .bind(&log.route_id)
        .bind(&log.service_id)
        .bind(&log.target_url)
        .bind(&log.consumer_id)
        .bind(&log.error)
        .execute(self.pool())
        .await
        .map_err(|error| Error::Storage(format!("cannot insert request log: {error}")))?;
        Ok(())
    }

    pub async fn list_request_logs(
        &self,
        limit: u32,
        request_id: Option<&str>,
        route_id: Option<&str>,
    ) -> Result<Vec<RequestLog>> {
        let rows = sqlx::query(
            "SELECT id, request_id, method, path, status, latency_ms, route_id, service_id,
                    target_url, consumer_id, error, created_at
               FROM request_logs
              WHERE (?1 IS NULL OR request_id = ?1)
                AND (?2 IS NULL OR route_id = ?2)
              ORDER BY id DESC
              LIMIT ?3",
        )
        .bind(request_id)
        .bind(route_id)
        .bind(i64::from(limit.clamp(1, 1000)))
        .fetch_all(self.pool())
        .await
        .map_err(|error| Error::Storage(format!("cannot list request logs: {error}")))?;
        rows.iter().map(request_log_from_row).collect()
    }

    pub async fn insert_audit_log(
        &self,
        actor: Option<&str>,
        action: &str,
        target: Option<&str>,
        detail: &serde_json::Value,
    ) -> Result<()> {
        let detail = serde_json::to_string(detail)
            .map_err(|error| Error::Storage(format!("cannot serialize audit detail: {error}")))?;
        sqlx::query(
            "INSERT INTO audit_logs (actor, action, target, detail) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(actor)
        .bind(action)
        .bind(target)
        .bind(detail)
        .execute(self.pool())
        .await
        .map_err(|error| Error::Storage(format!("cannot insert audit log: {error}")))?;
        Ok(())
    }

    pub async fn list_audit_logs(&self, limit: u32) -> Result<Vec<AuditLog>> {
        let rows = sqlx::query(
            "SELECT id, actor, action, target, detail, created_at
               FROM audit_logs
              ORDER BY id DESC
              LIMIT ?1",
        )
        .bind(i64::from(limit.clamp(1, 1000)))
        .fetch_all(self.pool())
        .await
        .map_err(|error| Error::Storage(format!("cannot list audit logs: {error}")))?;
        rows.iter().map(audit_log_from_row).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Storage;

    #[tokio::test]
    async fn stores_and_filters_request_logs() {
        let storage = Storage::connect_in_memory().await.unwrap();
        for (request_id, route) in [("r1", "route-a"), ("r2", "route-b")] {
            storage
                .logs
                .insert_request_log(&NewRequestLog {
                    request_id: request_id.into(),
                    method: "GET".into(),
                    path: "/items".into(),
                    status: 200,
                    latency_ms: 5,
                    route_id: Some(route.into()),
                    service_id: Some("svc".into()),
                    target_url: Some("http://up.test".into()),
                    consumer_id: None,
                    error: None,
                })
                .await
                .unwrap();
        }

        let all = storage
            .logs
            .list_request_logs(10, None, None)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].request_id, "r2");

        let filtered = storage
            .logs
            .list_request_logs(10, Some("r1"), None)
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].route_id.as_deref(), Some("route-a"));
    }

    #[tokio::test]
    async fn stores_audit_rows() {
        let storage = Storage::connect_in_memory().await.unwrap();
        storage
            .logs
            .insert_audit_log(
                Some("admin"),
                "service.create",
                Some("svc"),
                &serde_json::json!({"name": "svc"}),
            )
            .await
            .unwrap();
        let rows = storage.logs.list_audit_logs(10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "service.create");
        assert_eq!(rows[0].detail["name"], "svc");
    }
}
