//! PII redaction for requests, responses and streams.
//!
//! In `reversible` mode the upstream sees stable placeholders and the client
//! sees the original values. The mapping lives only in the plugin's request
//! state and is dropped when the request finishes. In `irreversible` mode the
//! placeholders are never restored.
//!
//! Errors and logs are never restored: upstream error bodies often echo the
//! request, and restoring them would leak PII back through an unexpected path.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use octra_api_plugin_types::{Capability, FailPolicy, PluginError, PluginManifest};
use octra_llm_core::{AssistantContent, ChatRequest, ContentPart, Message, StreamEvent};
use octra_llm_plugin_types::{
    LlmAction, LlmPlugin, LlmPluginFactory, LlmPluginResult, LlmProviderRequestContext,
    LlmRequestContext, LlmResponseContext, LlmStreamAction, LlmStreamContext, LlmStreamResult,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest;

const PLACEHOLDER_PREFIX: &str = "[[octra:";
const PLACEHOLDER_SUFFIX: &str = "]]";
const DEFAULT_MAX_PLACEHOLDERS: usize = 1024;
const HARD_MAX_PLACEHOLDERS: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiiKind {
    Email,
    Phone,
    IdCard,
    BankCard,
    ApiKey,
    Ip,
    Custom(String),
}

impl PiiKind {
    fn label(&self) -> &str {
        match self {
            Self::Email => "email",
            Self::Phone => "phone",
            Self::IdCard => "id_card",
            Self::BankCard => "bank_card",
            Self::ApiKey => "api_key",
            Self::Ip => "ip",
            Self::Custom(name) => name,
        }
    }
}

/// Part of the detector contract; consumers may use confidence or detector
/// provenance when chaining multiple detectors.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Detection {
    pub kind: PiiKind,
    pub start: usize,
    pub end: usize,
    pub confidence: f32,
    pub detector: &'static str,
}

pub trait PiiDetector: Send + Sync {
    fn detect(&self, text: &str) -> Vec<Detection>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Reversible,
    Irreversible,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct State {
    nonce: String,
    mapping: BTreeMap<String, String>,
    max_len: usize,
    next: usize,
    pending: BTreeMap<String, String>,
}

impl State {
    fn load(value: &Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }

    fn store(&self, value: &mut Value) {
        *value = serde_json::to_value(self).unwrap_or(Value::Null);
    }
}

#[derive(Debug, Clone)]
struct Rules {
    email: Option<Regex>,
    phone_cn: Option<Regex>,
    phone_intl: Option<Regex>,
    id_card: Option<Regex>,
    bank_card: Option<Regex>,
    api_key: Option<Regex>,
    ipv4: Option<Regex>,
    custom: Vec<(String, Regex)>,
}

#[derive(Debug)]
pub struct RuleDetector {
    rules: Rules,
}

impl RuleDetector {
    fn compile(flags: &RulesConfig, custom: &[(String, String)]) -> Result<Self, PluginError> {
        let build = |enabled: bool, pattern: &str| -> Result<Option<Regex>, PluginError> {
            if !enabled {
                return Ok(None);
            }
            Regex::new(pattern)
                .map(Some)
                .map_err(|error| PluginError::fail_closed(format!("invalid PII regex: {error}")))
        };
        let mut compiled = Vec::with_capacity(custom.len());
        for (name, pattern) in custom {
            let regex = Regex::new(pattern).map_err(|error| {
                PluginError::fail_closed(format!("invalid custom PII pattern '{name}': {error}"))
            })?;
            compiled.push((name.clone(), regex));
        }
        Ok(Self {
            rules: Rules {
                email: build(
                    flags.email,
                    r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}",
                )?,
                phone_cn: build(flags.phone, r"(?:\+?86[-\s]?)?1[3-9]\d{9}")?,
                phone_intl: build(flags.phone, r"\+\d{7,15}")?,
                id_card: build(flags.id_card, r"\b\d{17}[0-9Xx]\b")?,
                bank_card: build(flags.bank_card, r"\b\d{16,19}\b")?,
                api_key: build(flags.api_key, r"\b(?:sk|ak|pk)[-_][A-Za-z0-9]{16,}\b")?,
                ipv4: build(flags.ip, r"\b(?:\d{1,3}\.){3}\d{1,3}\b")?,
                custom: compiled,
            },
        })
    }
}

