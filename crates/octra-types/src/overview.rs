//! Cross-gateway traffic overview for the desktop dashboard.
//!
//! The three data planes persist requests in separate tables. This module
//! aggregates them over a fixed window so the UI does not have to fan out to
//! three list endpoints and bucket rows on the client.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct OverviewQuery {
    pub range: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Range {
    Hour,
    Day,
    Week,
}

impl Range {
    fn parse(value: Option<&str>) -> Option<Self> {
        match value.unwrap_or("24h") {
            "1h" => Some(Self::Hour),
            "24h" => Some(Self::Day),
            "7d" => Some(Self::Week),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::Day => "24h",
            Self::Week => "7d",
        }
    }

    fn seconds(self) -> i64 {
        match self {
            Self::Hour => 3_600,
            Self::Day => 86_400,
            Self::Week => 604_800,
        }
    }

    fn step_seconds(self) -> i64 {
        match self {
            Self::Hour => 300,
            Self::Day => 3_600,
            Self::Week => 86_400,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Overview {
    pub range: &'static str,
    pub generated_at: String,
    /// Unix epoch seconds for the start of each bucket, ascending.
    pub buckets: Vec<i64>,
    pub series: Series,
    pub totals: Totals,
    pub latency: Latency,
    pub destinations: Destinations,
}

#[derive(Debug, Serialize)]
pub struct Series {
    pub api: Vec<u64>,
    pub llm: Vec<u64>,
    pub mcp: Vec<u64>,
    pub errors: Vec<u64>,
}

#[derive(Debug, Serialize)]
pub struct Totals {
    pub api: u64,
    pub llm: u64,
    pub mcp: u64,
    pub errors: u64,
    pub requests: u64,
}

#[derive(Debug, Serialize)]
pub struct Latency {
    pub api_avg_ms: Option<f64>,
    pub llm_avg_ms: Option<f64>,
    pub mcp_avg_ms: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Destination {
    pub id: String,
    pub label: String,
    pub count: u64,
}

#[derive(Debug, Serialize)]
pub struct Destinations {
    pub api: Vec<Destination>,
    pub llm: Vec<Destination>,
    pub mcp: Vec<Destination>,
}

pub async fn overview(
    State(state): State<AppState>,
    Query(query): Query<OverviewQuery>,
) -> Response {
    let Some(range) = Range::parse(query.range.as_deref()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "range must be 1h, 24h or 7d" })),
        )
            .into_response();
    };

    match build(state.storage.pool(), range).await {
        Ok(overview) => Json(overview).into_response(),
        Err(error) => {
            tracing::error!(%error, "cannot build the system overview");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "cannot build the system overview" })),
            )
                .into_response()
        }
    }
}

async fn build(pool: &SqlitePool, range: Range) -> Result<Overview, sqlx::Error> {
    let now = Utc::now().timestamp();
    let step = range.step_seconds();
    let count = usize::try_from(range.seconds() / step).unwrap_or(1);
    // End on the current partial bucket and keep exactly `count` buckets, so
    // the series, latency and destination queries share one window.
    let last = (now / step) * step;
    let first = last - (count.saturating_sub(1) as i64) * step;
    let window_start = first;

    let api = gateway_buckets(
        pool,
        "request_logs",
        "created_at",
        "status >= 400",
        window_start,
        step,
    )
    .await?;
    let llm = gateway_buckets(
        pool,
        "llm_usage",
        "created_at",
        "(status_code IS NOT NULL AND status_code >= 400) OR error IS NOT NULL",
        window_start,
        step,
    )
    .await?;
    let mcp = gateway_buckets(
        pool,
        "mcp_invocations",
        "started_at",
        "status != 'ok'",
        window_start,
        step,
    )
    .await?;

    let buckets: Vec<i64> = (0..count)
        .map(|index| first + index as i64 * step)
        .collect();

    let mut series = Series {
        api: vec![0; count],
        llm: vec![0; count],
        mcp: vec![0; count],
        errors: vec![0; count],
    };
    fill_series(
        &buckets,
        first,
        step,
        &api,
        &mut series.api,
        &mut series.errors,
    );
    fill_series(
        &buckets,
        first,
        step,
        &llm,
        &mut series.llm,
        &mut series.errors,
    );
    fill_series(
        &buckets,
        first,
        step,
        &mcp,
        &mut series.mcp,
        &mut series.errors,
    );

    let totals = Totals {
        api: series.api.iter().sum(),
        llm: series.llm.iter().sum(),
        mcp: series.mcp.iter().sum(),
        errors: series.errors.iter().sum(),
        requests: series.api.iter().sum::<u64>()
            + series.llm.iter().sum::<u64>()
            + series.mcp.iter().sum::<u64>(),
    };

    let latency = Latency {
        api_avg_ms: average(
            pool,
            "SELECT AVG(latency_ms) FROM request_logs WHERE strftime('%s', created_at) >= ?1",
            window_start,
        )
        .await?,
        llm_avg_ms: average(
            pool,
            "SELECT AVG(latency_ms) FROM llm_usage
              WHERE latency_ms IS NOT NULL AND strftime('%s', created_at) >= ?1",
            window_start,
        )
        .await?,
        mcp_avg_ms: average(
            pool,
            "SELECT AVG(duration_ms) FROM mcp_invocations
              WHERE duration_ms IS NOT NULL AND strftime('%s', started_at) >= ?1",
            window_start,
        )
        .await?,
    };

    let destinations = Destinations {
        api: destinations(
            pool,
            "SELECT COALESCE(s.name, r.service_id, 'unassigned') AS label, COUNT(*) AS count
               FROM request_logs r
               LEFT JOIN services s ON s.id = r.service_id
              WHERE strftime('%s', r.created_at) >= ?1
              GROUP BY label
              ORDER BY count DESC
              LIMIT 3",
            window_start,
        )
        .await?,
        llm: destinations(
            pool,
            "SELECT COALESCE(NULLIF(provider_id, ''), 'unassigned') AS label, COUNT(*) AS count
               FROM llm_usage
              WHERE strftime('%s', created_at) >= ?1
              GROUP BY label
              ORDER BY count DESC
              LIMIT 3",
            window_start,
        )
        .await?,
        mcp: destinations(
            pool,
            "SELECT COALESCE(s.name, i.server_id) AS label, COUNT(*) AS count
               FROM mcp_invocations i
               LEFT JOIN mcp_servers s ON s.id = i.server_id
              WHERE strftime('%s', i.started_at) >= ?1
              GROUP BY label
              ORDER BY count DESC
              LIMIT 3",
            window_start,
        )
        .await?,
    };

    Ok(Overview {
        range: range.name(),
        generated_at: Utc::now().to_rfc3339(),
        buckets,
        series,
        totals,
        latency,
        destinations,
    })
}

