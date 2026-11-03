//! Tags requests with a routing intent hint.
//!
//! The hint is advisory: the core router combines it with model capabilities,
//! account health and quota. An explicit client header or an existing
//! `octra_intent` request field wins over the classifier and the configured
//! rules.
//!
//! An optional classifier delegates the decision to an OpenAI-compatible chat
//! endpoint (for example a small hosted model). Classifier failures never fail
//! the request; the plugin falls back to the rule-based hint.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use octra_api_plugin_types::{Capability, PluginError, PluginManifest};
use octra_llm_core::{ChatRequest, ContentPart, Role, estimate_tokens};
use octra_llm_plugin_types::{
    LlmAction, LlmPlugin, LlmPluginFactory, LlmPluginResult, LlmRequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifest;

const INTENT_FIELD: &str = "octra_intent";
const DEFAULT_CLASSIFIER_TIMEOUT_MS: u64 = 1_500;
const DEFAULT_CLASSIFIER_MAX_CHARS: usize = 4_000;
const DEFAULT_CLASSIFIER_CACHE_ENTRIES: usize = 512;
const MAX_CLASSIFIER_TIMEOUT_MS: u64 = 10_000;
const MAX_CLASSIFIER_CHARS: usize = 32_000;
const MAX_CLASSIFIER_CACHE_ENTRIES: usize = 65_536;

#[derive(Debug)]
pub struct IntentHint {
    manifest: PluginManifest,
    default: Option<String>,
    ignore_header: bool,
    rules: Vec<Rule>,
    classifier: Option<Classifier>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    intent: String,
    #[serde(default)]
    has_tools: Option<bool>,
    #[serde(default)]
    has_images: Option<bool>,
    #[serde(default)]
    reasoning_effort: Option<bool>,
    #[serde(default)]
    min_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    default: Option<String>,
    ignore_header: bool,
    rules: Vec<Rule>,
    classifier: Option<ClassifierConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ClassifierConfig {
    base_url: String,
    model: String,
    labels: Vec<String>,
    /// Resolved by the plugin host before `create`; kept for validation.
    secret_ref: Option<String>,
    secret: Option<String>,
    timeout_ms: Option<u64>,
    max_chars: Option<usize>,
    cache_entries: Option<usize>,
    system_prompt: Option<String>,
}

#[derive(Debug)]
struct Classifier {
    endpoint: String,
    model: String,
    labels: Vec<String>,
    system_prompt: String,
    secret: Option<String>,
    timeout: Duration,
    max_chars: usize,
    cache_entries: usize,
    cache: Mutex<HashMap<u64, String>>,
    client: reqwest::Client,
}

pub struct IntentHintFactory;

impl LlmPluginFactory for IntentHintFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "llm-intent-hint",
            "Tag requests with a routing intent hint for core model selection",
        )
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid intent hint config: {error}"))
        })?;
        let default = normalize_intent(config.default)?;
        let mut rules = Vec::with_capacity(config.rules.len());
        for rule in config.rules {
            let intent = normalize_intent(Some(rule.intent))?.expect("intent is required");
            if rule.min_tokens == Some(0) {
                return Err(PluginError::fail_closed(
                    "llm-intent-hint `min_tokens` must be positive",
                ));
            }
            rules.push(Rule {
                intent,
                has_tools: rule.has_tools,
                has_images: rule.has_images,
                reasoning_effort: rule.reasoning_effort,
                min_tokens: rule.min_tokens,
            });
        }
        let mut manifest = self.manifest();
        let classifier = match config.classifier {
            Some(classifier) => {
                let classifier = Classifier::from_config(classifier)?;
                // The host applies the manifest timeout to this stage, so it
                // must exceed the classifier's own request deadline.
                manifest.timeout_ms = (classifier.timeout.as_millis() as u64).saturating_add(200);
                if !manifest.capabilities.contains(&Capability::Network) {
                    manifest.capabilities.push(Capability::Network);
                }
                Some(classifier)
            }
            None => None,
        };
        Ok(Arc::new(IntentHint {
            manifest,
            default,
            ignore_header: config.ignore_header,
            rules,
            classifier,
        }))
    }
}

