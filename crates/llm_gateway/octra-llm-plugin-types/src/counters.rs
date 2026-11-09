//! Shared counter backends for rate limiting and budget guards.
//!
//! Built-in limiting plugins keep their counters behind [`LlmCounterStore`]
//! so a multi-process deployment can share them. The daemon selects the
//! backend; [`MemoryCounterStore`] is the default for tests and single-process
//! setups.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

/// One calendar-window bucket to admit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterWindow<'a> {
    pub key: &'a str,
    pub window_start: i64,
    pub window_end: i64,
    pub limit: u64,
}

/// Result of admitting a set of windows. When `accepted` is false nothing was
/// incremented and `counts` holds the current values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CounterOutcome {
    pub counts: Vec<u64>,
    pub accepted: bool,
}

/// Rolling budget window after an admission or settlement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetWindowView {
    pub accepted: bool,
    pub ends_at_ms: i64,
    pub requests: u64,
    pub tokens: u64,
}

#[async_trait]
pub trait LlmCounterStore: Send + Sync {
    /// Atomically checks every window against its limit and increments all of
    /// them when none is exhausted. A rejected admission leaves every counter
    /// unchanged.
    async fn admit_windows(
        &self,
        windows: &[CounterWindow<'_>],
    ) -> std::result::Result<CounterOutcome, String>;

    /// Admits one request into a rolling budget window, reserving `estimate`
    /// tokens when accepted.
    async fn admit_budget(
        &self,
        key: &str,
        now_ms: i64,
        period_ms: i64,
        max_requests: Option<u64>,
        max_tokens: Option<u64>,
        estimate: u64,
    ) -> std::result::Result<BudgetWindowView, String>;

    /// Replaces `reserved` tokens with `actual` ones. When the request failed,
    /// pass `actual = 0` to refund the reservation. Arithmetic saturates.
    async fn settle_budget(
        &self,
        key: &str,
        now_ms: i64,
        period_ms: i64,
        reserved: u64,
        actual: u64,
    ) -> std::result::Result<BudgetWindowView, String>;
}

#[derive(Debug, Clone)]
struct WindowValue {
    count: u64,
    window_end: i64,
}

#[derive(Debug, Clone, Copy, Default)]
struct BudgetState {
    ends_at_ms: i64,
    requests: u64,
    tokens: u64,
}

impl BudgetState {
    fn roll(&mut self, now_ms: i64, period_ms: i64) {
        if self.ends_at_ms == 0 {
            self.ends_at_ms = now_ms.saturating_add(period_ms);
        } else if now_ms >= self.ends_at_ms {
            self.ends_at_ms = now_ms.saturating_add(period_ms);
            self.requests = 0;
            self.tokens = 0;
        }
    }
}

/// Process-local counters. Cheap and dependency-free; use the database
/// backend when several daemons must share limits.
#[derive(Default)]
pub struct MemoryCounterStore {
    windows: Mutex<HashMap<(String, i64), WindowValue>>,
    budgets: Mutex<HashMap<String, BudgetState>>,
}

const MAX_WINDOWS: usize = 100_000;

impl MemoryCounterStore {
    /// Current rolling-window state for diagnostics and tests. Does not roll
    /// the window.
    pub fn budget_window(&self, key: &str) -> Option<BudgetWindowView> {
        let budgets = self.budgets.lock().ok()?;
        budgets.get(key).map(|state| BudgetWindowView {
            accepted: true,
            ends_at_ms: state.ends_at_ms,
            requests: state.requests,
            tokens: state.tokens,
        })
    }

