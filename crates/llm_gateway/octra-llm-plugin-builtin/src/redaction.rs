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

use aho_corasick::AhoCorasick;
use async_trait::async_trait;
use octra_api_plugin_types::{Capability, FailPolicy, PluginError, PluginManifest};
use octra_llm_core::{AssistantContent, ChatRequest, ContentPart, Message, StreamEvent};
use octra_llm_plugin_types::{
    LlmAction, LlmErrorContext, LlmLogContext, LlmPlugin, LlmPluginFactory, LlmPluginResult,
    LlmProviderRequestContext, LlmRequestContext, LlmResponseContext, LlmScope, LlmStreamAction,
    LlmStreamContext, LlmStreamResult, REQUEST_LLM_SCOPES,
};
use octra_pii_types::{
    DetectRequest, Detection, EngineReadiness, PiiDetector, PiiDetectorClient, PiiEngineResolver,
    PiiKind,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::manifest;

const PLACEHOLDER_PREFIX: &str = "[[octra:";
const PLACEHOLDER_SUFFIX: &str = "]]";
const DEFAULT_MAX_PLACEHOLDERS: usize = 1024;
const HARD_MAX_PLACEHOLDERS: usize = 10_000;
const DEFAULT_MAX_DICTIONARY_TERMS: usize = 10_000;
const HARD_MAX_DICTIONARY_TERMS: usize = 100_000;
const MAX_DICTIONARY_FILE_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_ENGINE_MAX_BYTES: usize = 1024 * 1024;
const DEFAULT_ENGINE_TIMEOUT_MS: u64 = 1_500;

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
    #[serde(default)]
    redacted: usize,
    #[serde(default)]
    redacted_kinds: BTreeMap<String, usize>,
}

impl State {
    fn load(value: &Value) -> Result<Self, PluginError> {
        // Null means the request stage did not run (for example an admission
        // rejection); a present but malformed value is corruption and must not
        // silently drop the placeholder mapping.
        if value.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(value.clone()).map_err(|error| {
            PluginError::fail_closed(format!(
                "llm-pii-redaction request state is corrupt: {error}"
            ))
        })
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
        .map_err(|error| PluginError::fail_closed(format!("invalid PII dictionary: {error}")))
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
    kinds: Vec<String>,
}

impl EngineConfig {
    fn disabled() -> Self {
        Self {
            binding: None,
            on_unavailable: OnUnavailable::Reject,
            timeout: Duration::from_millis(200),
            max_bytes: DEFAULT_ENGINE_MAX_BYTES,
            kinds: Vec::new(),
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
    engine_kinds: Vec<String>,
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
        let mut manifest = Self::manifest();
        if engine.binding.is_some() {
            // The host bounds this stage with the manifest timeout, so it must
            // allow the engine call plus masking overhead.
            manifest.timeout_ms = (engine.timeout.as_millis() as u64).saturating_add(200);
        }
        Ok(Redaction {
            manifest,
            mode,
            detector: RuleDetector::compile(&flags, &custom, dictionary)?,
            engine: engine.binding,
            engine_on_unavailable: engine.on_unavailable,
            engine_timeout: engine.timeout,
            engine_max_bytes: engine.max_bytes,
            engine_kinds: engine.kinds,
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
                "engine" | "timeout_ms" | "fail_closed" | "on_unavailable" | "max_bytes" | "kinds"
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
        let binding = match self
            .engines
            .as_ref()
            .and_then(|engines| engines.resolve(name))
        {
            Some(client) => EngineBinding::Client {
                name: name.to_string(),
                client,
            },
            None => EngineBinding::Missing {
                name: name.to_string(),
            },
        };
        let kinds = match object.get("kinds") {
            None => Vec::new(),
            Some(Value::Array(values)) => {
                let mut kinds = Vec::with_capacity(values.len());
                for value in values {
                    let kind = value
                        .as_str()
                        .map(str::trim)
                        .filter(|kind| !kind.is_empty())
                        .ok_or_else(|| {
                            PluginError::fail_closed(
                                "llm-pii-redaction `detector.kinds` entries must be non-empty strings",
                            )
                        })?;
                    kinds.push(kind.to_string());
                }
                kinds
            }
            Some(_) => {
                return Err(PluginError::fail_closed(
                    "llm-pii-redaction `detector.kinds` must be an array of strings",
                ));
            }
        };
        Ok(EngineConfig {
            binding: Some(binding),
            on_unavailable,
            timeout: Duration::from_millis(timeout_ms),
            max_bytes,
            kinds,
        })
    }

    fn manifest() -> PluginManifest {
        let mut manifest = manifest(
            "llm-pii-redaction",
            "Detect and mask PII before egress, restoring it for the client",
        );
        manifest.fail_policy = FailPolicy::Closed;
        manifest.timeout_ms = 200;
        manifest.priority = 700;
        manifest.capabilities = vec![Capability::RequestBody, Capability::ResponseBody];
        manifest.config_schema = Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "llm-pii-redaction config",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["reversible", "irreversible"],
                    "default": "reversible"
                },
                "rules": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "email": {"type": "boolean", "default": true},
                        "phone": {"type": "boolean", "default": true},
                        "id_card": {"type": "boolean", "default": true},
                        "bank_card": {"type": "boolean", "default": true},
                        "api_key": {"type": "boolean", "default": true},
                        "ip": {"type": "boolean", "default": false}
                    }
                },
                "custom_patterns": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["name", "regex"],
                        "properties": {
                            "name": {"type": "string", "minLength": 1},
                            "regex": {"type": "string"}
                        }
                    }
                },
                "dictionary": {
                    "type": "array",
                    "items": {"type": "string", "minLength": 1}
                },
                "dictionary_file": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Path to a newline-separated term file; `#` starts a comment."
                },
                "max_dictionary_terms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100000,
                    "default": 10000
                },
                "dictionary_case_insensitive": {
                    "type": "boolean",
                    "default": false
                },
                "detector": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["engine"],
                    "properties": {
                        "engine": {"type": "string", "minLength": 1},
                        "timeout_ms": {
                            "type": "integer",
                            "minimum": 1,
                            "default": 1500
                        },
                        "fail_closed": {
                            "type": "boolean",
                            "default": true,
                            "description": "Sets the default for `on_unavailable`."
                        },
                        "on_unavailable": {
                            "type": "string",
                            "enum": ["reject", "rules_only"]
                        },
                        "max_bytes": {
                            "type": "integer",
                            "minimum": 1,
                            "default": 1048576
                        },
                        "kinds": {
                            "type": "array",
                            "items": {"type": "string", "minLength": 1}
                        }
                    }
                },
                "max_placeholders": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10000,
                    "default": 1024
                },
                "redact_system": {"type": "boolean", "default": true},
                "redact_assistant": {"type": "boolean", "default": true},
                "redact_tool_results": {"type": "boolean", "default": true},
                "redact_tools": {"type": "boolean", "default": false}
            }
        }));
        manifest
    }
}