impl PiiDetector for RuleDetector {
    fn detect(&self, text: &str) -> Vec<Detection> {
        let mut detections = Vec::new();
        let mut push = |kind: PiiKind, mat: regex::Match<'_>| {
            detections.push(Detection {
                kind,
                start: mat.start(),
                end: mat.end(),
                confidence: 0.9,
                detector: "rules",
            });
        };
        if let Some(regex) = &self.rules.email {
            for mat in regex.find_iter(text) {
                push(PiiKind::Email, mat);
            }
        }
        for regex in [self.rules.phone_cn.as_ref(), self.rules.phone_intl.as_ref()]
            .into_iter()
            .flatten()
        {
            for mat in regex.find_iter(text) {
                push(PiiKind::Phone, mat);
            }
        }
        if let Some(regex) = &self.rules.id_card {
            for mat in regex.find_iter(text) {
                if valid_id_card(mat.as_str()) {
                    push(PiiKind::IdCard, mat);
                }
            }
        }
        if let Some(regex) = &self.rules.bank_card {
            for mat in regex.find_iter(text) {
                if luhn(mat.as_str()) {
                    push(PiiKind::BankCard, mat);
                }
            }
        }
        if let Some(regex) = &self.rules.api_key {
            for mat in regex.find_iter(text) {
                push(PiiKind::ApiKey, mat);
            }
        }
        if let Some(regex) = &self.rules.ipv4 {
            for mat in regex.find_iter(text) {
                if valid_ipv4(mat.as_str()) {
                    push(PiiKind::Ip, mat);
                }
            }
        }
        for (name, regex) in &self.rules.custom {
            for mat in regex.find_iter(text) {
                detections.push(Detection {
                    kind: PiiKind::Custom(name.clone()),
                    start: mat.start(),
                    end: mat.end(),
                    confidence: 1.0,
                    detector: "custom",
                });
            }
        }
        detections
    }
}

#[derive(Debug)]
struct RulesConfig {
    email: bool,
    phone: bool,
    id_card: bool,
    bank_card: bool,
    api_key: bool,
    ip: bool,
}

impl Default for RulesConfig {
    fn default() -> Self {
        Self {
            email: true,
            phone: true,
            id_card: true,
            bank_card: true,
            api_key: true,
            ip: false,
        }
    }
}

#[derive(Debug)]
pub struct Redaction {
    manifest: PluginManifest,
    mode: Mode,
    detector: RuleDetector,
    max_placeholders: usize,
    redact_system: bool,
    redact_assistant: bool,
    redact_tool_results: bool,
    redact_tools: bool,
}

pub struct RedactionFactory;

