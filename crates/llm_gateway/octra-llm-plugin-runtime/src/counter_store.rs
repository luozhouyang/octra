//! Counter backend backed by the daemon's SQLite database.
//!
//! Several daemon processes that share one database therefore share rate-limit
//! and budget counters. Every admission runs in a `BEGIN IMMEDIATE`
//! transaction, so concurrent writers serialise instead of over-admitting.

use std::sync::Arc;

use async_trait::async_trait;
use octra_llm_plugin_types::{BudgetWindowView, CounterOutcome, CounterWindow, LlmCounterStore};
use octra_storage::Storage;

pub struct StorageCounterStore {
    storage: Arc<Storage>,
}

impl StorageCounterStore {
    pub fn new(storage: Arc<Storage>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl LlmCounterStore for StorageCounterStore {
    async fn admit_windows(
        &self,
        windows: &[CounterWindow<'_>],
    ) -> std::result::Result<CounterOutcome, String> {
        self.storage
            .llm
            .admit_counter_windows(windows)
            .await
            .map_err(|error| error.to_string())
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
        self.storage
            .llm
            .admit_budget_window(key, now_ms, period_ms, max_requests, max_tokens, estimate)
            .await
            .map_err(|error| error.to_string())
    }

    async fn settle_budget(
        &self,
        key: &str,
        now_ms: i64,
        period_ms: i64,
        reserved: u64,
        actual: u64,
    ) -> std::result::Result<BudgetWindowView, String> {
        self.storage
            .llm
            .settle_budget_window(key, now_ms, period_ms, reserved, actual)
            .await
            .map_err(|error| error.to_string())
    }
}
