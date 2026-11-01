//! Lock-free request counters for the API data plane.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

#[derive(Debug, Default)]
pub struct ApiMetrics {
    started_at: OnceLock<Instant>,
    requests_total: AtomicU64,
    status_2xx: AtomicU64,
    status_3xx: AtomicU64,
    status_4xx: AtomicU64,
    status_5xx: AtomicU64,
    upstream_errors: AtomicU64,
    latency_micros_total: AtomicU64,
    retries: AtomicU64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MetricsSnapshot {
    pub uptime_secs: u64,
    pub requests_total: u64,
    pub status_2xx: u64,
    pub status_3xx: u64,
    pub status_4xx: u64,
    pub status_5xx: u64,
    pub upstream_errors: u64,
    pub retries: u64,
    pub latency_avg_ms: f64,
}

impl ApiMetrics {
    fn start(&self) -> Instant {
        *self.started_at.get_or_init(Instant::now)
    }

    /// Records one proxied or rejected request.
    pub fn record(&self, status: u16, latency: Duration, retries: u64) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        match status {
            200..=299 => self.status_2xx.fetch_add(1, Ordering::Relaxed),
            300..=399 => self.status_3xx.fetch_add(1, Ordering::Relaxed),
            400..=499 => self.status_4xx.fetch_add(1, Ordering::Relaxed),
            _ => self.status_5xx.fetch_add(1, Ordering::Relaxed),
        };
        self.latency_micros_total.fetch_add(
            latency.as_micros().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        if retries > 0 {
            self.retries.fetch_add(retries, Ordering::Relaxed);
        }
    }

    pub fn record_upstream_error(&self) {
        self.upstream_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Prometheus text exposition (version 0.0.4).
    pub fn prometheus(&self) -> String {
        let snapshot = self.snapshot();
        let micros = self.latency_micros_total.load(Ordering::Relaxed);
        format!(
            "\
# HELP octra_api_requests_total Total requests handled by the API data plane.
# TYPE octra_api_requests_total counter
octra_api_requests_total {requests_total}
# HELP octra_api_responses_total Responses grouped by status class.
# TYPE octra_api_responses_total counter
octra_api_responses_total{{class=\"2xx\"}} {status_2xx}
octra_api_responses_total{{class=\"3xx\"}} {status_3xx}
octra_api_responses_total{{class=\"4xx\"}} {status_4xx}
octra_api_responses_total{{class=\"5xx\"}} {status_5xx}
# HELP octra_api_upstream_errors_total Upstream transport failures.
# TYPE octra_api_upstream_errors_total counter
octra_api_upstream_errors_total {upstream_errors}
# HELP octra_api_retries_total Retried upstream attempts.
# TYPE octra_api_retries_total counter
octra_api_retries_total {retries}
# HELP octra_api_request_duration_seconds Request latency.
# TYPE octra_api_request_duration_seconds summary
octra_api_request_duration_seconds_sum {seconds}
octra_api_request_duration_seconds_count {requests_total}
# HELP octra_api_uptime_seconds Seconds since the first recorded request.
# TYPE octra_api_uptime_seconds gauge
octra_api_uptime_seconds {uptime}
",
            requests_total = snapshot.requests_total,
            status_2xx = snapshot.status_2xx,
            status_3xx = snapshot.status_3xx,
            status_4xx = snapshot.status_4xx,
            status_5xx = snapshot.status_5xx,
            upstream_errors = snapshot.upstream_errors,
            retries = snapshot.retries,
            seconds = micros as f64 / 1_000_000.0,
            uptime = snapshot.uptime_secs,
        )
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let total = self.requests_total.load(Ordering::Relaxed);
        let micros = self.latency_micros_total.load(Ordering::Relaxed);
        MetricsSnapshot {
            uptime_secs: self.start().elapsed().as_secs(),
            requests_total: total,
            status_2xx: self.status_2xx.load(Ordering::Relaxed),
            status_3xx: self.status_3xx.load(Ordering::Relaxed),
            status_4xx: self.status_4xx.load(Ordering::Relaxed),
            status_5xx: self.status_5xx.load(Ordering::Relaxed),
            upstream_errors: self.upstream_errors.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            latency_avg_ms: if total == 0 {
                0.0
            } else {
                micros as f64 / total as f64 / 1000.0
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_by_status_class() {
        let metrics = ApiMetrics::default();
        metrics.record(200, Duration::from_millis(10), 0);
        metrics.record(304, Duration::from_millis(5), 1);
        metrics.record(404, Duration::from_millis(2), 0);
        metrics.record(502, Duration::from_millis(20), 2);
        metrics.record_upstream_error();

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.requests_total, 4);
        assert_eq!(snapshot.status_2xx, 1);
        assert_eq!(snapshot.status_3xx, 1);
        assert_eq!(snapshot.status_4xx, 1);
        assert_eq!(snapshot.status_5xx, 1);
        assert_eq!(snapshot.upstream_errors, 1);
        assert_eq!(snapshot.retries, 3);
        assert!(snapshot.latency_avg_ms > 0.0);

        let text = metrics.prometheus();
        assert!(text.contains("octra_api_requests_total 4"));
        assert!(text.contains("octra_api_responses_total{class=\"4xx\"} 1"));
        assert!(text.contains("octra_api_upstream_errors_total 1"));
        assert!(text.contains("octra_api_request_duration_seconds_count 4"));
    }
}
