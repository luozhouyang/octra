//! Soft per-scope request and token budget guard.
//!
//! The guard is a policy at the edge: it rejects early with a stable error
//! code, while authoritative account quota stays in the core account pool.
//! Counters go through the configured counter backend keyed by the stable
//! instance id, so a chain reload keeps them and a shared backend can serve
//! several daemon processes. A rolling window starts at the first admitted
//! request.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use octra_api_plugin_types::{Capability, PluginError, PluginManifest};
use octra_llm_core::{GatewayError, estimate_tokens};
use octra_llm_plugin_types::{
    LlmAction, LlmCounterStore, LlmErrorContext, LlmLogContext, LlmPlugin, LlmPluginCreateContext,
    LlmPluginFactory, LlmPluginResult, LlmRequestContext, LlmScope, REQUEST_LLM_SCOPES,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifest;

const DEFAULT_CODE: &str = "budget_exceeded";
const DEFAULT_STATUS: u16 = 429;
const DEFAULT_PERIOD_MS: i64 = 86_400_000;
const TOKEN_RESERVATION: &str = "tokens";

pub struct BudgetGuard {
    manifest: PluginManifest,
    key: String,
    requests: Option<u64>,
    tokens: Option<u64>,
    period_ms: i64,
    code: String,
    status: u16,
    counters: Arc<dyn LlmCounterStore>,
}

impl BudgetGuard {
    fn counter_key(&self) -> String {
        format!("budget:{}", self.key)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    requests: Option<u64>,
    tokens: Option<u64>,
    period: Option<String>,
    code: Option<String>,
    status: Option<u16>,
}

pub struct BudgetGuardFactory;

impl BudgetGuardFactory {
    fn build(
        &self,
        config: Value,
        instance_key: &str,
        counters: Arc<dyn LlmCounterStore>,
    ) -> Result<BudgetGuard, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid budget guard config: {error}"))
        })?;
        if config.requests.is_none() && config.tokens.is_none() {
            return Err(PluginError::fail_closed(
                "llm-budget-guard requires `requests` or `tokens`",
            ));
        }
        if config.requests == Some(0) || config.tokens == Some(0) {
            return Err(PluginError::fail_closed(
                "llm-budget-guard limits must be positive",
            ));
        }
        let period_ms = match config.period.as_deref().unwrap_or("day") {
            "minute" => 60_000,
            "hour" => 3_600_000,
            "day" => DEFAULT_PERIOD_MS,
            "month" => DEFAULT_PERIOD_MS * 30,
            other => {
                return Err(PluginError::fail_closed(format!(
                    "llm-budget-guard period must be minute, hour, day or month, got '{other}'"
                )));
            }
        };
        let status = config.status.unwrap_or(DEFAULT_STATUS);
        if !(400..=599).contains(&status) {
            return Err(PluginError::fail_closed(
                "llm-budget-guard status must be a 4xx or 5xx code",
            ));
        }
        let code = config
            .code
            .as_deref()
            .map(str::trim)
            .filter(|code| !code.is_empty())
            .unwrap_or(DEFAULT_CODE)
            .to_string();
        Ok(BudgetGuard {
            manifest: self.manifest(),
            key: instance_key.to_string(),
            requests: config.requests,
            tokens: config.tokens,
            period_ms,
            code,
            status,
            counters,
        })
    }
}

impl LlmPluginFactory for BudgetGuardFactory {
    fn manifest(&self) -> PluginManifest {
        let mut manifest = manifest(
            "llm-budget-guard",
            "Reject requests that exceed a soft per-scope request or token budget",
        );
        manifest.priority = 650;
        manifest.capabilities = vec![Capability::RequestBody];
        manifest.config_schema = Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "llm-budget-guard config",
            "description": "At least one of `requests` or `tokens` is required.",
            "type": "object",
            "additionalProperties": false,
            "anyOf": [
                {"required": ["requests"]},
                {"required": ["tokens"]}
            ],
            "properties": {
                "requests": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum requests per rolling window."
                },
                "tokens": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum estimated plus actual tokens per rolling window."
                },
                "period": {
                    "type": "string",
                    "enum": ["minute", "hour", "day", "month"],
                    "default": "day"
                },
                "code": {
                    "type": "string",
                    "default": "budget_exceeded",
                    "description": "Stable error code returned in the rejection."
                },
                "status": {
                    "type": "integer",
                    "minimum": 400,
                    "maximum": 599,
                    "default": 429
                }
            }
        }));
        manifest
    }

    fn supported_scopes(&self) -> &'static [LlmScope] {
        &REQUEST_LLM_SCOPES
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        // Direct construction (tests, embeddings) has no host state; give the
        // instance a key derived from its configuration.
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        config.to_string().hash(&mut hasher);
        let key = format!("adhoc:{:x}", hasher.finish());
        Ok(Arc::new(self.build(
            config,
            &key,
            Arc::new(octra_llm_plugin_types::MemoryCounterStore::default()),
        )?))
    }

    fn create_with(
        &self,
        config: Value,
        context: &LlmPluginCreateContext<'_>,
    ) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        Ok(Arc::new(self.build(
            config,
            context.instance_key,
            context.counters.clone(),
        )?))
    }
}

