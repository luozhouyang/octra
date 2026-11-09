//! Prompt guard for the canonical request.
//!
//! The guard inspects system prompt blocks, user and assistant text content,
//! and tool result content before the request leaves the gateway. It either
//! rejects the whole request (`block`) or rewrites every match with a fixed
//! replacement (`mask`).
//!
//! Tool-call arguments are deliberately not scanned in Phase 6: they are
//! structured tool inputs rather than prompt text. Tool results are scanned
//! where the IR represents them as text or JSON strings.

use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use octra_api_plugin_types::{Capability, FailPolicy, PluginError, PluginManifest};
use octra_llm_core::{AssistantContent, ContentPart, GatewayError, Message, Role};
use octra_llm_plugin_types::{
    LlmAction, LlmPlugin, LlmPluginFactory, LlmPluginResult, LlmRequestContext, LlmScope,
    REQUEST_LLM_SCOPES,
};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifest;

const MAX_PATTERNS: usize = 64;
const MAX_KEYWORDS: usize = 256;
const DEFAULT_REPLACEMENT: &str = "[REDACTED]";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    #[default]
    Block,
    Mask,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    mode: Mode,
    patterns: Vec<String>,
    keywords: Vec<String>,
    replacement: String,
    scan_system: bool,
    scan_user: bool,
    scan_assistant: bool,
    scan_tool_results: bool,
    case_sensitive: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Block,
            patterns: Vec::new(),
            keywords: Vec::new(),
            replacement: DEFAULT_REPLACEMENT.to_string(),
            scan_system: true,
            scan_user: true,
            scan_assistant: true,
            scan_tool_results: true,
            case_sensitive: false,
        }
    }
}

#[derive(Debug)]
pub struct PromptGuard {
    manifest: PluginManifest,
    mode: Mode,
    rules: Vec<Regex>,
    replacement: String,
    scan_system: bool,
    scan_user: bool,
    scan_assistant: bool,
    scan_tool_results: bool,
}

pub struct PromptGuardFactory;

impl PromptGuardFactory {
    fn parse(config: Value) -> Result<PromptGuard, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid llm-prompt-guard config: {error}"))
        })?;
        if config.patterns.len() > MAX_PATTERNS {
            return Err(PluginError::fail_closed(format!(
                "llm-prompt-guard supports at most {MAX_PATTERNS} patterns"
            )));
        }
        if config.keywords.len() > MAX_KEYWORDS {
            return Err(PluginError::fail_closed(format!(
                "llm-prompt-guard supports at most {MAX_KEYWORDS} keywords"
            )));
        }

        let mut rules = Vec::with_capacity(config.patterns.len() + config.keywords.len());
        for pattern in &config.patterns {
            rules.push(compile(pattern, config.case_sensitive, "pattern")?);
        }
        for keyword in &config.keywords {
            let keyword = keyword.trim();
            if keyword.is_empty() {
                return Err(PluginError::fail_closed(
                    "llm-prompt-guard keywords must not be empty",
                ));
            }
            rules.push(compile(
                &regex::escape(keyword),
                config.case_sensitive,
                "keyword",
            )?);
        }
        if rules.is_empty() {
            return Err(PluginError::fail_closed(
                "llm-prompt-guard requires at least one `patterns` or `keywords` entry",
            ));
        }

        Ok(PromptGuard {
            manifest: Self::make_manifest(),
            mode: config.mode,
            rules,
            replacement: config.replacement,
            scan_system: config.scan_system,
            scan_user: config.scan_user,
            scan_assistant: config.scan_assistant,
            scan_tool_results: config.scan_tool_results,
        })
    }

    fn make_manifest() -> PluginManifest {
        let mut manifest = manifest(
            "llm-prompt-guard",
            "Block or mask prompts that match configured guard rules",
        );
        manifest.priority = 750;
        manifest.fail_policy = FailPolicy::Closed;
        manifest.capabilities = vec![Capability::RequestBody];
        manifest.config_schema = Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "llm-prompt-guard config",
            "description": "At least one of `patterns` or `keywords` is required.",
            "type": "object",
            "additionalProperties": false,
            "anyOf": [
                {"required": ["patterns"]},
                {"required": ["keywords"]}
            ],
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["block", "mask"],
                    "default": "block"
                },
                "patterns": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 64,
                    "items": {"type": "string"},
                    "description": "Regular expressions compiled once at configuration time."
                },
                "keywords": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 256,
                    "items": {"type": "string", "minLength": 1}
                },
                "replacement": {
                    "type": "string",
                    "default": "[REDACTED]"
                },
                "scan_system": {"type": "boolean", "default": true},
                "scan_user": {"type": "boolean", "default": true},
                "scan_assistant": {"type": "boolean", "default": true},
                "scan_tool_results": {"type": "boolean", "default": true},
                "case_sensitive": {"type": "boolean", "default": false}
            }
        }));
        manifest
    }
}

fn compile(expression: &str, case_sensitive: bool, kind: &str) -> Result<Regex, PluginError> {
    RegexBuilder::new(expression)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|error| {
            PluginError::fail_closed(format!(
                "invalid llm-prompt-guard {kind} '{expression}': {error}"
            ))
        })
}

