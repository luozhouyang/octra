//! PII redaction for requests, responses and streams.
//!
//! In `reversible` mode the upstream sees stable placeholders and the client
//! sees the original values. The mapping lives only in the plugin's request
//! state and is dropped when the request finishes. In `irreversible` mode the
//! placeholders are never restored.
//!
//! Errors and logs are never restored: upstream error bodies often echo the
//! request, and restoring them would leak PII back through an unexpected path.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use octra_api_plugin_types::{Capability, FailPolicy, PluginError, PluginManifest};
use octra_llm_core::{AssistantContent, ChatRequest, ContentPart, Message, StreamEvent};
use octra_llm_plugin_types::{
    LlmAction, LlmPlugin, LlmPluginFactory, LlmPluginResult, LlmProviderRequestContext,
    LlmRequestContext, LlmResponseContext, LlmStreamAction, LlmStreamContext, LlmStreamResult,
};
use aho_corasick::AhoCorasick;
use futures_util::future::BoxFuture;
use octra_pii_types::{
    DetectRequest, Detection, EngineReadiness, PiiDetector, PiiDetectorClient, PiiEngineResolver,
    PiiKind,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest;

const PLACEHOLDER_PREFIX: &str = "[[octra:";
const PLACEHOLDER_SUFFIX: &str = "]]";
const DEFAULT_MAX_PLACEHOLDERS: usize = 1024;
const HARD_MAX_PLACEHOLDERS: usize = 10_000;
const DEFAULT_MAX_DICTIONARY_TERMS: usize = 10_000;
const HARD_MAX_DICTIONARY_TERMS: usize = 100_000;
const MAX_DICTIONARY_FILE_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_ENGINE_MAX_BYTES: usize = 1024 * 1024;
const DEFAULT_ENGINE_TIMEOUT_MS: u64 = 200;

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
    dictionary: Option<AhoCorasick>,
}

#[derive(Debug)]
pub struct RuleDetector {
    rules: Rules,
}

impl RuleDetector {
    fn compile(
        flags: &RulesConfig,
        custom: &[(String, String)],
        dictionary: Option<AhoCorasick>,
    ) -> Result<Self, PluginError> {
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
                dictionary,
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
        if let Some(dictionary) = &self.rules.dictionary {
            for mat in dictionary.find_iter(text) {
                detections.push(Detection {
                    kind: PiiKind::Custom("dictionary".to_string()),
                    start: mat.start(),
                    end: mat.end(),
                    confidence: 1.0,
                    detector: "dictionary",
                });
            }
        }
        detections
    }
}

fn compile_dictionary(config: &Value) -> Result<Option<AhoCorasick>, PluginError> {
    let mut terms: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    if let Some(values) = config.get("dictionary") {
        let values = values.as_array().ok_or_else(|| {
            PluginError::fail_closed("llm-pii-redaction `dictionary` must be an array")
        })?;
        for value in values {
            let term = value.as_str().ok_or_else(|| {
                PluginError::fail_closed("llm-pii-redaction dictionary terms must be strings")
            })?;
            push_dictionary_term(&mut terms, &mut seen, term)?;
        }
    }
    if let Some(path) = config.get("dictionary_file").and_then(Value::as_str) {
        let path = path.trim();
        if path.is_empty() {
            return Err(PluginError::fail_closed(
                "llm-pii-redaction `dictionary_file` must not be empty",
            ));
        }
        let metadata = std::fs::metadata(path).map_err(|error| {
            PluginError::fail_closed(format!("cannot read dictionary file {path}: {error}"))
        })?;
        if metadata.len() > MAX_DICTIONARY_FILE_BYTES {
            return Err(PluginError::fail_closed(format!(
                "dictionary file {path} exceeds {MAX_DICTIONARY_FILE_BYTES} bytes"
            )));
        }
        let content = std::fs::read_to_string(path).map_err(|error| {
            PluginError::fail_closed(format!("cannot read dictionary file {path}: {error}"))
        })?;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            push_dictionary_term(&mut terms, &mut seen, line)?;
        }
    }
    if terms.is_empty() {
        return Ok(None);
    }
    let max_terms = config
        .get("max_dictionary_terms")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_MAX_DICTIONARY_TERMS)
        .min(HARD_MAX_DICTIONARY_TERMS);
    if max_terms == 0 {
        return Err(PluginError::fail_closed(
            "llm-pii-redaction `max_dictionary_terms` must be positive",
        ));
    }
    if terms.len() > max_terms {
        return Err(PluginError::fail_closed(format!(
            "llm-pii-redaction dictionary has {} terms, over the limit of {max_terms}",
            terms.len()
        )));
    }
    let case_insensitive = config
        .get("dictionary_case_insensitive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    AhoCorasick::builder()
        .ascii_case_insensitive(case_insensitive)
        .build(&terms)
        .map(Some)
        .map_err(|error| {
            PluginError::fail_closed(format!("invalid PII dictionary: {error}"))
        })
}