async fn gateway_buckets(
    pool: &SqlitePool,
    table: &str,
    time_column: &str,
    error_expression: &str,
    start: i64,
    step: i64,
) -> Result<Vec<(i64, u64, u64)>, sqlx::Error> {
    let query = format!(
        "SELECT (strftime('%s', {time_column}) / ?1) * ?1 AS bucket,
                COUNT(*) AS count,
                SUM(CASE WHEN {error_expression} THEN 1 ELSE 0 END) AS errors
           FROM {table}
          WHERE strftime('%s', {time_column}) >= ?2
          GROUP BY bucket
          ORDER BY bucket ASC"
    );
    let rows = sqlx::query(&query)
        .bind(step)
        .bind(start)
        .fetch_all(pool)
        .await?;

    Ok(rows
        .iter()
        .map(|row| {
            (
                row.get::<i64, _>("bucket"),
                row.get::<i64, _>("count").max(0) as u64,
                row.get::<Option<i64>, _>("errors").unwrap_or(0).max(0) as u64,
            )
        })
        .collect())
}

fn fill_series(
    buckets: &[i64],
    first: i64,
    step: i64,
    rows: &[(i64, u64, u64)],
    counts: &mut [u64],
    errors: &mut [u64],
) {
    for (bucket, count, bucket_errors) in rows {
        let index = (bucket - first) / step;
        if let Ok(index) = usize::try_from(index)
            && index < buckets.len()
        {
            counts[index] += *count;
            errors[index] += *bucket_errors;
        }
    }
}

async fn average(pool: &SqlitePool, query: &str, start: i64) -> Result<Option<f64>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<f64>>(query)
        .bind(start)
        .fetch_one(pool)
        .await
}

async fn destinations(
    pool: &SqlitePool,
    query: &str,
    start: i64,
) -> Result<Vec<Destination>, sqlx::Error> {
    let rows = sqlx::query(query).bind(start).fetch_all(pool).await?;
    Ok(rows
        .iter()
        .map(|row| {
            let label: String = row.get("label");
            Destination {
                id: label.clone(),
                label,
                count: row.get::<i64, _>("count").max(0) as u64,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{Range, fill_series};

    #[test]
    fn parses_supported_ranges() {
        assert_eq!(Range::parse(Some("1h")), Some(Range::Hour));
        assert_eq!(Range::parse(Some("24h")), Some(Range::Day));
        assert_eq!(Range::parse(Some("7d")), Some(Range::Week));
        assert_eq!(Range::parse(Some("30d")), None);
    }

    #[test]
    fn fills_missing_buckets_with_zeros() {
        let buckets = vec![100, 200, 300];
        let mut counts = vec![0; 3];
        let mut errors = vec![0; 3];
        fill_series(&buckets, 100, 100, &[(200, 7, 2)], &mut counts, &mut errors);
        assert_eq!(counts, vec![0, 7, 0]);
        assert_eq!(errors, vec![0, 2, 0]);
    }
}