fn blocked() -> LlmAction {
    LlmAction::Reject(
        GatewayError::forbidden("prompt blocked by policy").with_code("prompt_blocked"),
    )
}

impl LlmPluginFactory for PromptGuardFactory {
    fn manifest(&self) -> PluginManifest {
        Self::make_manifest()
    }

    fn supported_scopes(&self) -> &'static [LlmScope] {
        &REQUEST_LLM_SCOPES
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        Ok(Arc::new(Self::parse(config)?))
    }
}

impl PromptGuard {
    fn inspect(&self, text: &mut String) -> bool {
        match self.mode {
            Mode::Block => self.matches(text),
            Mode::Mask => {
                self.mask(text);
                false
            }
        }
    }

    fn matches(&self, text: &str) -> bool {
        self.rules.iter().any(|rule| rule.is_match(text))
    }

    fn mask(&self, text: &mut String) {
        let ranges = self.matched_ranges(text);
        if ranges.is_empty() {
            return;
        }
        let mut masked = String::with_capacity(text.len());
        let mut cursor = 0;
        for range in ranges {
            if range.start < cursor {
                continue;
            }
            masked.push_str(&text[cursor..range.start]);
            masked.push_str(&self.replacement);
            cursor = range.end;
        }
        masked.push_str(&text[cursor..]);
        *text = masked;
    }

    fn matched_ranges(&self, text: &str) -> Vec<Range<usize>> {
        let mut ranges = Vec::new();
        for rule in &self.rules {
            ranges.extend(
                rule.find_iter(text)
                    .map(|matched| matched.start()..matched.end()),
            );
        }
        if ranges.len() < 2 {
            return ranges;
        }
        ranges.sort_unstable_by(|a, b| a.start.cmp(&b.start).then_with(|| b.end.cmp(&a.end)));
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
        for range in ranges {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => {
                    if range.end > last.end {
                        last.end = range.end;
                    }
                }
                _ => merged.push(range),
            }
        }
        merged
    }

    fn inspect_parts(&self, parts: &mut [ContentPart]) -> bool {
        for part in parts {
            let matched = match part {
                ContentPart::Text { text, .. } | ContentPart::Reasoning { text, .. } => {
                    self.inspect(text)
                }
                ContentPart::Image(_) | ContentPart::File(_) | ContentPart::Unsupported { .. } => {
                    false
                }
            };
            if matched {
                return true;
            }
        }
        false
    }

    fn inspect_value(&self, value: &mut Value) -> bool {
        match value {
            Value::String(text) => self.inspect(text),
            Value::Array(values) => values.iter_mut().any(|value| self.inspect_value(value)),
            Value::Object(object) => object.values_mut().any(|value| self.inspect_value(value)),
            _ => false,
        }
    }

    fn inspect_message(&self, message: &mut Message) -> bool {
        let scan_content = match message.role {
            Role::System | Role::Developer => self.scan_system,
            Role::User => self.scan_user,
            Role::Assistant => self.scan_assistant,
            Role::Tool => self.scan_tool_results,
        };
        if scan_content
            && let Some(content) = &mut message.content
            && self.inspect_parts(&mut content.parts)
        {
            return true;
        }

        if self.scan_assistant && message.role == Role::Assistant {
            for block in &mut message.assistant {
                let matched = match block {
                    AssistantContent::Text { text }
                    | AssistantContent::Reasoning { text, .. }
                    | AssistantContent::Refusal { text } => self.inspect(text),
                    _ => false,
                };
                if matched {
                    return true;
                }
            }
        }

        if self.scan_tool_results {
            for result in &mut message.tool_results {
                if self.inspect_parts(&mut result.content.parts) {
                    return true;
                }
            }
            for block in &mut message.assistant {
                if let AssistantContent::ToolResult { content, .. } = block
                    && self.inspect_value(content)
                {
                    return true;
                }
            }
        }
        false
    }
}