fn push_dictionary_term(
    terms: &mut Vec<String>,
    seen: &mut HashSet<String>,
    term: &str,
) -> Result<(), PluginError> {
    let term = term.trim();
    if term.is_empty() {
        return Err(PluginError::fail_closed(
            "llm-pii-redaction dictionary terms must not be empty",
        ));
    }
    if seen.insert(term.to_string()) {
        terms.push(term.to_string());
    }
    Ok(())
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

enum EngineBinding {
    Client {
        name: String,
        client: Arc<dyn PiiDetectorClient>,
    },
    Missing {
        name: String,
    },
}

impl std::fmt::Debug for EngineBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client { name, .. } => formatter
                .debug_struct("Client")
                .field("name", name)
                .finish(),
            Self::Missing { name } => formatter
                .debug_struct("Missing")
                .field("name", name)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnUnavailable {
    Reject,
    RulesOnly,
}

#[derive(Debug)]
struct EngineConfig {
    binding: Option<EngineBinding>,
    on_unavailable: OnUnavailable,
    timeout: Duration,
    max_bytes: usize,
}

impl EngineConfig {
    fn disabled() -> Self {
        Self {
            binding: None,
            on_unavailable: OnUnavailable::Reject,
            timeout: Duration::from_millis(200),
            max_bytes: DEFAULT_ENGINE_MAX_BYTES,
        }
    }
}

#[derive(Debug)]
pub struct Redaction {
    manifest: PluginManifest,
    mode: Mode,
    detector: RuleDetector,
    engine: Option<EngineBinding>,
    engine_on_unavailable: OnUnavailable,
    engine_timeout: Duration,
    engine_max_bytes: usize,
    max_placeholders: usize,
    redact_system: bool,
    redact_assistant: bool,
    redact_tool_results: bool,
    redact_tools: bool,
}

pub struct RedactionFactory {
    engines: Option<Arc<dyn PiiEngineResolver>>,
}

impl RedactionFactory {
    pub fn new(engines: Option<Arc<dyn PiiEngineResolver>>) -> Self {
        Self { engines }
    }

    fn parse(&self, config: &Value) -> Result<Redaction, PluginError> {
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
        let engine = self.parse_engine(config)?;
        let dictionary = compile_dictionary(config)?;
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
            detector: RuleDetector::compile(&flags, &custom, dictionary)?,
            engine: engine.binding,
            engine_on_unavailable: engine.on_unavailable,
            engine_timeout: engine.timeout,
            engine_max_bytes: engine.max_bytes,
            max_placeholders,
            redact_system: flag("redact_system", true),
            redact_assistant: flag("redact_assistant", true),
            redact_tool_results: flag("redact_tool_results", true),
            redact_tools: flag("redact_tools", false),
        })
    }

    fn parse_engine(&self, config: &Value) -> Result<EngineConfig, PluginError> {
        let Some(detector) = config.get("detector") else {
            return Ok(EngineConfig::disabled());
        };
        let object = detector.as_object().ok_or_else(|| {
            PluginError::fail_closed("llm-pii-redaction `detector` must be an object")
        })?;
        for key in object.keys() {
            if !matches!(
                key.as_str(),
                "engine" | "timeout_ms" | "fail_closed" | "on_unavailable" | "max_bytes"
            ) {
                return Err(PluginError::fail_closed(format!(
                    "unknown PII detector option '{key}'"
                )));
            }
        }
        let name = object
            .get("engine")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                PluginError::fail_closed("llm-pii-redaction `detector.engine` is required")
            })?;
        let fail_closed = object
            .get("fail_closed")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let on_unavailable = match object.get("on_unavailable").and_then(Value::as_str) {
            None => {
                if fail_closed {
                    OnUnavailable::Reject
                } else {
                    OnUnavailable::RulesOnly
                }
            }
            Some("reject") => OnUnavailable::Reject,
            Some("rules_only") => OnUnavailable::RulesOnly,
            Some(other) => {
                return Err(PluginError::fail_closed(format!(
                    "llm-pii-redaction `detector.on_unavailable` must be reject or rules_only, got '{other}'"
                )));
            }
        };
        let timeout_ms = object
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_ENGINE_TIMEOUT_MS)
            .max(1);
        let max_bytes = object
            .get("max_bytes")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_ENGINE_MAX_BYTES);
        if max_bytes == 0 {
            return Err(PluginError::fail_closed(
                "llm-pii-redaction `detector.max_bytes` must be positive",
            ));
        }
        let binding = match self.engines.as_ref().and_then(|engines| engines.resolve(name)) {
            Some(client) => EngineBinding::Client {
                name: name.to_string(),
                client,
            },
            None => EngineBinding::Missing {
                name: name.to_string(),
            },
        };
        Ok(EngineConfig {
            binding: Some(binding),
            on_unavailable,
            timeout: Duration::from_millis(timeout_ms),
            max_bytes,
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
        Ok(Arc::new(self.parse(&config)?))
    }
}

