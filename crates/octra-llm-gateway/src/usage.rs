//! Asynchronous usage accounting. Recording never blocks a response.

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use octra_llm_core::{FinishReason, Usage};
use octra_storage::{LlmUsageInput, Storage};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub request_id: String,
    pub virtual_model_id: String,
    pub provider_id: Option<String>,
    pub provider_name: Option<String>,
    pub model_id: Option<String>,
    pub account_id: Option<String>,
    pub account_name: Option<String>,
    pub api_key_id: Option<String>,
    pub protocol: Option<String>,
    pub status_code: Option<u16>,
    pub latency_ms: Option<u64>,
    pub usage: Usage,
    pub finish_reason: Option<String>,
    pub cost_micros: Option<i64>,
    pub cost_currency: Option<String>,
    /// Model pricing used to compute the cost when it was not set directly.
    pub pricing: Option<Value>,
    pub error: Option<String>,
}

impl UsageRecord {
    pub fn new(request_id: impl Into<String>, virtual_model_id: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            virtual_model_id: virtual_model_id.into(),
            provider_id: None,
            provider_name: None,
            model_id: None,
            account_id: None,
            account_name: None,
            api_key_id: None,
            protocol: None,
            status_code: None,
            latency_ms: None,
            usage: Usage::default(),
            finish_reason: None,
            cost_micros: None,
            cost_currency: None,
            pricing: None,
            error: None,
        }
    }
}

/// A usage record plus the request start time; `received_at` lets the final
/// latency include time spent streaming.
#[derive(Debug, Clone)]
pub struct UsageTracker {
    pub started: Instant,
    pub received_at: DateTime<Utc>,
    pub record: UsageRecord,
}

impl UsageTracker {
    pub fn new(request_id: impl Into<String>, virtual_model_id: impl Into<String>) -> Self {
        Self {
            started: Instant::now(),
            received_at: Utc::now(),
            record: UsageRecord::new(request_id, virtual_model_id),
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    pub fn finish_with_status(&mut self, status: u16) {
        self.record.status_code = Some(status);
        self.record.latency_ms = Some(self.elapsed_ms());
    }
}

/// Spawns a task that inserts the usage row. The insert must never block the
/// response path and must never panic the handler.
pub fn spawn_record(storage: Arc<Storage>, record: UsageRecord) {
    tokio::spawn(async move {
        let (cost_micros, cost_currency) = match (record.cost_micros, &record.pricing) {
            (Some(micros), _) => (micros, record.cost_currency.clone()),
            (None, Some(pricing)) => match crate::cost::compute(&record.usage, pricing) {
                Some(cost) => (cost.micros, Some(cost.currency)),
                None => {
                    tracing::debug!(
                        request_id = %record.request_id,
                        "llm model has no usable pricing; recording zero cost"
                    );
                    (0, None)
                }
            },
            (None, None) => (0, record.cost_currency.clone()),
        };
        let input_tokens = record.usage.input_tokens.min(i64::MAX as u64) as i64;
        let output_tokens = record.usage.output_tokens.min(i64::MAX as u64) as i64;
        let input = LlmUsageInput {
            request_id: record.request_id.clone(),
            virtual_model_id: record.virtual_model_id.clone(),
            provider_id: record.provider_id.clone(),
            model_id: record.model_id.clone(),
            account_id: record.account_id.clone(),
            account_name: record.account_name.clone(),
            api_key_id: record.api_key_id.clone(),
            protocol: record.protocol.clone(),
            status_code: record.status_code.map(i64::from),
            latency_ms: record
                .latency_ms
                .map(|value| value.min(i64::MAX as u64) as i64),
            input_tokens,
            output_tokens,
            cache_read_tokens: record
                .usage
                .cache_read_tokens
                .map(|value| value.min(i64::MAX as u64) as i64),
            cache_write_tokens: record
                .usage
                .cache_write_tokens
                .map(|value| value.min(i64::MAX as u64) as i64),
            reasoning_tokens: record
                .usage
                .reasoning_tokens
                .map(|value| value.min(i64::MAX as u64) as i64),
            cost_micros: Some(cost_micros),
            cost_currency,
            finish_reason: record.finish_reason.clone(),
            error: record.error.clone(),
        };
        if let Err(error) = storage.llm.insert_usage(&input).await {
            tracing::warn!(error = ?error, request_id = %record.request_id, "cannot record llm usage");
        }
        if let Some(account_id) = &record.account_id {
            let tokens = input_tokens.saturating_add(output_tokens);
            if let Err(error) = storage
                .llm
                .consume_account_quota(account_id, 1, tokens, cost_micros)
                .await
            {
                tracing::warn!(
                    error = ?error,
                    account_id,
                    request_id = %record.request_id,
                    "cannot update account quota"
                );
            }
        }
    });
}

pub fn finish_reason_label(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::Refusal => "refusal",
        FinishReason::Pause => "pause",
        FinishReason::Other => "other",
    }
}

pub fn usage_json(usage: &Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_read_tokens": usage.cache_read_tokens,
        "cache_write_tokens": usage.cache_write_tokens,
        "reasoning_tokens": usage.reasoning_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json as j;

    #[tokio::test]
    async fn spawn_record_persists_usage() {
        let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
        let mut record = UsageRecord::new("req-1", "octra-default");
        record.provider_id = Some("p1".into());
        record.model_id = Some("m1".into());
        record.protocol = Some("openai".into());
        record.status_code = Some(200);
        record.latency_ms = Some(12);
        record.usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        };
        spawn_record(storage.clone(), record);

        let mut rows = Vec::new();
        for _ in 0..50 {
            rows = storage.llm.list_usage(10).await.unwrap();
            if !rows.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, "req-1");
        assert_eq!(rows[0].input_tokens, 10);
    }

    #[test]
    fn json_shape_and_labels() {
        assert_eq!(finish_reason_label(&FinishReason::ToolCalls), "tool_calls");
        let value = usage_json(&Usage::default());
        assert_eq!(
            value,
            j!({"input_tokens": 0, "output_tokens": 0, "cache_read_tokens": null, "cache_write_tokens": null, "reasoning_tokens": null})
        );
    }
}