impl LlmPluginFactory for RedactionFactory {
    fn manifest(&self) -> PluginManifest {
        Self::manifest()
    }

    fn supported_scopes(&self) -> &'static [LlmScope] {
        &REQUEST_LLM_SCOPES
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        Ok(Arc::new(self.parse(&config)?))
    }
}

impl Redaction {
    fn reversible(&self) -> bool {
        self.mode == Mode::Reversible
    }

    async fn mask_request(
        &self,
        request: &mut ChatRequest,
        state: &mut State,
    ) -> Result<(), PluginError> {
        let mut slots = Vec::new();
        collect_request_text_mut(request, self, &mut slots);
        if slots.is_empty() {
            return Ok(());
        }
        let texts: Vec<&str> = slots.iter().map(|slot| slot.as_str()).collect();
        let engine_results = self.engine_detections_batch(&texts).await?;
        drop(texts);
        debug_assert_eq!(engine_results.len(), slots.len());
        for (slot, engine) in slots.into_iter().zip(engine_results) {
            let mut detections = self.detector.detect(slot.as_str());
            detections.extend(engine);
            resolve_overlaps(&mut detections);
            self.apply_detections(slot, detections, state)?;
        }
        Ok(())
    }

    fn apply_detections(
        &self,
        text: &mut String,
        detections: Vec<Detection>,
        state: &mut State,
    ) -> Result<(), PluginError> {
        if detections.is_empty() {
            return Ok(());
        }
        let original = text.as_str();
        let mut out = String::with_capacity(original.len());
        let mut cursor = 0;
        for detection in detections {
            if detection.start < cursor
                || detection.end > original.len()
                || !original.is_char_boundary(detection.start)
                || !original.is_char_boundary(detection.end)
            {
                continue;
            }
            out.push_str(&original[cursor..detection.start]);
            if state.next >= self.max_placeholders {
                return Err(PluginError::fail_closed(format!(
                    "llm-pii-redaction exceeded {} placeholders",
                    self.max_placeholders
                )));
            }
            let label = detection.kind.label().to_string();
            let key = format!(
                "{PLACEHOLDER_PREFIX}{}:{}:{}{PLACEHOLDER_SUFFIX}",
                state.nonce, label, state.next
            );
            state.next += 1;
            state.redacted += 1;
            *state.redacted_kinds.entry(label).or_default() += 1;
            state.max_len = state.max_len.max(key.len());
            if self.reversible() {
                state.mapping.insert(
                    key.clone(),
                    original[detection.start..detection.end].to_string(),
                );
            }
            out.push_str(&key);
            cursor = detection.end;
        }
        out.push_str(&original[cursor..]);
        *text = out;
        Ok(())
    }

