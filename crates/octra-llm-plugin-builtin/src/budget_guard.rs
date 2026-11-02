//! Soft per-scope request and token budget guard.
//!
//! The guard is a policy at the edge: it rejects early with a stable error
//! code, while authoritative account quota stays in the core account pool.
//! Counters live in the plugin instance and reset on chain reload; a rolling
//! window starts at the first admitted request.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use octra_api_plugin_types::{Capability, PluginError, PluginManifest};
use octra_llm_core::{GatewayError, estimate_tokens};
use octra_llm_plugin_types::{
    LlmAction, LlmLogContext, LlmPlugin, LlmPluginFactory, LlmPluginResult, LlmRequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifest;

const DEFAULT_CODE: &str = "budget_exceeded";
const DEFAULT_STATUS: u16 = 429;
const DEFAULT_PERIOD_MS: i64 = 86_400_000;
const TOKEN_RESERVATION: &str = "tokens";

#[derive(Debug)]
pub struct BudgetGuard {
    manifest: PluginManifest,
    requests: Option<u64>,
    tokens: Option<u64>,
    period_ms: i64,
    code: String,
    status: u16,
    window: Mutex<Window>,
}

#[derive(Debug, Default)]
struct Window {
    ends_at_ms: i64,
    requests: u64,
    tokens: u64,
}

impl Window {
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
    fn build(&self, config: Value) -> Result<BudgetGuard, PluginError> {
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
            requests: config.requests,
            tokens: config.tokens,
            period_ms,
            code,
            status,
            window: Mutex::new(Window::default()),
        })
    }
}

impl LlmPluginFactory for BudgetGuardFactory {
    fn manifest(&self) -> PluginManifest {
        let mut manifest = manifest(
            "llm-budget-guard",
            "Reject requests that exceed a soft per-scope request or token budget",
        );
        manifest.capabilities = vec![Capability::RequestBody];
        manifest
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        Ok(Arc::new(self.build(config)?))
    }
}

#[async_trait]
impl LlmPlugin for BudgetGuard {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
        let estimate = estimate_tokens(cx.request);
        let now = now_ms();
        let (rejected, code) = {
            let mut window = self
                .window
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            window.roll(now, self.period_ms);
            let requests_exceeded = self.requests.is_some_and(|limit| window.requests >= limit);
            let tokens_exceeded = self
                .tokens
                .is_some_and(|limit| window.tokens.saturating_add(estimate) > limit);
            if requests_exceeded || tokens_exceeded {
                (true, self.code.clone())
            } else {
                window.requests = window.requests.saturating_add(1);
                window.tokens = window.tokens.saturating_add(estimate);
                (false, String::new())
            }
        };
        if rejected {
            tracing::debug!(
                request_id = %cx.meta.request_id,
                code = %code,
                "llm budget rejected request"
            );
            return Ok(LlmAction::Reject(
                GatewayError::upstream(self.status, "request budget exhausted").with_code(code),
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
        let mut window = self
            .window
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        window.roll(now_ms(), self.period_ms);
        window.tokens = window
            .tokens
            .saturating_sub(reserved)
            .saturating_add(actual);
        *cx.state = Value::Null;
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

    fn meta() -> LlmRequestMeta<'static> {
        LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        }
    }

    fn guard(config: Value) -> BudgetGuard {
        BudgetGuardFactory.build(config).unwrap()
    }

    async fn request(plugin: &BudgetGuard, text: &str) -> LlmPluginResult {
        let mut request = ChatRequest::new("m", vec![Message::user(text)]);
        let meta = meta();
        let mut state = Value::Null;
        let mut cx = LlmRequestContext {
            meta: &meta,
            request: &mut request,
            state: &mut state,
        };
        plugin.on_request(&mut cx).await
    }

    #[tokio::test]
    async fn request_limit_rejects_with_stable_code() {
        let plugin = guard(json!({"requests": 1, "period": "day"}));
        assert!(matches!(
            request(&plugin, "hello").await.unwrap(),
            LlmAction::Continue
        ));
        let rejected = request(&plugin, "hello").await.unwrap();
        let LlmAction::Reject(error) = rejected else {
            panic!("second request should be rejected");
        };
        assert_eq!(error.status, 429);
        assert_eq!(error.code.as_deref(), Some("budget_exceeded"));
    }

    #[tokio::test]
    async fn token_budget_reconciles_actual_usage() {
        let plugin = guard(json!({"tokens": 100, "period": "hour"}));
        let meta = meta();
        let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
        let mut state = Value::Null;
        let mut cx = LlmRequestContext {
            meta: &meta,
            request: &mut request,
            state: &mut state,
        };
        plugin.on_request(&mut cx).await.unwrap();
        let reserved = plugin.window.lock().unwrap().tokens;
        assert!(reserved > 0);

        let usage = Usage {
            input_tokens: 2,
            output_tokens: 3,
            ..Usage::default()
        };
        let event = LlmLogEvent {
            model: "m",
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
        plugin.on_log(&mut cx).await;
        let window = plugin.window.lock().unwrap();
        assert_eq!(window.tokens, 5);
        assert_eq!(state, Value::Null);
    }

    #[tokio::test]
    async fn window_rolls_over() {
        let plugin = guard(json!({"requests": 1, "period": "minute"}));
        assert!(matches!(
            request(&plugin, "hi").await.unwrap(),
            LlmAction::Continue
        ));
        {
            let mut window = plugin.window.lock().unwrap();
            window.ends_at_ms = now_ms() - 1;
        }
        assert!(matches!(
            request(&plugin, "hi").await.unwrap(),
            LlmAction::Continue
        ));
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
