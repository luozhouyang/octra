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