    async fn engine_detections_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<Vec<Detection>>, PluginError> {
        let Some(binding) = &self.engine else {
            return Ok(vec![Vec::new(); texts.len()]);
        };
        let client = match binding {
            EngineBinding::Client { client, .. } => client,
            EngineBinding::Missing { name } => {
                return self.engine_unavailable_batch(
                    texts.len(),
                    format!("PII engine '{name}' is not configured"),
                );
            }
        };
        if client.readiness() != EngineReadiness::Ready {
            return self
                .engine_unavailable_batch(texts.len(), "PII engine is not ready".to_string());
        }
        if let Some(text) = texts.iter().find(|text| text.len() > self.engine_max_bytes) {
            return self.engine_unavailable_batch(
                texts.len(),
                format!(
                    "PII engine input exceeds {} bytes ({} bytes)",
                    self.engine_max_bytes,
                    text.len()
                ),
            );
        }
        let requests: Vec<DetectRequest<'_>> = texts
            .iter()
            .map(|text| DetectRequest {
                text,
                locale: None,
                kinds: &[],
                max_bytes: self.engine_max_bytes,
            })
            .collect();
        match tokio::time::timeout(self.engine_timeout, client.detect_batch(&requests)).await {
            Ok(Ok(mut results)) => {
                if !self.engine_kinds.is_empty() {
                    for detections in &mut results {
                        detections.retain(|detection| {
                            self.engine_kinds
                                .iter()
                                .any(|kind| kind == detection.kind.label())
                        });
                    }
                }
                Ok(results)
            }
            Ok(Err(error)) => {
                self.engine_unavailable_batch(texts.len(), format!("PII engine failed: {error}"))
            }
            Err(_) => self.engine_unavailable_batch(
                texts.len(),
                format!(
                    "PII engine timed out after {}ms",
                    self.engine_timeout.as_millis()
                ),
            ),
        }
    }

    fn engine_unavailable_batch(
        &self,
        count: usize,
        message: String,
    ) -> Result<Vec<Vec<Detection>>, PluginError> {
        match self.engine_on_unavailable {
            OnUnavailable::Reject => Err(PluginError::fail_closed(format!(
                "llm-pii-redaction: {message}"
            ))),
            OnUnavailable::RulesOnly => {
                tracing::warn!("{message}; continuing with rule detection only");
                Ok(vec![Vec::new(); count])
            }
        }
    }

    /// Error bodies are never restored and may be the only place an entity
    /// survived; mask what the rules find with a fixed marker.
    fn redact_error_text(
        &self,
        text: &str,
        kinds: &mut BTreeMap<String, usize>,
    ) -> (String, usize) {
        let mut detections = self.detector.detect(text);
        resolve_overlaps(&mut detections);
        if detections.is_empty() {
            return (text.to_string(), 0);
        }
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        let mut redacted = 0;
        for detection in detections {
            if detection.start < cursor
                || detection.end > text.len()
                || !text.is_char_boundary(detection.start)
                || !text.is_char_boundary(detection.end)
            {
                continue;
            }
            out.push_str(&text[cursor..detection.start]);
            let label = detection.kind.label().to_string();
            out.push_str(&format!("[REDACTED:{label}]"));
            *kinds.entry(label).or_default() += 1;
            redacted += 1;
            cursor = detection.end;
        }
        out.push_str(&text[cursor..]);
        (out, redacted)
    }