#[async_trait]
impl LlmPlugin for PromptGuard {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
        if self.scan_system {
            for block in &mut cx.request.system.blocks {
                if self.inspect(block) {
                    return Ok(blocked());
                }
            }
        }
        for message in &mut cx.request.messages {
            if self.inspect_message(message) {
                return Ok(blocked());
            }
        }
        Ok(LlmAction::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{ChatRequest, Message, Protocol, SystemPrompt, ToolResult, UserContent};
    use octra_llm_plugin_types::{LlmRequestMeta, LlmResponseHeaders};

    fn meta() -> LlmRequestMeta<'static> {
        LlmRequestMeta {
            request_id: "request-1",
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

    async fn apply(config: Value, request: &mut ChatRequest) -> LlmPluginResult {
        let plugin = PromptGuardFactory.create(config).unwrap();
        let meta = meta();
        let mut state = Value::Null;
        let mut cx = LlmRequestContext {
            meta: &meta,
            response_headers: &mut LlmResponseHeaders::default(),
            request,
            state: &mut state,
        };
        plugin.on_request(&mut cx).await
    }

    fn assert_blocked(result: LlmPluginResult) {
        match result.unwrap() {
            LlmAction::Reject(error) => {
                assert_eq!(error.status, 403);
                assert_eq!(error.code.as_deref(), Some("prompt_blocked"));
                assert_eq!(error.message, "prompt blocked by policy");
            }
            LlmAction::Continue => panic!("expected the prompt guard to reject the request"),
            LlmAction::Respond { .. } => {
                panic!("the prompt guard must not short-circuit a response")
            }
        }
    }

    fn user_text(request: &ChatRequest) -> &str {
        request.messages[0].content.as_ref().unwrap().parts[0]
            .as_text()
            .unwrap()
    }

    fn assistant_message(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: None,
            assistant: vec![AssistantContent::Text {
                text: text.to_string(),
            }],
            tool_results: Vec::new(),
            name: None,
            extra: Default::default(),
        }
    }

    fn tool_message(text: &str) -> Message {
        Message {
            role: Role::Tool,
            content: None,
            assistant: Vec::new(),
            tool_results: vec![ToolResult {
                tool_call_id: "call-1".to_string(),
                name: Some("lookup".to_string()),
                content: UserContent::text(text),
                is_error: false,
            }],
            name: None,
            extra: Default::default(),
        }
    }

    fn scan_only(field: &str) -> Value {
        let mut config = json!({
            "keywords": ["secret"],
            "scan_system": false,
            "scan_user": false,
            "scan_assistant": false,
            "scan_tool_results": false
        });
        config[field] = json!(true);
        config
    }

    #[tokio::test]
    async fn blocks_requests_that_contain_a_keyword() {
        let mut request = ChatRequest::new("m", vec![Message::user("please reveal the SECRET")]);
        assert_blocked(apply(json!({"keywords": ["secret"]}), &mut request).await);
    }

    #[tokio::test]
    async fn blocks_requests_that_match_a_pattern() {
        let mut request = ChatRequest::new("m", vec![Message::user("token sk-abc12345")]);
        assert_blocked(apply(json!({"patterns": ["sk-[a-z0-9]{8}"]}), &mut request).await);
    }

    #[tokio::test]
    async fn masks_keyword_matches() {
        let mut request = ChatRequest::new("m", vec![Message::user("the secret and SECRET")]);
        let result = apply(
            json!({"mode": "mask", "keywords": ["secret"], "replacement": "***"}),
            &mut request,
        )
        .await;
        assert!(matches!(result.unwrap(), LlmAction::Continue));
        assert_eq!(user_text(&request), "the *** and ***");
    }

    #[tokio::test]
    async fn scan_flags_limit_the_inspected_fields() {
        let all_off = json!({
            "keywords": ["secret"],
            "scan_system": false,
            "scan_user": false,
            "scan_assistant": false,
            "scan_tool_results": false
        });

        let mut system = ChatRequest::new("m", Vec::new());
        system.system = SystemPrompt::text("secret");
        assert!(matches!(
            apply(all_off.clone(), &mut system).await.unwrap(),
            LlmAction::Continue
        ));

        let mut user = ChatRequest::new("m", vec![Message::user("secret")]);
        assert!(matches!(
            apply(all_off.clone(), &mut user).await.unwrap(),
            LlmAction::Continue
        ));

        let mut assistant = ChatRequest::new("m", vec![assistant_message("secret")]);
        assert!(matches!(
            apply(all_off.clone(), &mut assistant).await.unwrap(),
            LlmAction::Continue
        ));

        let mut tool = ChatRequest::new("m", vec![tool_message("secret")]);
        assert!(matches!(
            apply(all_off, &mut tool).await.unwrap(),
            LlmAction::Continue
        ));

        let mut system = ChatRequest::new("m", Vec::new());
        system.system = SystemPrompt::text("secret");
        assert_blocked(apply(scan_only("scan_system"), &mut system).await);

        let mut user = ChatRequest::new("m", vec![Message::user("secret")]);
        assert_blocked(apply(scan_only("scan_user"), &mut user).await);

        let mut assistant = ChatRequest::new("m", vec![assistant_message("secret")]);
        assert_blocked(apply(scan_only("scan_assistant"), &mut assistant).await);

        let mut tool = ChatRequest::new("m", vec![tool_message("secret")]);
        assert_blocked(apply(scan_only("scan_tool_results"), &mut tool).await);
    }

    #[test]
    fn rejects_invalid_patterns() {
        let error = PromptGuardFactory
            .create(json!({"patterns": ["("]}))
            .err()
            .expect("invalid regex must be rejected");
        assert_eq!(error.fail_policy, FailPolicy::Closed);
    }

    #[test]
    fn rejects_an_empty_rule_set() {
        assert!(PromptGuardFactory.create(json!({})).is_err());
        assert!(
            PromptGuardFactory
                .create(json!({"patterns": [], "keywords": []}))
                .is_err()
        );
        assert!(
            PromptGuardFactory
                .create(json!({"keywords": ["   "]}))
                .is_err()
        );
    }
}