impl RedactionFactory {
    fn parse(config: &Value) -> Result<Redaction, PluginError> {
        let mode = match config.get("mode").and_then(Value::as_str) {
            None | Some("reversible") => Mode::Reversible,
            Some("irreversible") => Mode::Irreversible,
            Some(other) => {
                return Err(PluginError::fail_closed(format!(
                    "llm-pii-redaction mode '{other}' must be reversible or irreversible"
                )));
            }
        };
        let mut flags = RulesConfig::default();
        if let Some(rules) = config.get("rules") {
            let object = rules.as_object().ok_or_else(|| {
                PluginError::fail_closed("llm-pii-redaction `rules` must be an object")
            })?;
            for (key, value) in object {
                let value = value.as_bool().ok_or_else(|| {
                    PluginError::fail_closed(format!(
                        "llm-pii-redaction rule `{key}` must be a boolean"
                    ))
                })?;
                match key.as_str() {
                    "email" => flags.email = value,
                    "phone" => flags.phone = value,
                    "id_card" => flags.id_card = value,
                    "bank_card" => flags.bank_card = value,
                    "api_key" => flags.api_key = value,
                    "ip" => flags.ip = value,
                    other => {
                        return Err(PluginError::fail_closed(format!(
                            "unknown PII rule '{other}'"
                        )));
                    }
                }
            }
        }
        let mut custom = Vec::new();
        if let Some(patterns) = config.get("custom_patterns") {
            let patterns = patterns.as_array().ok_or_else(|| {
                PluginError::fail_closed("llm-pii-redaction `custom_patterns` must be an array")
            })?;
            for pattern in patterns {
                let name = pattern
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .ok_or_else(|| {
                        PluginError::fail_closed("custom PII pattern requires a `name`")
                    })?;
                let regex = pattern
                    .get("regex")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        PluginError::fail_closed(format!(
                            "custom PII pattern '{name}' requires a `regex`"
                        ))
                    })?;
                custom.push((name.to_string(), regex.to_string()));
            }
        }
        let max_placeholders = config
            .get("max_placeholders")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_MAX_PLACEHOLDERS)
            .min(HARD_MAX_PLACEHOLDERS);
        if max_placeholders == 0 {
            return Err(PluginError::fail_closed(
                "llm-pii-redaction `max_placeholders` must be positive",
            ));
        }
        let flag =
            |key: &str, default: bool| config.get(key).and_then(Value::as_bool).unwrap_or(default);
        Ok(Redaction {
            manifest: Self::manifest(),
            mode,
            detector: RuleDetector::compile(&flags, &custom)?,
            max_placeholders,
            redact_system: flag("redact_system", true),
            redact_assistant: flag("redact_assistant", true),
            redact_tool_results: flag("redact_tool_results", true),
            redact_tools: flag("redact_tools", false),
        })
    }

    fn manifest() -> PluginManifest {
        let mut manifest = manifest(
            "llm-pii-redaction",
            "Detect and mask PII before egress, restoring it for the client",
        );
        manifest.fail_policy = FailPolicy::Closed;
        manifest.timeout_ms = 200;
        manifest.capabilities = vec![Capability::RequestBody, Capability::ResponseBody];
        manifest
    }
}

impl LlmPluginFactory for RedactionFactory {
    fn manifest(&self) -> PluginManifest {
        Self::manifest()
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        Ok(Arc::new(Self::parse(&config)?))
    }
}

impl Redaction {
    fn reversible(&self) -> bool {
        self.mode == Mode::Reversible
    }

    fn mask(&self, text: &str, state: &mut State) -> Result<String, PluginError> {
        let mut detections = self.detector.detect(text);
        resolve_overlaps(&mut detections);
        if detections.is_empty() {
            return Ok(text.to_string());
        }
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for detection in detections {
            if detection.start < cursor
                || detection.end > text.len()
                || !text.is_char_boundary(detection.start)
                || !text.is_char_boundary(detection.end)
            {
                continue;
            }
            out.push_str(&text[cursor..detection.start]);
            if state.next >= self.max_placeholders {
                return Err(PluginError::fail_closed(format!(
                    "llm-pii-redaction exceeded {} placeholders",
                    self.max_placeholders
                )));
            }
            let key = format!(
                "{PLACEHOLDER_PREFIX}{}:{}:{}{PLACEHOLDER_SUFFIX}",
                state.nonce,
                detection.kind.label(),
                state.next
            );
            state.next += 1;
            state.max_len = state.max_len.max(key.len());
            if self.reversible() {
                state.mapping.insert(
                    key.clone(),
                    text[detection.start..detection.end].to_string(),
                );
            }
            out.push_str(&key);
            cursor = detection.end;
        }
        out.push_str(&text[cursor..]);
        Ok(out)
    }

    fn mask_parts(&self, parts: &mut [ContentPart], state: &mut State) -> Result<(), PluginError> {
        for part in parts {
            match part {
                ContentPart::Text { text, .. } | ContentPart::Reasoning { text, .. } => {
                    *text = self.mask(text, state)?;
                }
                ContentPart::Image(_) | ContentPart::File(_) | ContentPart::Unsupported { .. } => {}
            }
        }
        Ok(())
    }

    fn mask_message(&self, message: &mut Message, state: &mut State) -> Result<(), PluginError> {
        if let Some(content) = &mut message.content {
            self.mask_parts(&mut content.parts, state)?;
        }
        if self.redact_assistant {
            for block in &mut message.assistant {
                match block {
                    AssistantContent::Text { text }
                    | AssistantContent::Reasoning { text, .. }
                    | AssistantContent::Refusal { text } => {
                        *text = self.mask(text, state)?;
                    }
                    AssistantContent::ToolCall { arguments, .. } => {
                        mask_value(arguments, state, self)?;
                    }
                    AssistantContent::ToolResult { content, .. } => {
                        mask_value(content, state, self)?;
                    }
                    AssistantContent::Raw { .. } => {}
                }
            }
        }
        if self.redact_tool_results {
            for result in &mut message.tool_results {
                self.mask_parts(&mut result.content.parts, state)?;
            }
        }
        Ok(())
    }