    fn redact_error_value(&self, value: &mut Value, kinds: &mut BTreeMap<String, usize>) -> usize {
        match value {
            Value::String(text) => {
                let (redacted, count) = self.redact_error_text(text, kinds);
                if count > 0 {
                    *text = redacted;
                }
                count
            }
            Value::Array(values) => values
                .iter_mut()
                .map(|value| self.redact_error_value(value, kinds))
                .sum(),
            Value::Object(object) => object
                .values_mut()
                .map(|value| self.redact_error_value(value, kinds))
                .sum(),
            _ => 0,
        }
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

    fn restores_sensitive_content(&self) -> bool {
        self.reversible()
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
        let state = State::load(cx.state)?;
        if state.mapping.is_empty() {
            return Ok(LlmAction::Continue);
        }
        self.restore_message(&mut cx.response.message, &state.mapping);
        if let Some(reasoning) = &mut cx.response.reasoning {
            reasoning.text = Self::restore_text(&reasoning.text, &state.mapping);
        }
        // Tell downstream observers (notably the semantic cache) that this
        // response now contains restored PII and must not be persisted.
        cx.response
            .extra
            .insert("octra_pii_restored".into(), Value::Bool(true));
        Ok(LlmAction::Continue)
    }

    async fn on_stream_event(&self, cx: &mut LlmStreamContext<'_>) -> LlmStreamResult {
        if !self.reversible() {
            return Ok(LlmStreamAction::Continue);
        }
        let mut state = State::load(cx.state)?;
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

    async fn on_error(&self, cx: &mut LlmErrorContext<'_>) -> LlmPluginResult {
        let mut kinds = BTreeMap::new();
        let mut redacted = 0;

        let (message, count) = self.redact_error_text(&cx.error.message, &mut kinds);
        if count > 0 {
            cx.error.message = message;
            redacted += count;
        }
        if let Some(body) = &mut cx.error.upstream_body {
            redacted += self.redact_error_value(body, &mut kinds);
        }
        for value in cx.error.extra.values_mut() {
            redacted += self.redact_error_value(value, &mut kinds);
        }

        if redacted > 0 {
            tracing::warn!(
                target: "octra.audit",
                request_id = cx.meta.request_id,
                redacted,
                kinds = ?kinds,
                "PII found in an upstream error body"
            );
            let mut state = State::load(cx.state)?;
            state.redacted += redacted;
            for (kind, count) in kinds {
                *state.redacted_kinds.entry(kind).or_default() += count;
            }
            state.store(cx.state);
        }
        Ok(LlmAction::Continue)
    }

    async fn on_log(&self, cx: &mut LlmLogContext<'_>) {
        let state = match State::load(cx.state) {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(
                    request_id = cx.meta.request_id,
                    error = %error,
                    "llm-pii-redaction cannot read its request state"
                );
                return;
            }
        };
        if state.redacted == 0 {
            return;
        }
        tracing::info!(
            target: "octra.audit",
            request_id = cx.meta.request_id,
            redacted = state.redacted,
            kinds = ?state.redacted_kinds,
            "PII redaction applied"
        );
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

fn collect_request_text_mut<'a>(
    request: &'a mut ChatRequest,
    plugin: &Redaction,
    out: &mut Vec<&'a mut String>,
) {
    if plugin.redact_system {
        for block in &mut request.system.blocks {
            out.push(block);
        }
    }
    for message in &mut request.messages {
        if let Some(content) = &mut message.content {
            collect_parts_text_mut(&mut content.parts, out);
        }
        if plugin.redact_assistant {
            for block in &mut message.assistant {
                match block {
                    AssistantContent::Text { text }
                    | AssistantContent::Reasoning { text, .. }
                    | AssistantContent::Refusal { text } => out.push(text),
                    AssistantContent::ToolCall { arguments, .. } => {
                        collect_value_text_mut(arguments, out);
                    }
                    AssistantContent::ToolResult { content, .. } => {
                        collect_value_text_mut(content, out);
                    }
                    AssistantContent::Raw { .. } => {}
                }
            }
        }
        if plugin.redact_tool_results {
            for result in &mut message.tool_results {
                collect_parts_text_mut(&mut result.content.parts, out);
            }
        }
    }
    if plugin.redact_tools {
        for tool in &mut request.tools {
            let Some(function) = &mut tool.function else {
                continue;
            };
            if let Some(description) = &mut function.description {
                out.push(description);
            }
            if let Some(parameters) = &mut function.parameters {
                collect_value_text_mut(parameters, out);
            }
        }
    }
}

fn collect_parts_text_mut<'a>(parts: &'a mut [ContentPart], out: &mut Vec<&'a mut String>) {
    for part in parts {
        match part {
            ContentPart::Text { text, .. } | ContentPart::Reasoning { text, .. } => out.push(text),
            ContentPart::Image(_) | ContentPart::File(_) | ContentPart::Unsupported { .. } => {}
        }
    }
}

fn collect_value_text_mut<'a>(value: &'a mut Value, out: &mut Vec<&'a mut String>) {
    match value {
        Value::String(text) => out.push(text),
        Value::Array(values) => {
            for value in values {
                collect_value_text_mut(value, out);
            }
        }
        Value::Object(object) => {
            for value in object.values_mut() {
                collect_value_text_mut(value, out);
            }
        }
        _ => {}
    }
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
        ChatRequest, ChatResponse, ErrorKind, FinishReason, GatewayError, Message, Protocol, Role,
        StreamEvent, UserContent,
    };
    use octra_llm_plugin_types::LlmResponseHeaders;
    use octra_llm_plugin_types::{
        LlmErrorContext, LlmPlugin, LlmRequestContext, LlmRequestMeta, LlmResponseContext,
    };
    use serde_json::json;

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
            headers: &mut LlmResponseHeaders::default(),
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
        let plugin = plugin_with(Some(resolver), json!({"detector": {"engine": "fake"}}));
        let mut request = text_request(text);
        let mut state = Value::Null;
        mask_request(&plugin, &mut request, &mut state).await;
        let masked = request_text(&request);
        assert!(!masked.contains("Alice"), "{masked}");
        assert!(masked.contains(PLACEHOLDER_PREFIX), "{masked}");
    }

    #[tokio::test]
    async fn engine_kind_filter_skips_unselected_entities() {
        let text = "Alice wrote to a@b.com";
        let engine = Arc::new(FakeEngine {
            readiness: EngineReadiness::Ready,
            detections: vec![
                engine_span(PiiKind::Custom("private_person".into()), text, "Alice"),
                engine_span(PiiKind::Email, text, "a@b.com"),
            ],
        });
        let resolver = Arc::new(FakeResolver {
            name: "fake",
            engine,
        });
        let plugin = plugin_with(
            Some(resolver),
            json!({"detector": {"engine": "fake", "kinds": ["email"]}}),
        );
        let mut request = text_request(text);
        let mut state = Value::Null;
        mask_request(&plugin, &mut request, &mut state).await;
        let masked = request_text(&request);
        assert!(masked.contains("Alice"), "{masked}");
        assert!(!masked.contains("a@b.com"), "{masked}");
    }

    #[test]
    fn corrupt_state_fails_closed() {
        assert!(State::load(&Value::Null).is_ok());
        assert!(State::load(&json!({"nonce": 1})).is_err());
        assert!(State::load(&json!({"mapping": "not-a-map"})).is_err());
    }

    #[tokio::test]
    async fn state_records_content_free_counts() {
        let plugin = plugin(json!({"rules": {"phone": true}}));
        let mut request = text_request("a@b.com and 13800138000");
        let mut state = Value::Null;
        mask_request(&plugin, &mut request, &mut state).await;
        let counts = State::load(&state).unwrap();
        assert_eq!(counts.redacted, 2);
        assert_eq!(counts.redacted_kinds["email"], 1);
        assert_eq!(counts.redacted_kinds["phone"], 1);
    }

    #[test]
    fn rejects_bad_detector_kinds() {
        let resolver_less = RedactionFactory::new(None);
        assert!(
            resolver_less
                .create(json!({"detector": {"engine": "fake", "kinds": "email"}}))
                .is_err()
        );
        assert!(
            resolver_less
                .create(json!({"detector": {"engine": "fake", "kinds": [""]}}))
                .is_err()
        );
    }

    struct AnyResolver {
        name: &'static str,
        engine: Arc<dyn PiiDetectorClient>,
    }

    impl PiiEngineResolver for AnyResolver {
        fn resolve(&self, name: &str) -> Option<Arc<dyn PiiDetectorClient>> {
            (name == self.name).then(|| self.engine.clone())
        }
    }

    struct BatchEngine {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl PiiDetectorClient for BatchEngine {
        fn readiness(&self) -> EngineReadiness {
            EngineReadiness::Ready
        }

        async fn detect(
            &self,
            _request: DetectRequest<'_>,
        ) -> Result<Vec<Detection>, octra_pii_types::PiiEngineError> {
            Err(octra_pii_types::PiiEngineError::new(
                octra_pii_types::PiiEngineErrorKind::Unavailable,
                "per-field detection must not be used",
            ))
        }

        async fn detect_batch(
            &self,
            requests: &[DetectRequest<'_>],
        ) -> Result<Vec<Vec<Detection>>, octra_pii_types::PiiEngineError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(requests
                .iter()
                .map(|request| {
                    request
                        .text
                        .match_indices("Alice")
                        .map(|(start, needle)| Detection {
                            kind: PiiKind::Custom("private_person".into()),
                            start,
                            end: start + needle.len(),
                            confidence: 0.99,
                            detector: "engine",
                        })
                        .collect()
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn one_engine_call_masks_every_text_field() {
        let engine = Arc::new(BatchEngine {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let resolver = Arc::new(AnyResolver {
            name: "batch",
            engine: engine.clone(),
        });
        let plugin = plugin_with(Some(resolver), json!({"detector": {"engine": "batch"}}));
        let mut request = text_request("Alice one");
        request.messages.push(Message {
            role: Role::User,
            content: Some(UserContent::text("Alice two")),
            assistant: Vec::new(),
            tool_results: Vec::new(),
            name: None,
            extra: Default::default(),
        });
        let mut state = Value::Null;
        mask_request(&plugin, &mut request, &mut state).await;

        assert_eq!(engine.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!request_text(&request).contains("Alice"));
        let second = request.messages[1]
            .content
            .as_ref()
            .unwrap()
            .parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(!second.contains("Alice"), "{second}");
    }

    #[tokio::test]
    async fn missing_engine_fails_closed_by_default() {
        let plugin = plugin_with(None, json!({"detector": {"engine": "missing"}}));
        let mut request = text_request("hello");
        let mut state = Value::Null;
        let meta = meta();
        let mut cx = LlmRequestContext {
            meta: &meta,
            headers: &mut LlmResponseHeaders::default(),
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
        let plugin = plugin_with(Some(resolver), json!({"detector": {"engine": "fake"}}));
        let mut request = text_request("hello");
        let mut state = Value::Null;
        let meta = meta();
        let mut cx = LlmRequestContext {
            meta: &meta,
            headers: &mut LlmResponseHeaders::default(),
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
        let path =
            std::env::temp_dir().join(format!("octra-pii-dictionary-{}.txt", uuid::Uuid::new_v4()));
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
        assert!(
            !masked.to_ascii_lowercase().contains("acme corp"),
            "{masked}"
        );
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

    #[tokio::test]
    async fn error_bodies_are_rescanned_and_masked() {
        let plugin = plugin(json!({}));
        let mut error = GatewayError::new(
            502,
            ErrorKind::Upstream,
            "upstream echoed a@b.com in its error",
        );
        error.upstream_body = Some(json!({
            "detail": "phone 13800138000",
            "nested": {"mail": "c@d.com"},
        }));
        error
            .extra
            .insert("hint".to_string(), json!("mail e@f.com"));

        let meta = meta();
        let mut state = Value::Null;
        {
            let mut cx = LlmErrorContext {
                meta: &meta,
                provider_id: None,
                account_id: None,
                error: &mut error,
                state: &mut state,
            };
            plugin.on_error(&mut cx).await.unwrap();
        }

        assert!(!error.message.contains("a@b.com"), "{}", error.message);
        assert!(error.message.contains("[REDACTED:email]"));
        let body = error
            .upstream_body
            .as_ref()
            .map(Value::to_string)
            .unwrap_or_default();
        assert!(!body.contains("13800138000"), "{body}");
        assert!(!body.contains("c@d.com"), "{body}");
        assert!(body.contains("[REDACTED:"));
        assert!(
            !error.extra["hint"]
                .as_str()
                .unwrap_or_default()
                .contains("e@f.com"),
            "{:?}",
            error.extra
        );
        assert!(State::load(&state).unwrap().redacted >= 4);
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
    fn manifest_timeout_covers_the_engine_deadline() {
        let plugin = RedactionFactory::new(None)
            .create(json!({"detector": {"engine": "x", "timeout_ms": 900}}))
            .unwrap();
        assert_eq!(plugin.manifest().timeout_ms, 1_100);
        let plain = RedactionFactory::new(None).create(json!({})).unwrap();
        assert_eq!(plain.manifest().timeout_ms, 200);
    }

    #[test]
    fn rejects_bad_configuration() {
        assert!(
            RedactionFactory::new(None)
                .create(json!({"mode": "maybe"}))
                .is_err()
        );
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
