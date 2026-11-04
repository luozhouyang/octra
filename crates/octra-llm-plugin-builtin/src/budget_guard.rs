//! Soft per-scope request and token budget guard.
//!
//! The guard is a policy at the edge: it rejects early with a stable error
//! code, while authoritative account quota stays in the core account pool.
//! Counters live in host state keyed by the configured instance, so they
//! survive chain reloads and are flushed to storage for restarts. A rolling
//! window starts at the first admitted request.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use octra_api_plugin_types::{Capability, PluginError, PluginManifest};
use octra_llm_core::{GatewayError, estimate_tokens};
use octra_llm_plugin_types::{
    LlmAction, LlmErrorContext, LlmLogContext, LlmPlugin, LlmPluginCreateContext, LlmPluginFactory,
    LlmPluginResult, LlmPluginStateStore, LlmRequestContext, LlmScope, REQUEST_LLM_SCOPES,
};
use serde::{Deserialize, Serialize};
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
    window: Arc<Mutex<Window>>,
    state: Arc<dyn LlmPluginStateStore>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Window {
    ends_at_ms: i64,
    requests: u64,
    tokens: u64,
}

/// Windows are shared by stable instance key so a reload reuses the live
/// counter instead of starting from zero.
fn windows() -> &'static Mutex<HashMap<String, Arc<Mutex<Window>>>> {
    static WINDOWS: OnceLock<Mutex<HashMap<String, Arc<Mutex<Window>>>>> = OnceLock::new();
    WINDOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn state_key(instance_key: &str) -> String {
    format!("budget:{instance_key}")
}

fn shared_window(instance_key: &str, state: &Arc<dyn LlmPluginStateStore>) -> Arc<Mutex<Window>> {
    let mut windows = windows()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(window) = windows.get(instance_key) {
        return window.clone();
    }
    let window = state
        .get(&state_key(instance_key))
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    let window = Arc::new(Mutex::new(window));
    windows.insert(instance_key.to_string(), window.clone());
    window
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
    fn build(
        &self,
        config: Value,
        instance_key: &str,
        state: Arc<dyn LlmPluginStateStore>,
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
            window: shared_window(instance_key, &state),
            state,
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
            Arc::new(MemoryStateStore::default()),
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
            context.state.clone(),
        )?))
    }
}

/// Minimal store used when no host store is supplied.
#[derive(Debug, Default)]
struct MemoryStateStore {
    values: Mutex<HashMap<String, Value>>,
}

impl LlmPluginStateStore for MemoryStateStore {
    fn get(&self, key: &str) -> Option<Value> {
        self.values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .cloned()
    }

    fn put(&self, key: &str, value: Value) {
        self.values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key.to_string(), value);
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
        let (rejected, code, snapshot) = {
            let mut window = self
                .window
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            window.roll(now, self.period_ms);
            let requests_exceeded = self.requests.is_some_and(|limit| window.requests >= limit);
            let tokens_exceeded = self
                .tokens
                .is_some_and(|limit| window.tokens.saturating_add(estimate) > limit);
            let rejected = requests_exceeded || tokens_exceeded;
            if !rejected {
                window.requests = window.requests.saturating_add(1);
                window.tokens = window.tokens.saturating_add(estimate);
            }
            (rejected, self.code.clone(), window.clone())
        };
        self.persist(&snapshot);
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
        let snapshot = {
            let mut window = self
                .window
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            window.roll(now_ms(), self.period_ms);
            window.tokens = window
                .tokens
                .saturating_sub(reserved)
                .saturating_add(actual);
            window.clone()
        };
        self.persist(&snapshot);
        *cx.state = Value::Null;
    }

    /// Refunds the token reservation when the request fails before a log event
    /// can reconcile it. `on_log` clears the same slot, so this is idempotent.
    async fn on_error(&self, cx: &mut LlmErrorContext<'_>) -> LlmPluginResult {
        let Some(reserved) = cx.state.get(TOKEN_RESERVATION).and_then(Value::as_u64) else {
            return Ok(LlmAction::Continue);
        };
        let snapshot = {
            let mut window = self
                .window
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            window.roll(now_ms(), self.period_ms);
            window.tokens = window.tokens.saturating_sub(reserved);
            window.clone()
        };
        self.persist(&snapshot);
        *cx.state = Value::Null;
        Ok(LlmAction::Continue)
    }
}

impl BudgetGuard {
    fn persist(&self, window: &Window) {
        if let Ok(value) = serde_json::to_value(window) {
            self.state.put(&state_key(&self.key), value);
        }
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
    use octra_llm_plugin_types::LlmResponseHeaders;
    use octra_llm_plugin_types::{LlmLogEvent, LlmRequestMeta};

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

    fn guard(config: Value) -> BudgetGuard {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let key = format!("test-instance-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        BudgetGuardFactory
            .build(config, &key, Arc::new(MemoryStateStore::default()))
            .unwrap()
    }

    #[tokio::test]
    async fn counters_survive_instance_recreation() {
        let state: Arc<dyn LlmPluginStateStore> = Arc::new(MemoryStateStore::default());
        let first = BudgetGuardFactory
            .build(json!({"requests": 1}), "reload-instance", state.clone())
            .unwrap();
        assert!(matches!(
            request(&first, "hello").await.unwrap(),
            LlmAction::Continue
        ));

        // Simulates a chain reload: a new instance with the same key reuses
        // the live window.
        let second = BudgetGuardFactory
            .build(json!({"requests": 1}), "reload-instance", state)
            .unwrap();
        assert!(matches!(
            request(&second, "hello").await.unwrap(),
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
    async fn error_refunds_the_token_reservation_once() {
        let plugin = guard(json!({"tokens": 100, "period": "hour"}));
        let meta = meta();
        let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
        let mut state = Value::Null;
        let mut cx = LlmRequestContext {
            meta: &meta,
            headers: &mut LlmResponseHeaders::default(),
            request: &mut request,
            state: &mut state,
        };
        plugin.on_request(&mut cx).await.unwrap();
        assert!(plugin.window.lock().unwrap().tokens > 0);

        let mut error = GatewayError::upstream(500, "boom");
        let mut cx = octra_llm_plugin_types::LlmErrorContext {
            meta: &meta,
            provider_id: None,
            account_id: None,
            error: &mut error,
            state: &mut state,
        };
        plugin.on_error(&mut cx).await.unwrap();
        assert_eq!(plugin.window.lock().unwrap().tokens, 0);
        assert!(cx.state.is_null());

        // A later on_log/on_error for the same request must not refund twice.
        plugin.on_error(&mut cx).await.unwrap();
        assert_eq!(plugin.window.lock().unwrap().tokens, 0);
    }

    #[tokio::test]
    async fn token_budget_reconciles_actual_usage() {
        let plugin = guard(json!({"tokens": 100, "period": "hour"}));
        let meta = meta();
        let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
        let mut state = Value::Null;
        let mut cx = LlmRequestContext {
            meta: &meta,
            headers: &mut LlmResponseHeaders::default(),
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