#[async_trait]
impl LlmPlugin for BudgetGuard {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
        let estimate = estimate_tokens(cx.request);
        let window = self
            .counters
            .admit_budget(
                &self.counter_key(),
                now_ms(),
                self.period_ms,
                self.requests,
                self.tokens,
                estimate,
            )
            .await
            .map_err(|error| PluginError {
                message: format!("llm-budget-guard counter backend failed: {error}"),
                fail_policy: self.manifest.fail_policy,
            })?;
        if !window.accepted {
            tracing::debug!(
                request_id = %cx.meta.request_id,
                code = %self.code,
                "llm budget rejected request"
            );
            return Ok(LlmAction::Reject(
                GatewayError::upstream(self.status, "request budget exhausted")
                    .with_code(self.code.clone()),
            ));
        }
        *cx.state = json!({ TOKEN_RESERVATION: estimate });
        Ok(LlmAction::Continue)
    }

    /// Replaces the token estimate with the actual usage once it is known.
    async fn on_log(&self, cx: &mut LlmLogContext<'_>) {
        let Some(reserved) = cx.state.get(TOKEN_RESERVATION).and_then(Value::as_u64) else {
            return;
        };
        let actual = cx
            .event
            .usage
            .input_tokens
            .saturating_add(cx.event.usage.output_tokens);
        if let Err(error) = self
            .counters
            .settle_budget(
                &self.counter_key(),
                now_ms(),
                self.period_ms,
                reserved,
                actual,
            )
            .await
        {
            tracing::warn!(%error, "cannot settle the LLM token budget");
        }
        *cx.state = Value::Null;
    }

    /// Refunds the token reservation when the request fails before a log event
    /// can reconcile it. `on_log` clears the same slot, so this is idempotent.
    async fn on_error(&self, cx: &mut LlmErrorContext<'_>) -> LlmPluginResult {
        let Some(reserved) = cx.state.get(TOKEN_RESERVATION).and_then(Value::as_u64) else {
            return Ok(LlmAction::Continue);
        };
        if let Err(error) = self
            .counters
            .settle_budget(&self.counter_key(), now_ms(), self.period_ms, reserved, 0)
            .await
        {
            tracing::warn!(%error, "cannot refund the LLM token budget");
        }
        *cx.state = Value::Null;
        Ok(LlmAction::Continue)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{ChatRequest, Message, Protocol, Usage};
    use octra_llm_plugin_types::{LlmLogEvent, LlmRequestMeta};
    use octra_llm_plugin_types::{LlmResponseHeaders, MemoryCounterStore};

    fn meta() -> LlmRequestMeta<'static> {
        LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        }
    }

    struct Harness {
        plugin: BudgetGuard,
        counters: Arc<MemoryCounterStore>,
        key: String,
    }

    fn guard(config: Value) -> Harness {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let key = format!("test-instance-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        harness_with(config, &key, Arc::new(MemoryCounterStore::default()))
    }

    fn harness_with(config: Value, key: &str, counters: Arc<MemoryCounterStore>) -> Harness {
        let plugin = BudgetGuardFactory
            .build(config, key, counters.clone())
            .unwrap();
        Harness {
            plugin,
            counters,
            key: format!("budget:{key}"),
        }
    }

    fn window_tokens(harness: &Harness) -> u64 {
        harness
            .counters
            .budget_window(&harness.key)
            .map(|window| window.tokens)
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn counters_survive_instance_recreation() {
        let counters = Arc::new(MemoryCounterStore::default());
        let first = harness_with(json!({"requests": 1}), "reload-instance", counters.clone());
        assert!(matches!(
            request(&first.plugin, "hello").await.unwrap(),
            LlmAction::Continue
        ));

        // Simulates a chain reload: a new instance with the same key and the
        // same shared counter store sees the live window.
        let second = harness_with(json!({"requests": 1}), "reload-instance", counters);
        assert!(matches!(
            request(&second.plugin, "hello").await.unwrap(),
            LlmAction::Reject(_)
        ));
    }

    async fn request(plugin: &BudgetGuard, text: &str) -> LlmPluginResult {
        let mut request = ChatRequest::new("m", vec![Message::user(text)]);
        let meta = meta();
        let mut state = Value::Null;
        let mut cx = LlmRequestContext {
            meta: &meta,
            headers: &mut LlmResponseHeaders::default(),
            request: &mut request,
            state: &mut state,
        };
        plugin.on_request(&mut cx).await
    }

    #[tokio::test]
    async fn request_limit_rejects_with_stable_code() {
        let harness = guard(json!({"requests": 1, "period": "day"}));
        assert!(matches!(
            request(&harness.plugin, "hello").await.unwrap(),
            LlmAction::Continue
        ));
        let rejected = request(&harness.plugin, "hello").await.unwrap();
        let error = match rejected {
            LlmAction::Reject(error) => error,
            LlmAction::Continue | LlmAction::Respond(_) => {
                panic!("second request should be rejected")
            }
        };
        assert_eq!(error.status, 429);
        assert_eq!(error.code.as_deref(), Some("budget_exceeded"));
    }

    #[tokio::test]
    async fn error_refunds_the_token_reservation_once() {
        let harness = guard(json!({"tokens": 100, "period": "hour"}));
        let meta = meta();
        let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
        let mut state = Value::Null;
        let mut cx = LlmRequestContext {
            meta: &meta,
            headers: &mut LlmResponseHeaders::default(),
            request: &mut request,
            state: &mut state,
        };
        harness.plugin.on_request(&mut cx).await.unwrap();
        assert!(window_tokens(&harness) > 0);

        let mut error = GatewayError::upstream(500, "boom");
        let mut cx = octra_llm_plugin_types::LlmErrorContext {
            meta: &meta,
            provider_id: None,
            account_id: None,
            error: &mut error,
            state: &mut state,
        };
        harness.plugin.on_error(&mut cx).await.unwrap();
        assert_eq!(window_tokens(&harness), 0);
        assert!(cx.state.is_null());

        // A later on_log/on_error for the same request must not refund twice.
        harness.plugin.on_error(&mut cx).await.unwrap();
        assert_eq!(window_tokens(&harness), 0);
    }

    #[tokio::test]
    async fn token_budget_reconciles_actual_usage() {
        let harness = guard(json!({"tokens": 100, "period": "hour"}));
        let meta = meta();
        let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
        let mut state = Value::Null;
        let mut cx = LlmRequestContext {
            meta: &meta,
            headers: &mut LlmResponseHeaders::default(),
            request: &mut request,
            state: &mut state,
        };
        harness.plugin.on_request(&mut cx).await.unwrap();
        let reserved = window_tokens(&harness);
        assert!(reserved > 0);

        let usage = Usage {
            input_tokens: 2,
            output_tokens: 3,
            ..Usage::default()
        };
        let event = LlmLogEvent {
            model: "m",
            requested_model: None,
            provider_id: None,
            provider_name: None,
            account_id: None,
            account_name: None,
            status: 200,
            latency_ms: 1,
            usage: &usage,
            finish_reason: Some("stop"),
            streaming: false,
            error: None,
        };
        let mut state = json!({ TOKEN_RESERVATION: reserved });
        let mut cx = LlmLogContext {
            meta: &meta,
            event: &event,
            state: &mut state,
        };
        harness.plugin.on_log(&mut cx).await;
        assert_eq!(window_tokens(&harness), 5);
        assert_eq!(state, Value::Null);
    }

    #[test]
    fn invalid_config_is_rejected() {
        assert!(BudgetGuardFactory.create(json!({})).is_err());
        assert!(BudgetGuardFactory.create(json!({"requests": 0})).is_err());
        assert!(
            BudgetGuardFactory
                .create(json!({"requests": 1, "period": "year"}))
                .is_err()
        );
        assert!(
            BudgetGuardFactory
                .create(json!({"requests": 1, "status": 200}))
                .is_err()
        );
        assert!(
            BudgetGuardFactory
                .create(json!({"requests": 1, "unknown": true}))
                .is_err()
        );
    }
}