impl Classifier {
    fn from_config(config: ClassifierConfig) -> Result<Self, PluginError> {
        let base_url = config.base_url.trim().trim_end_matches('/');
        if base_url.is_empty() {
            return Err(PluginError::fail_closed(
                "llm-intent-hint classifier `base_url` is required",
            ));
        }
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(PluginError::fail_closed(
                "llm-intent-hint classifier `base_url` must be an HTTP(S) URL",
            ));
        }
        let model = config.model.trim();
        if model.is_empty() {
            return Err(PluginError::fail_closed(
                "llm-intent-hint classifier `model` is required",
            ));
        }
        let labels = normalize_labels(&config.labels)?;
        let timeout_ms = config
            .timeout_ms
            .unwrap_or(DEFAULT_CLASSIFIER_TIMEOUT_MS)
            .clamp(100, MAX_CLASSIFIER_TIMEOUT_MS);
        let max_chars = config
            .max_chars
            .unwrap_or(DEFAULT_CLASSIFIER_MAX_CHARS)
            .clamp(1, MAX_CLASSIFIER_CHARS);
        let cache_entries = config
            .cache_entries
            .unwrap_or(DEFAULT_CLASSIFIER_CACHE_ENTRIES)
            .min(MAX_CLASSIFIER_CACHE_ENTRIES);
        let system_prompt = config
            .system_prompt
            .map(|prompt| prompt.trim().to_string())
            .filter(|prompt| !prompt.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "Classify the user's request into exactly one of these intents: {}. Reply with the label only.",
                    labels.join(", ")
                )
            });
        let secret = config
            .secret
            .as_deref()
            .map(str::trim)
            .filter(|secret| !secret.is_empty())
            .map(str::to_string);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .map_err(|error| {
                PluginError::fail_closed(format!(
                    "cannot build the intent classifier client: {error}"
                ))
            })?;
        Ok(Self {
            endpoint: format!("{base_url}/chat/completions"),
            model: model.to_string(),
            labels,
            system_prompt,
            secret,
            timeout: Duration::from_millis(timeout_ms),
            max_chars,
            cache_entries,
            cache: Mutex::new(HashMap::new()),
            client,
        })
    }

    async fn classify(&self, text: &str) -> Option<String> {
        let text = truncate_chars(text, self.max_chars);
        let key = self.cache_key(&text);
        if let Some(cached) = self.cached(key) {
            return Some(cached);
        }
        let body = json!({
            "model": self.model,
            "temperature": 0,
            "max_tokens": 16,
            "messages": [
                {"role": "system", "content": self.system_prompt},
                {"role": "user", "content": text},
            ],
        });
        let mut request = self.client.post(&self.endpoint).json(&body);
        if let Some(secret) = &self.secret {
            request = request.bearer_auth(secret);
        }
        let result = tokio::time::timeout(self.timeout, async {
            let response = request.send().await.map_err(|error| error.to_string())?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("HTTP {status}"));
            }
            let value: Value = response.json().await.map_err(|error| error.to_string())?;
            value
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "response has no message content".to_string())
        })
        .await;
        let content = match result {
            Ok(Ok(content)) => content,
            Ok(Err(reason)) => {
                tracing::warn!(reason, "intent classifier request failed");
                return None;
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = self.timeout.as_millis() as u64,
                    "intent classifier timed out"
                );
                return None;
            }
        };
        let label = self.match_label(&content)?;
        self.remember(key, &label);
        Some(label)
    }

    fn match_label(&self, content: &str) -> Option<String> {
        let cleaned = content
            .trim()
            .trim_matches(|character: char| {
                character.is_whitespace()
                    || matches!(character, '"' | '\'' | '`' | '.' | ',' | ':' | ';')
            })
            .to_lowercase();
        if let Some(label) = self
            .labels
            .iter()
            .find(|label| label.eq_ignore_ascii_case(&cleaned))
        {
            return Some(label.clone());
        }
        let tokens: Vec<&str> = cleaned
            .split(|character: char| {
                !(character.is_alphanumeric() || character == '_' || character == '-')
            })
            .filter(|token| !token.is_empty())
            .collect();
        self.labels
            .iter()
            .find(|label| tokens.iter().any(|token| token.eq_ignore_ascii_case(label)))
            .cloned()
    }

    fn cache_key(&self, text: &str) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.endpoint.hash(&mut hasher);
        self.model.hash(&mut hasher);
        text.hash(&mut hasher);
        hasher.finish()
    }

    fn cached(&self, key: u64) -> Option<String> {
        self.cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(&key).cloned())
    }

    fn remember(&self, key: u64, label: &str) {
        if self.cache_entries == 0 {
            return;
        }
        if let Ok(mut cache) = self.cache.lock() {
            if cache.len() >= self.cache_entries {
                cache.clear();
            }
            cache.insert(key, label.to_string());
        }
    }
}