    fn mask_request(
        &self,
        request: &mut ChatRequest,
        state: &mut State,
    ) -> Result<(), PluginError> {
        if self.redact_system {
            for block in &mut request.system.blocks {
                *block = self.mask(block, state)?;
            }
        }
        for message in &mut request.messages {
            self.mask_message(message, state)?;
        }
        if self.redact_tools {
            for tool in &mut request.tools {
                let Some(function) = &mut tool.function else {
                    continue;
                };
                if let Some(description) = &mut function.description {
                    *description = self.mask(description, state)?;
                }
                if let Some(parameters) = &mut function.parameters {
                    mask_value(parameters, state, self)?;
                }
            }
        }
        Ok(())
    }

    fn restore_text(text: &str, mapping: &BTreeMap<String, String>) -> String {
        if mapping.is_empty() || !text.contains(PLACEHOLDER_PREFIX) {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find(PLACEHOLDER_PREFIX) {
            out.push_str(&rest[..start]);
            let after = &rest[start..];
            let Some(end) = after.find(PLACEHOLDER_SUFFIX) else {
                out.push_str(after);
                return out;
            };
            let end = end + PLACEHOLDER_SUFFIX.len();
            let key = &after[..end];
            match mapping.get(key) {
                Some(original) => out.push_str(original),
                None => out.push_str(key),
            }
            rest = &after[end..];
        }
        out.push_str(rest);
        out
    }

    fn restore_value(value: &mut Value, mapping: &BTreeMap<String, String>) {
        match value {
            Value::String(text) => *text = Self::restore_text(text, mapping),
            Value::Array(values) => {
                for value in values {
                    Self::restore_value(value, mapping);
                }
            }
            Value::Object(object) => {
                for value in object.values_mut() {
                    Self::restore_value(value, mapping);
                }
            }
            _ => {}
        }
    }

    fn restore_message(&self, message: &mut Message, mapping: &BTreeMap<String, String>) {
        if let Some(content) = &mut message.content {
            for part in &mut content.parts {
                match part {
                    ContentPart::Text { text, .. } | ContentPart::Reasoning { text, .. } => {
                        *text = Self::restore_text(text, mapping);
                    }
                    _ => {}
                }
            }
        }
        for block in &mut message.assistant {
            match block {
                AssistantContent::Text { text }
                | AssistantContent::Reasoning { text, .. }
                | AssistantContent::Refusal { text } => {
                    *text = Self::restore_text(text, mapping);
                }
                AssistantContent::ToolCall { arguments, .. } => {
                    Self::restore_value(arguments, mapping);
                }
                AssistantContent::ToolResult { content, .. } => {
                    Self::restore_value(content, mapping);
                }
                AssistantContent::Raw { .. } => {}
            }
        }
        for result in &mut message.tool_results {
            for part in &mut result.content.parts {
                match part {
                    ContentPart::Text { text, .. } | ContentPart::Reasoning { text, .. } => {
                        *text = Self::restore_text(text, mapping);
                    }
                    _ => {}
                }
            }
        }
    }

    fn restore_channel(channel: &str, delta: &str, state: &mut State) -> String {
        let mut pending = state.pending.remove(channel).unwrap_or_default();
        pending.push_str(delta);
        let restored = Self::restore_text(&pending, &state.mapping);
        let keep = state.max_len.saturating_sub(1).min(restored.len());
        let mut split = restored.len() - keep;
        while split < restored.len() && !restored.is_char_boundary(split) {
            split += 1;
        }
        let emit = restored[..split].to_string();
        state
            .pending
            .insert(channel.to_string(), restored[split..].to_string());
        emit
    }
}

#[async_trait]
impl LlmPlugin for Redaction {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
        let mut state = State {
            nonce: uuid::Uuid::new_v4().simple().to_string(),
            ..State::default()
        };
        self.mask_request(cx.request, &mut state)?;
        state.store(cx.state);
        Ok(LlmAction::Continue)
    }