    fn prune(windows: &mut HashMap<(String, i64), WindowValue>) {
        if windows.len() <= MAX_WINDOWS {
            return;
        }
        let now = now_unix();
        windows.retain(|_, value| value.window_end > now);
        if windows.len() > MAX_WINDOWS {
            let mut ends: Vec<i64> = windows.values().map(|value| value.window_end).collect();
            ends.sort_unstable();
            let cutoff = ends[ends.len() - MAX_WINDOWS];
            windows.retain(|_, value| value.window_end >= cutoff);
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[async_trait]
impl LlmCounterStore for MemoryCounterStore {
    async fn admit_windows(
        &self,
        windows: &[CounterWindow<'_>],
    ) -> std::result::Result<CounterOutcome, String> {
        let mut store = self
            .windows
            .lock()
            .map_err(|_| "counter store lock poisoned".to_string())?;
        Self::prune(&mut store);

        let mut counts = Vec::with_capacity(windows.len());
        let mut accepted = true;
        for window in windows {
            let count = store
                .get(&(window.key.to_string(), window.window_start))
                .map(|value| value.count)
                .unwrap_or(0);
            if count >= window.limit {
                accepted = false;
            }
            counts.push(count);
        }
        if accepted {
            for (window, count) in windows.iter().zip(counts.iter_mut()) {
                *count = count.saturating_add(1);
                store.insert(
                    (window.key.to_string(), window.window_start),
                    WindowValue {
                        count: *count,
                        window_end: window.window_end,
                    },
                );
            }
        }
        Ok(CounterOutcome { counts, accepted })
    }

    async fn admit_budget(
        &self,
        key: &str,
        now_ms: i64,
        period_ms: i64,
        max_requests: Option<u64>,
        max_tokens: Option<u64>,
        estimate: u64,
    ) -> std::result::Result<BudgetWindowView, String> {
        let mut budgets = self
            .budgets
            .lock()
            .map_err(|_| "counter store lock poisoned".to_string())?;
        let state = budgets.entry(key.to_string()).or_default();
        state.roll(now_ms, period_ms);
        let requests_exceeded = max_requests.is_some_and(|limit| state.requests >= limit);
        let tokens_exceeded =
            max_tokens.is_some_and(|limit| state.tokens.saturating_add(estimate) > limit);
        let accepted = !(requests_exceeded || tokens_exceeded);
        if accepted {
            state.requests = state.requests.saturating_add(1);
            state.tokens = state.tokens.saturating_add(estimate);
        }
        Ok(BudgetWindowView {
            accepted,
            ends_at_ms: state.ends_at_ms,
            requests: state.requests,
            tokens: state.tokens,
        })
    }

    async fn settle_budget(
        &self,
        key: &str,
        now_ms: i64,
        period_ms: i64,
        reserved: u64,
        actual: u64,
    ) -> std::result::Result<BudgetWindowView, String> {
        let mut budgets = self
            .budgets
            .lock()
            .map_err(|_| "counter store lock poisoned".to_string())?;
        let state = budgets.entry(key.to_string()).or_default();
        state.roll(now_ms, period_ms);
        state.tokens = state.tokens.saturating_sub(reserved).saturating_add(actual);
        Ok(BudgetWindowView {
            accepted: true,
            ends_at_ms: state.ends_at_ms,
            requests: state.requests,
            tokens: state.tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn admits_all_or_nothing() {
        let store = MemoryCounterStore::default();
        let windows = [
            CounterWindow {
                key: "a",
                window_start: 10,
                window_end: 20,
                limit: 2,
            },
            CounterWindow {
                key: "b",
                window_start: 10,
                window_end: 20,
                limit: 1,
            },
        ];
        let first = store.admit_windows(&windows).await.unwrap();
        assert!(first.accepted);
        assert_eq!(first.counts, vec![1, 1]);

        let second = store.admit_windows(&windows).await.unwrap();
        assert!(!second.accepted, "b is exhausted");
        assert_eq!(
            second.counts,
            vec![1, 1],
            "nothing is incremented on reject"
        );

        let third = store
            .admit_windows(&[CounterWindow {
                key: "a",
                window_start: 10,
                window_end: 20,
                limit: 2,
            }])
            .await
            .unwrap();
        assert!(third.accepted);
        assert_eq!(third.counts, vec![2]);
    }

    #[tokio::test]
    async fn budget_rolls_and_settles() {
        let store = MemoryCounterStore::default();
        let admitted = store
            .admit_budget("budget", 1_000, 1_000, Some(1), None, 10)
            .await
            .unwrap();
        assert!(admitted.accepted);
        assert_eq!(admitted.requests, 1);
        assert_eq!(admitted.tokens, 10);

        let rejected = store
            .admit_budget("budget", 1_000, 1_000, Some(1), None, 10)
            .await
            .unwrap();
        assert!(!rejected.accepted);
        assert_eq!(rejected.requests, 1);

        let settled = store
            .settle_budget("budget", 1_000, 1_000, 10, 30)
            .await
            .unwrap();
        assert_eq!(settled.tokens, 30);

        let rolled = store
            .admit_budget("budget", 2_000, 1_000, Some(1), None, 5)
            .await
            .unwrap();
        assert!(rolled.accepted);
        assert_eq!(rolled.ends_at_ms, 3_000);
        assert_eq!(rolled.requests, 1);
        assert_eq!(rolled.tokens, 5);
    }
}