impl Redaction {
    fn reversible(&self) -> bool {
        self.mode == Mode::Reversible
    }

    async fn mask(&self, text: &str, state: &mut State) -> Result<String, PluginError> {
        let mut detections = self.detector.detect(text);
        if let Some(binding) = &self.engine {
            detections.extend(self.engine_detections(binding, text).await?);
        }
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

    async fn engine_detections(
        &self,
        binding: &EngineBinding,
        text: &str,
    ) -> Result<Vec<Detection>, PluginError> {
        let client = match binding {
            EngineBinding::Client { client, .. } => client,
            EngineBinding::Missing { name } => {
                return self.engine_unavailable(format!("PII engine '{name}' is not configured"));
            }
        };
        if client.readiness() != EngineReadiness::Ready {
            return self.engine_unavailable("PII engine is not ready".to_string());
        }
        if text.len() > self.engine_max_bytes {
            return self.engine_unavailable(format!(
                "PII engine input exceeds {} bytes",
                self.engine_max_bytes
            ));
        }
        let request = DetectRequest {
            text,
            locale: None,
            kinds: &[],
            max_bytes: self.engine_max_bytes,
        };
        match tokio::time::timeout(self.engine_timeout, client.detect(request)).await {
            Ok(Ok(detections)) => Ok(detections),
            Ok(Err(error)) => self.engine_unavailable(format!("PII engine failed: {error}")),
            Err(_) => self.engine_unavailable(format!(
                "PII engine timed out after {}ms",
                self.engine_timeout.as_millis()
            )),
        }
    }

    fn engine_unavailable(&self, message: String) -> Result<Vec<Detection>, PluginError> {
        match self.engine_on_unavailable {
            OnUnavailable::Reject => Err(PluginError::fail_closed(format!(
                "llm-pii-redaction: {message}"
            ))),
            OnUnavailable::RulesOnly => {
                tracing::warn!("{message}; continuing with rule detection only");
                Ok(Vec::new())
            }
        }
    }

    async fn mask_parts(
        &self,
        parts: &mut [ContentPart],
        state: &mut State,
    ) -> Result<(), PluginError> {
        for part in parts {
            match part {
                ContentPart::Text { text, .. } | ContentPart::Reasoning { text, .. } => {
                    *text = self.mask(text, state).await?;
                }
                ContentPart::Image(_) | ContentPart::File(_) | ContentPart::Unsupported { .. } => {}
            }
        }
        Ok(())
    }

    async fn mask_message(
        &self,
        message: &mut Message,
        state: &mut State,
    ) -> Result<(), PluginError> {
        if let Some(content) = &mut message.content {
            self.mask_parts(&mut content.parts, state).await?;
        }
        if self.redact_assistant {
            for block in &mut message.assistant {
                match block {
                    AssistantContent::Text { text }
                    | AssistantContent::Reasoning { text, .. }
                    | AssistantContent::Refusal { text } => {
                        *text = self.mask(text, state).await?;
                    }
                    AssistantContent::ToolCall { arguments, .. } => {
                        mask_value(arguments, state, self).await?;
                    }
                    AssistantContent::ToolResult { content, .. } => {
                        mask_value(content, state, self).await?;
                    }
                    AssistantContent::Raw { .. } => {}
                }
            }
        }
        if self.redact_tool_results {
            for result in &mut message.tool_results {
                self.mask_parts(&mut result.content.parts, state).await?;
            }
        }
        Ok(())
    }

    async fn mask_request(
        &self,
        request: &mut ChatRequest,
        state: &mut State,
    ) -> Result<(), PluginError> {
        if self.redact_system {
            for block in &mut request.system.blocks {
                *block = self.mask(block, state).await?;
            }
        }
        for message in &mut request.messages {
            self.mask_message(message, state).await?;
        }
        if self.redact_tools {
            for tool in &mut request.tools {
                let Some(function) = &mut tool.function else {
                    continue;
                };
                if let Some(description) = &mut function.description {
                    *description = self.mask(description, state).await?;
                }
                if let Some(parameters) = &mut function.parameters {
                    mask_value(parameters, state, self).await?;
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
        self.mask_request(cx.request, &mut state).await?;
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

fn mask_value<'a>(
    value: &'a mut Value,
    state: &'a mut State,
    plugin: &'a Redaction,
) -> BoxFuture<'a, Result<(), PluginError>> {
    Box::pin(async move {
        match value {
            Value::String(text) => *text = plugin.mask(text, state).await?,
            Value::Array(values) => {
                for value in values {
                    mask_value(value, state, plugin).await?;
                }
            }
            Value::Object(object) => {
                for value in object.values_mut() {
                    mask_value(value, state, plugin).await?;
                }
            }
            _ => {}
        }
        Ok(())
    })
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
            intent: None,
        }
    }

    fn plugin(config: Value) -> Arc<dyn LlmPlugin> {
        RedactionFactory::new(None).create(config).unwrap()
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

    struct FakeEngine {
        readiness: EngineReadiness,
        detections: Vec<Detection>,
    }

    #[async_trait]
    impl PiiDetectorClient for FakeEngine {
        fn readiness(&self) -> EngineReadiness {
            self.readiness
        }

        async fn detect(
            &self,
            _request: DetectRequest<'_>,
        ) -> Result<Vec<Detection>, octra_pii_types::PiiEngineError> {
            Ok(self.detections.clone())
        }
    }

    struct FakeResolver {
        name: &'static str,
        engine: Arc<FakeEngine>,
    }

    impl PiiEngineResolver for FakeResolver {
        fn resolve(&self, name: &str) -> Option<Arc<dyn PiiDetectorClient>> {
            (name == self.name).then(|| self.engine.clone() as Arc<dyn PiiDetectorClient>)
        }
    }

    fn plugin_with(
        engines: Option<Arc<dyn PiiEngineResolver>>,
        config: Value,
    ) -> Arc<dyn LlmPlugin> {
        RedactionFactory::new(engines).create(config).unwrap()
    }

    fn engine_span(kind: PiiKind, text: &str, needle: &str) -> Detection {
        let start = text.find(needle).unwrap();
        Detection {
            kind,
            start,
            end: start + needle.len(),
            confidence: 0.99,
            detector: "engine",
        }
    }

    #[tokio::test]
    async fn engine_detections_are_masked() {
        let text = "Alice says hi";
        let engine = Arc::new(FakeEngine {
            readiness: EngineReadiness::Ready,
            detections: vec![engine_span(PiiKind::Custom("person".into()), text, "Alice")],
        });
        let resolver = Arc::new(FakeResolver {
            name: "fake",
            engine: engine.clone(),
        });
        let plugin = plugin_with(
            Some(resolver),
            json!({"detector": {"engine": "fake"}}),
        );
        let mut request = text_request(text);
        let mut state = Value::Null;
        mask_request(&plugin, &mut request, &mut state).await;
        let masked = request_text(&request);
        assert!(!masked.contains("Alice"), "{masked}");
        assert!(masked.contains(PLACEHOLDER_PREFIX), "{masked}");
    }

    #[tokio::test]
    async fn missing_engine_fails_closed_by_default() {
        let plugin = plugin_with(None, json!({"detector": {"engine": "missing"}}));
        let mut request = text_request("hello");
        let mut state = Value::Null;
        let meta = meta();
        let mut cx = LlmRequestContext {
            meta: &meta,
            request: &mut request,
            state: &mut state,
        };
        assert!(plugin.on_request(&mut cx).await.is_err());
    }

    #[tokio::test]
    async fn rules_only_falls_back_when_engine_is_missing() {
        let plugin = plugin_with(
            None,
            json!({"detector": {"engine": "missing", "on_unavailable": "rules_only"}}),
        );
        let mut request = text_request("mail a@b.com");
        let mut state = Value::Null;
        mask_request(&plugin, &mut request, &mut state).await;
        let masked = request_text(&request);
        assert!(!masked.contains("a@b.com"), "{masked}");
        assert!(masked.contains(PLACEHOLDER_PREFIX), "{masked}");
    }

    #[tokio::test]
    async fn unready_engine_fails_closed() {
        let engine = Arc::new(FakeEngine {
            readiness: EngineReadiness::Starting,
            detections: Vec::new(),
        });
        let resolver = Arc::new(FakeResolver {
            name: "fake",
            engine,
        });
        let plugin = plugin_with(
            Some(resolver),
            json!({"detector": {"engine": "fake"}}),
        );
        let mut request = text_request("hello");
        let mut state = Value::Null;
        let meta = meta();
        let mut cx = LlmRequestContext {
            meta: &meta,
            request: &mut request,
            state: &mut state,
        };
        assert!(plugin.on_request(&mut cx).await.is_err());
    }

    #[tokio::test]
    async fn dictionary_terms_are_masked_and_restored() {
        let plugin = plugin(json!({"dictionary": ["张三", "Project Falcon"]}));
        let secret = "张三 owns Project Falcon";
        let mut request = text_request(secret);
        let mut state = Value::Null;
        mask_request(&plugin, &mut request, &mut state).await;

        let masked = request_text(&request);
        assert!(!masked.contains("张三"), "{masked}");
        assert!(!masked.contains("Project Falcon"), "{masked}");
        assert!(masked.contains(PLACEHOLDER_PREFIX), "{masked}");

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
    async fn dictionary_file_ignores_comments_and_matches_case_insensitively() {
        let path = std::env::temp_dir().join(format!(
            "octra-pii-dictionary-{}.txt",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, "# comment\n\n Acme Corp \n").unwrap();
        let plugin = plugin(json!({
            "dictionary_file": path.to_str().unwrap(),
            "dictionary_case_insensitive": true
        }));
        let mut request = text_request("visit acme corp today");
        let mut state = Value::Null;
        mask_request(&plugin, &mut request, &mut state).await;
        let masked = request_text(&request);
        let _ = std::fs::remove_file(&path);
        assert!(!masked.to_ascii_lowercase().contains("acme corp"), "{masked}");
        assert!(masked.contains(PLACEHOLDER_PREFIX), "{masked}");
    }

    #[test]
    fn rejects_bad_dictionary_configuration() {
        assert!(
            RedactionFactory::new(None)
                .create(json!({"dictionary": [""]}))
                .is_err()
        );
        assert!(
            RedactionFactory::new(None)
                .create(json!({"dictionary": ["acme"], "max_dictionary_terms": 0}))
                .is_err()
        );
        assert!(
            RedactionFactory::new(None)
                .create(json!({"dictionary": ["acme"], "max_dictionary_terms": 1}))
                .is_ok()
        );
        assert!(
            RedactionFactory::new(None)
                .create(json!({"dictionary": ["a", "b"], "max_dictionary_terms": 1}))
                .is_err()
        );
        assert!(
            RedactionFactory::new(None)
                .create(json!({"dictionary_file": "/nonexistent/octra-dictionary.txt"}))
                .is_err()
        );
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
        assert!(RedactionFactory::new(None).create(json!({"mode": "maybe"})).is_err());
        assert!(
            RedactionFactory::new(None)
                .create(json!({"custom_patterns": [{"name": "x", "regex": "("}]}))
                .is_err()
        );
        assert!(
            RedactionFactory::new(None)
                .create(json!({"rules": {"email": "yes"}}))
                .is_err()
        );
        assert!(
            RedactionFactory::new(None)
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