#[async_trait]
impl LlmPlugin for IntentHint {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
        if !self.ignore_header
            && let Some(intent) = normalize_intent(cx.meta.intent.map(str::to_string))?
        {
            set_intent(cx.request, &intent);
            return Ok(LlmAction::Continue);
        }
        if existing_intent(cx.request).is_some() {
            return Ok(LlmAction::Continue);
        }
        if let Some(classifier) = &self.classifier
            && let Some(text) = last_user_text(cx.request)
            && let Some(intent) = classifier.classify(&text).await
        {
            set_intent(cx.request, &intent);
            return Ok(LlmAction::Continue);
        }
        let tokens = estimate_tokens(cx.request);
        let matched = self
            .rules
            .iter()
            .find(|rule| rule.matches(cx.request, tokens))
            .map(|rule| rule.intent.clone())
            .or_else(|| self.default.clone());
        if let Some(intent) = matched {
            set_intent(cx.request, &intent);
        }
        Ok(LlmAction::Continue)
    }
}

impl Rule {
    fn matches(&self, request: &ChatRequest, tokens: u64) -> bool {
        self.has_tools
            .is_none_or(|expected| expected == !request.tools.is_empty())
            && self
                .has_images
                .is_none_or(|expected| expected == has_images(request))
            && self.reasoning_effort.is_none_or(|expected| {
                expected
                    == request
                        .reasoning_effort
                        .as_deref()
                        .is_some_and(|v| !v.is_empty())
            })
            && self.min_tokens.is_none_or(|min| tokens >= min)
    }
}

fn normalize_intent(value: Option<String>) -> Result<Option<String>, PluginError> {
    match value {
        None => Ok(None),
        Some(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Err(PluginError::fail_closed(
                    "llm-intent-hint intent must not be empty",
                ));
            }
            Ok(Some(value.to_string()))
        }
    }
}

fn normalize_labels(labels: &[String]) -> Result<Vec<String>, PluginError> {
    let mut normalized = Vec::with_capacity(labels.len());
    for label in labels {
        let label = label.trim();
        if label.is_empty() {
            return Err(PluginError::fail_closed(
                "llm-intent-hint classifier labels must not be empty",
            ));
        }
        if !normalized
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(label))
        {
            normalized.push(label.to_string());
        }
    }
    if normalized.is_empty() {
        return Err(PluginError::fail_closed(
            "llm-intent-hint classifier needs at least one label",
        ));
    }
    Ok(normalized)
}

fn last_user_text(request: &ChatRequest) -> Option<String> {
    let message = request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)?;
    let content = message.content.as_ref()?;
    let text = content
        .parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

