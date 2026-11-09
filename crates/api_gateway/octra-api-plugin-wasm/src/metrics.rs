//! Per-plugin execution counters exposed through the admin plane.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::RwLock;
use serde::Serialize;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PluginStatsSnapshot {
    pub calls: u64,
    pub rejections: u64,
    pub failures: u64,
    pub timeouts: u64,
    pub total_latency_micros: u64,
    pub max_latency_micros: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Continue,
    Reject,
    Failure { timeout: bool },
}

#[derive(Default)]
pub(crate) struct PluginStats {
    calls: AtomicU64,
    rejections: AtomicU64,
    failures: AtomicU64,
    timeouts: AtomicU64,
    total_latency_micros: AtomicU64,
    max_latency_micros: AtomicU64,
}

impl PluginStats {
    pub(crate) fn record(&self, outcome: Outcome, latency: Duration) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        match outcome {
            Outcome::Continue => {}
            Outcome::Reject => {
                self.rejections.fetch_add(1, Ordering::Relaxed);
            }
            Outcome::Failure { timeout } => {
                self.failures.fetch_add(1, Ordering::Relaxed);
                if timeout {
                    self.timeouts.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let micros = u64::try_from(latency.as_micros()).unwrap_or(u64::MAX);
        self.total_latency_micros
            .fetch_add(micros, Ordering::Relaxed);
        self.max_latency_micros.fetch_max(micros, Ordering::Relaxed);
    }

    fn snapshot(&self) -> PluginStatsSnapshot {
        PluginStatsSnapshot {
            calls: self.calls.load(Ordering::Relaxed),
            rejections: self.rejections.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
            total_latency_micros: self.total_latency_micros.load(Ordering::Relaxed),
            max_latency_micros: self.max_latency_micros.load(Ordering::Relaxed),
        }
    }
}

#[derive(Default)]
pub struct PluginMetrics {
    stats: RwLock<HashMap<String, Arc<PluginStats>>>,
}

impl PluginMetrics {
    pub(crate) fn stats(&self, plugin: &str) -> Arc<PluginStats> {
        if let Some(stats) = self.stats.read().get(plugin) {
            return stats.clone();
        }
        self.stats
            .write()
            .entry(plugin.to_string())
            .or_default()
            .clone()
    }

    pub fn snapshot(&self, plugin: &str) -> Option<PluginStatsSnapshot> {
        self.stats.read().get(plugin).map(|stats| stats.snapshot())
    }

    /// All plugins with recorded activity, ordered by name.
    pub fn snapshots(&self) -> Vec<(String, PluginStatsSnapshot)> {
        let mut snapshots = self
            .stats
            .read()
            .iter()
            .map(|(name, stats)| (name.clone(), stats.snapshot()))
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| left.0.cmp(&right.0));
        snapshots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_outcomes_and_latency() {
        let stats = PluginStats::default();
        stats.record(Outcome::Continue, Duration::from_micros(10));
        stats.record(Outcome::Reject, Duration::from_micros(20));
        stats.record(
            Outcome::Failure { timeout: true },
            Duration::from_micros(30),
        );
        stats.record(
            Outcome::Failure { timeout: false },
            Duration::from_micros(40),
        );

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.calls, 4);
        assert_eq!(snapshot.rejections, 1);
        assert_eq!(snapshot.failures, 2);
        assert_eq!(snapshot.timeouts, 1);
        assert_eq!(snapshot.total_latency_micros, 100);
        assert_eq!(snapshot.max_latency_micros, 40);
    }

    #[test]
    fn metrics_are_scoped_per_plugin() {
        let metrics = PluginMetrics::default();
        let alpha = metrics.stats("alpha");
        let same = metrics.stats("alpha");
        assert!(Arc::ptr_eq(&alpha, &same));
        alpha.record(Outcome::Continue, Duration::ZERO);
        metrics
            .stats("beta")
            .record(Outcome::Reject, Duration::ZERO);

        let names: Vec<String> = metrics
            .snapshots()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["alpha", "beta"]);
        assert_eq!(metrics.snapshot("alpha").unwrap().calls, 1);
        assert_eq!(metrics.snapshot("beta").unwrap().rejections, 1);
        assert!(metrics.snapshot("missing").is_none());
    }
}
