//! Tags requests with a routing intent hint.
//!
//! The hint is advisory: the core router combines it with model capabilities,
//! account health and quota. An explicit client header or an existing
//! `octra_intent` request field wins over the configured rules.

use std::sync::Arc;

use async_trait::async_trait;
use octra_api_plugin_types::{PluginError, PluginManifest};
use octra_llm_core::{ChatRequest, ContentPart, estimate_tokens};
use octra_llm_plugin_types::{
    LlmAction, LlmPlugin, LlmPluginFactory, LlmPluginResult, LlmRequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifest;

const INTENT_FIELD: &str = "octra_intent";

#[derive(Debug)]
pub struct IntentHint {
    manifest: PluginManifest,
    default: Option<String>,
    ignore_header: bool,
    rules: Vec<Rule>,
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
        Ok(Arc::new(IntentHint {
            manifest: self.manifest(),
            default,
            ignore_header: config.ignore_header,
            rules,
        }))
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

    async fn apply(
        config: Value,
        intent: Option<&str>,
        request: &mut ChatRequest,
    ) -> Option<String> {
        let plugin = IntentHintFactory.create(config).unwrap();
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
    }
}
