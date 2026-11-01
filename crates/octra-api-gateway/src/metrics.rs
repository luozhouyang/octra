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
    }
}