    async fn on_provider_request(
        &self,
        _cx: &mut LlmProviderRequestContext<'_>,
    ) -> LlmPluginResult {
        Ok(LlmAction::Continue)
    }

    async fn on_response(&self, cx: &mut LlmResponseContext<'_>) -> LlmPluginResult {
        if !self.reversible() {
            return Ok(LlmAction::Continue);
        }
        let state = State::load(cx.state);
        if state.mapping.is_empty() {
            return Ok(LlmAction::Continue);
        }
        self.restore_message(&mut cx.response.message, &state.mapping);
        if let Some(reasoning) = &mut cx.response.reasoning {
            reasoning.text = Self::restore_text(&reasoning.text, &state.mapping);
        }
        Ok(LlmAction::Continue)
    }

    async fn on_stream_event(&self, cx: &mut LlmStreamContext<'_>) -> LlmStreamResult {
        if !self.reversible() {
            return Ok(LlmStreamAction::Continue);
        }
        let mut state = State::load(cx.state);
        if state.mapping.is_empty() {
            return Ok(LlmStreamAction::Continue);
        }
        let action = match cx.event {
            StreamEvent::TextDelta { text } => {
                let emit = Self::restore_channel("text", text, &mut state);
                replace_text(&emit)
            }
            StreamEvent::ReasoningDelta { text } => {
                let emit = Self::restore_channel("reasoning", text, &mut state);
                replace_reasoning(&emit)
            }
            StreamEvent::ToolCallDelta { index, arguments } => {
                let channel = format!("tool:{index}");
                let emit = Self::restore_channel(&channel, arguments, &mut state);
                if emit.is_empty() {
                    LlmStreamAction::Replace(Vec::new())
                } else {
                    LlmStreamAction::Replace(vec![StreamEvent::ToolCallDelta {
                        index: *index,
                        arguments: emit,
                    }])
                }
            }
            StreamEvent::Finish { reason } => {
                let mut events = Vec::new();
                let mut channels: Vec<String> = state.pending.keys().cloned().collect();
                channels.sort();
                for channel in channels {
                    let pending = state.pending.remove(&channel).unwrap_or_default();
                    let text = Self::restore_text(&pending, &state.mapping);
                    if text.is_empty() {
                        continue;
                    }
                    match channel.as_str() {
                        "text" => events.push(StreamEvent::TextDelta { text }),
                        "reasoning" => events.push(StreamEvent::ReasoningDelta { text }),
                        other if other.starts_with("tool:") => {
                            if let Ok(index) = other[5..].parse::<usize>() {
                                events.push(StreamEvent::ToolCallDelta {
                                    index,
                                    arguments: text,
                                });
                            }
                        }
                        _ => {}
                    }
                }
                events.push(StreamEvent::Finish { reason: *reason });
                LlmStreamAction::Replace(events)
            }
            _ => LlmStreamAction::Continue,
        };
        state.store(cx.state);
        Ok(action)
    }
}

fn replace_text(text: &str) -> LlmStreamAction {
    if text.is_empty() {
        LlmStreamAction::Replace(Vec::new())
    } else {
        LlmStreamAction::Replace(vec![StreamEvent::TextDelta {
            text: text.to_string(),
        }])
    }
}

fn replace_reasoning(text: &str) -> LlmStreamAction {
    if text.is_empty() {
        LlmStreamAction::Replace(Vec::new())
    } else {
        LlmStreamAction::Replace(vec![StreamEvent::ReasoningDelta {
            text: text.to_string(),
        }])
    }
}