fn existing_intent(request: &ChatRequest) -> Option<&str> {
    request
        .extra
        .get(INTENT_FIELD)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn set_intent(request: &mut ChatRequest, intent: &str) {
    request
        .extra
        .insert(INTENT_FIELD.to_string(), json!(intent));
}

fn has_images(request: &ChatRequest) -> bool {
    request
        .messages
        .iter()
        .filter_map(|message| message.content.as_ref())
        .any(|content| {
            content
                .parts
                .iter()
                .any(|part| matches!(part, ContentPart::Image(_)))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{FunctionDefinition, Message, Protocol, Tool};
    use octra_llm_plugin_types::LlmRequestMeta;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn meta(intent: Option<&str>) -> LlmRequestMeta<'_> {
        LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent,
        }
    }

    fn tool() -> Tool {
        Tool::function(FunctionDefinition {
            name: "lookup".into(),
            ..FunctionDefinition::default()
        })
    }

    async fn apply_with(
        plugin: &Arc<dyn LlmPlugin>,
        intent: Option<&str>,
        request: &mut ChatRequest,
    ) -> Option<String> {
        let meta = meta(intent);
        let mut state = Value::Null;
        let mut cx = LlmRequestContext {
            meta: &meta,
            request,
            state: &mut state,
        };
        plugin.on_request(&mut cx).await.unwrap();
        existing_intent(cx.request).map(str::to_string)
    }

    async fn apply(
        config: Value,
        intent: Option<&str>,
        request: &mut ChatRequest,
    ) -> Option<String> {
        let plugin = IntentHintFactory.create(config).unwrap();
        apply_with(&plugin, intent, request).await
    }

    /// Minimal OpenAI-compatible endpoint: responds to every request with the
    /// same chat completion body and counts connections.
    async fn classifier_server(body: &'static str) -> (String, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 8 * 1024];
                    let _ = socket.read(&mut buffer).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (format!("http://{address}/v1"), hits)
    }

    fn classifier_config(base_url: &str) -> Value {
        json!({
            "classifier": {
                "base_url": base_url,
                "model": "llama-3.1-8b-instant",
                "labels": ["chat", "code"],
            },
            "rules": [{"intent": "chat", "min_tokens": 1}]
        })
    }

    #[tokio::test]
    async fn header_intent_wins_over_rules() {
        let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
        request.tools.push(tool());
        let intent = apply(
            json!({"rules": [{"intent": "tool_use", "has_tools": true}]}),
            Some("vision"),
            &mut request,
        )
        .await;
        assert_eq!(intent.as_deref(), Some("vision"));
    }

    #[tokio::test]
    async fn first_matching_rule_and_default_apply() {
        let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
        request.tools.push(tool());
        let config = json!({
            "default": "chat",
            "rules": [
                {"intent": "vision", "has_images": true},
                {"intent": "tool_use", "has_tools": true}
            ]
        });
        let intent = apply(config.clone(), None, &mut request).await;
        assert_eq!(intent.as_deref(), Some("tool_use"));

        let mut plain = ChatRequest::new("m", vec![Message::user("hi")]);
        let intent = apply(config, None, &mut plain).await;
        assert_eq!(intent.as_deref(), Some("chat"));
    }

    #[tokio::test]
    async fn existing_intent_is_preserved() {
        let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
        request
            .extra
            .insert(INTENT_FIELD.to_string(), json!("coding"));
        let intent = apply(
            json!({"default": "chat", "ignore_header": true}),
            None,
            &mut request,
        )
        .await;
        assert_eq!(intent.as_deref(), Some("coding"));
    }

    #[tokio::test]
    async fn classifier_label_beats_the_rules() {
        let (base_url, _hits) =
            classifier_server(r#"{"choices":[{"message":{"content":"code"}}]}"#).await;
        let mut request = ChatRequest::new("m", vec![Message::user("write a rust function")]);
        let intent = apply(classifier_config(&base_url), None, &mut request).await;
        assert_eq!(intent.as_deref(), Some("code"));
    }

    #[tokio::test]
    async fn classifier_failure_falls_back_to_the_rules() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let mut request = ChatRequest::new("m", vec![Message::user("hello there")]);
        let intent = apply(
            classifier_config(&format!("http://{address}/v1")),
            None,
            &mut request,
        )
        .await;
        assert_eq!(intent.as_deref(), Some("chat"));
    }

    #[tokio::test]
    async fn classifier_results_are_cached_per_instance() {
        let (base_url, hits) =
            classifier_server(r#"{"choices":[{"message":{"content":"code"}}]}"#).await;
        let plugin = IntentHintFactory
            .create(classifier_config(&base_url))
            .unwrap();

        let mut first = ChatRequest::new("m", vec![Message::user("same question")]);
        let intent = apply_with(&plugin, None, &mut first).await;
        assert_eq!(intent.as_deref(), Some("code"));

        let mut second = ChatRequest::new("m", vec![Message::user("same question")]);
        let intent = apply_with(&plugin, None, &mut second).await;
        assert_eq!(intent.as_deref(), Some("code"));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn classifier_ignores_unmatched_answers() {
        let (base_url, _hits) =
            classifier_server(r#"{"choices":[{"message":{"content":"banana"}}]}"#).await;
        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        let intent = apply(classifier_config(&base_url), None, &mut request).await;
        assert_eq!(intent.as_deref(), Some("chat"));
    }

    #[test]
    fn invalid_config_is_rejected() {
        assert!(
            IntentHintFactory
                .create(json!({"rules": [{"intent": ""}]}))
                .is_err()
        );
        assert!(
            IntentHintFactory
                .create(json!({"rules": [{"intent": "x", "min_tokens": 0}]}))
                .is_err()
        );
        assert!(
            IntentHintFactory
                .create(json!({"rules": [{"intent": "x", "unknown": true}]}))
                .is_err()
        );
        for classifier in [
            json!({"model": "m", "labels": ["chat"]}),
            json!({"base_url": "ftp://example.invalid", "model": "m", "labels": ["chat"]}),
            json!({"base_url": "http://example.invalid", "model": "", "labels": ["chat"]}),
            json!({"base_url": "http://example.invalid", "model": "m", "labels": []}),
            json!({"base_url": "http://example.invalid", "model": "m", "labels": [" "] }),
        ] {
            assert!(
                IntentHintFactory
                    .create(json!({"classifier": classifier}))
                    .is_err()
            );
        }
    }
}