fn mask_value(value: &mut Value, state: &mut State, plugin: &Redaction) -> Result<(), PluginError> {
    match value {
        Value::String(text) => *text = plugin.mask(text, state)?,
        Value::Array(values) => {
            for value in values {
                mask_value(value, state, plugin)?;
            }
        }
        Value::Object(object) => {
            for value in object.values_mut() {
                mask_value(value, state, plugin)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn resolve_overlaps(detections: &mut Vec<Detection>) {
    detections.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| b.end.cmp(&a.end)));
    let mut last_end = 0;
    detections.retain(|detection| {
        if detection.start < last_end {
            false
        } else {
            last_end = detection.end;
            true
        }
    });
}

fn luhn(value: &str) -> bool {
    let digits: Vec<u32> = value
        .chars()
        .filter_map(|character| character.to_digit(10))
        .collect();
    if digits.len() < 12 {
        return false;
    }
    let mut sum = 0;
    for (index, digit) in digits.iter().rev().enumerate() {
        let mut digit = *digit;
        if index % 2 == 1 {
            digit *= 2;
            if digit > 9 {
                digit -= 9;
            }
        }
        sum += digit;
    }
    sum % 10 == 0
}

fn valid_id_card(value: &str) -> bool {
    if value.len() != 18 {
        return false;
    }
    let bytes = value.as_bytes();
    if !bytes[..17].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let check = bytes[17].to_ascii_uppercase();
    if !check.is_ascii_digit() && check != b'X' {
        return false;
    }
    let year: u32 = match value[6..10].parse() {
        Ok(year) => year,
        Err(_) => return false,
    };
    let month: u32 = value[10..12].parse().unwrap_or(0);
    let day: u32 = value[12..14].parse().unwrap_or(0);
    if !(1900..=2100).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return false;
    }
    const WEIGHTS: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    const CHECKS: [u8; 11] = *b"10X98765432";
    let mut sum = 0;
    for (index, weight) in WEIGHTS.iter().enumerate() {
        sum += (bytes[index] - b'0') as u32 * weight;
    }
    CHECKS[(sum % 11) as usize] == check
}

fn valid_ipv4(value: &str) -> bool {
    value.split('.').all(|octet| octet.parse::<u8>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{
        ChatRequest, ChatResponse, FinishReason, Message, Protocol, Role, StreamEvent, UserContent,
    };
    use octra_llm_plugin_types::{
        LlmPlugin, LlmRequestContext, LlmRequestMeta, LlmResponseContext,
    };
    use serde_json::json;

    fn meta() -> LlmRequestMeta<'static> {
        LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            session_id: None,
            client_ip: None,
            user_agent: None,
        }
    }

    fn plugin(config: Value) -> Arc<dyn LlmPlugin> {
        RedactionFactory.create(config).unwrap()
    }

    fn text_request(text: &str) -> ChatRequest {
        ChatRequest::new(
            "m",
            vec![Message {
                role: Role::User,
                content: Some(UserContent::text(text)),
                assistant: Vec::new(),
                tool_results: Vec::new(),
                name: None,
                extra: Default::default(),
            }],
        )
    }

    async fn mask_request(
        plugin: &Arc<dyn LlmPlugin>,
        request: &mut ChatRequest,
        state: &mut Value,
    ) {
        let meta = meta();
        let mut cx = LlmRequestContext {
            meta: &meta,
            request,
            state,
        };
        plugin.on_request(&mut cx).await.unwrap();
    }

    fn request_text(request: &ChatRequest) -> String {
        request.messages[0]
            .content
            .as_ref()
            .unwrap()
            .parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn masks_and_restores_round_trip() {
        let plugin = plugin(json!({}));
        let secret = "mail a@b.com from 13800138000 card 4111111111111111";
        let mut request = text_request(secret);
        let mut state = Value::Null;
        mask_request(&plugin, &mut request, &mut state).await;

        let masked = request_text(&request);
        assert!(!masked.contains("a@b.com"), "{masked}");
        assert!(!masked.contains("13800138000"), "{masked}");
        assert!(!masked.contains("4111111111111111"), "{masked}");
        assert!(masked.contains(PLACEHOLDER_PREFIX), "{masked}");

        // The upstream echoes the masked text; the response restores it.
        let mut response = ChatResponse::new(
            "id",
            "m",
            Message {
                role: Role::Assistant,
                content: None,
                assistant: vec![AssistantContent::Text { text: masked }],
                tool_results: Vec::new(),
                name: None,
                extra: Default::default(),
            },
        );
        let meta = meta();
        let mut usage = usage();
        let mut cx = LlmResponseContext {
            meta: &meta,
            request_model: "m",
            response: &mut response,
            usage: &mut usage,
            state: &mut state,
        };
        plugin.on_response(&mut cx).await.unwrap();
        let restored = match &response.message.assistant[0] {
            AssistantContent::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert_eq!(restored, secret);
    }

    #[tokio::test]
    async fn irreversible_does_not_restore() {
        let plugin = plugin(json!({"mode": "irreversible"}));
        let mut request = text_request("a@b.com");
        let mut state = Value::Null;
        mask_request(&plugin, &mut request, &mut state).await;
        let masked = request_text(&request);
        assert!(masked.contains(PLACEHOLDER_PREFIX));

        let mut response = ChatResponse::new(
            "id",
            "m",
            Message {
                role: Role::Assistant,
                content: None,
                assistant: vec![AssistantContent::Text {
                    text: masked.clone(),
                }],
                tool_results: Vec::new(),
                name: None,
                extra: Default::default(),
            },
        );
        let meta = meta();
        let mut usage = usage();
        let mut cx = LlmResponseContext {
            meta: &meta,
            request_model: "m",
            response: &mut response,
            usage: &mut usage,
            state: &mut state,
        };
        plugin.on_response(&mut cx).await.unwrap();
        let text = match &response.message.assistant[0] {
            AssistantContent::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert_eq!(text, masked);
    }

    #[tokio::test]
    async fn stream_restore_handles_split_placeholders() {
        let plugin = plugin(json!({}));
        let mut request = text_request("contact a@b.com now");
        let mut state = Value::Null;
        mask_request(&plugin, &mut request, &mut state).await;
        let masked = request_text(&request);

        let meta = meta();
        let mut emit = String::new();
        for chunk in split_at_every(&masked, 3) {
            let mut event = StreamEvent::TextDelta { text: chunk };
            let mut cx = LlmStreamContext {
                meta: &meta,
                event: &mut event,
                state: &mut state,
            };
            match plugin.on_stream_event(&mut cx).await.unwrap() {
                LlmStreamAction::Replace(events) => {
                    for event in events {
                        if let StreamEvent::TextDelta { text } = event {
                            emit.push_str(&text);
                        }
                    }
                }
                LlmStreamAction::Continue => {
                    if let StreamEvent::TextDelta { text } = event {
                        emit.push_str(&text);
                    }
                }
                LlmStreamAction::Reject(_) => panic!("unexpected rejection"),
            }
        }
        let mut finish = StreamEvent::Finish {
            reason: FinishReason::Stop,
        };
        let mut cx = LlmStreamContext {
            meta: &meta,
            event: &mut finish,
            state: &mut state,
        };
        if let LlmStreamAction::Replace(events) = plugin.on_stream_event(&mut cx).await.unwrap() {
            for event in events {
                if let StreamEvent::TextDelta { text } = event {
                    emit.push_str(&text);
                }
            }
        }
        assert_eq!(emit, "contact a@b.com now");
    }

    #[test]
    fn validators_reject_bad_checksums() {
        assert!(luhn("4111111111111111"));
        assert!(!luhn("4111111111111112"));
        assert!(valid_id_card("11010519491231002X"));
        // Same body with a wrong check digit.
        assert!(!valid_id_card("110105194912310021"));
        assert!(valid_ipv4("192.168.1.1"));
        assert!(!valid_ipv4("999.1.1.1"));
    }

    #[test]
    fn rejects_bad_configuration() {
        assert!(RedactionFactory.create(json!({"mode": "maybe"})).is_err());
        assert!(
            RedactionFactory
                .create(json!({"custom_patterns": [{"name": "x", "regex": "("}]}))
                .is_err()
        );
        assert!(
            RedactionFactory
                .create(json!({"rules": {"email": "yes"}}))
                .is_err()
        );
        assert!(
            RedactionFactory
                .create(json!({"rules": {"unknown": true}}))
                .is_err()
        );
    }

    fn usage() -> octra_llm_core::Usage {
        octra_llm_core::Usage::default()
    }

    fn split_at_every(text: &str, size: usize) -> Vec<String> {
        let mut chunks = Vec::new();
        let mut current = String::new();
        for character in text.chars() {
            current.push(character);
            if current.len() >= size {
                chunks.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            chunks.push(current);
        }
        chunks
    }
}
