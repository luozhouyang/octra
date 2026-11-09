//! Bounded semantic-ish response cache for repeated prompts.
//!
//! The cache short-circuits the request stage with a stored, complete
//! non-streaming response. Lookups are scoped by default to the effective
//! model, the consumer and the virtual key, so one caller never receives a
//! response cached for another unless the operator disables that isolation.
//!
//! Matching uses an exact canonical request hash first. When the configured
//! similarity threshold is below `1.0`, the plugin also compares a local
//! lexical vector (lowercase words and character trigrams feature-hashed into
//! 256 dimensions). An optional OpenAI-compatible embeddings endpoint can
//! replace the local vector; any failure falls back to the local vector.
//!
//! Streaming requests use the same lookup and pending path. The gateway
//! reconstructs a canonical response from the stream at close and stores it
//! through the final-response hook, so cached streams are replayed as SSE. The
//! cache never persists prompt text.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use octra_api_plugin_types::{Capability, FailPolicy, PluginError, PluginManifest};
use octra_llm_core::{AssistantContent, ChatRequest, ChatResponse, ContentPart, FinishReason};
use octra_llm_plugin_runtime::MemoryPluginStateStore;
use octra_llm_plugin_types::{
    LlmAction, LlmFinalResponseContext, LlmPlugin, LlmPluginCreateContext, LlmPluginFactory,
    LlmPluginResult, LlmPluginStateStore, LlmRequestContext, LlmRequestMeta, LlmResponseSource,
    LlmScope, REQUEST_LLM_SCOPES,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::manifest;

const PLUGIN_NAME: &str = "llm-semantic-cache";
const CACHE_HEADER: &str = "X-Octra-Cache";
const STATE_PREFIX: &str = "llm-semantic-cache";

const DEFAULT_MAX_ENTRIES: usize = 256;
const MIN_MAX_ENTRIES: usize = 1;
const MAX_MAX_ENTRIES: usize = 4096;
const DEFAULT_TTL_SECS: i64 = 3_600;
const MIN_TTL_SECS: i64 = 1;
const MAX_TTL_SECS: i64 = 604_800;
const DEFAULT_SIMILARITY_THRESHOLD: f64 = 1.0;
const DEFAULT_MAX_PROMPT_CHARS: usize = 8_000;
const MIN_MAX_PROMPT_CHARS: usize = 1;
const MAX_MAX_PROMPT_CHARS: usize = 64_000;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 1_048_576;
const MIN_MAX_RESPONSE_BYTES: usize = 1_024;
const MAX_MAX_RESPONSE_BYTES: usize = 16_777_216;
const DEFAULT_EMBEDDING_TIMEOUT_MS: u64 = 1_500;
const MIN_EMBEDDING_TIMEOUT_MS: u64 = 100;
const MAX_EMBEDDING_TIMEOUT_MS: u64 = 10_000;

/// Local lexical vectors always use this many feature-hashed dimensions.
const VECTOR_DIMS: usize = 256;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    max_entries: usize,
    ttl_secs: i64,
    similarity_threshold: f64,
    scope_by_consumer: bool,
    scope_by_virtual_key: bool,
    max_prompt_chars: usize,
    max_response_bytes: usize,
    embedding: Option<EmbeddingConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            ttl_secs: DEFAULT_TTL_SECS,
            similarity_threshold: DEFAULT_SIMILARITY_THRESHOLD,
            scope_by_consumer: true,
            scope_by_virtual_key: true,
            max_prompt_chars: DEFAULT_MAX_PROMPT_CHARS,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            embedding: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbeddingConfig {
    base_url: String,
    model: String,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug)]
struct Embedding {
    endpoint: String,
    model: String,
    secret: Option<String>,
    timeout: Duration,
    client: reqwest::Client,
}

impl Embedding {
    fn from_config(config: EmbeddingConfig) -> Result<Self, PluginError> {
        let base_url = config.base_url.trim().trim_end_matches('/');
        if base_url.is_empty() {
            return Err(PluginError::fail_closed(
                "llm-semantic-cache embedding `base_url` is required",
            ));
        }
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(PluginError::fail_closed(
                "llm-semantic-cache embedding `base_url` must be an HTTP(S) URL",
            ));
        }
        let model = config.model.trim();
        if model.is_empty() {
            return Err(PluginError::fail_closed(
                "llm-semantic-cache embedding `model` is required",
            ));
        }
        let timeout_ms = config.timeout_ms.unwrap_or(DEFAULT_EMBEDDING_TIMEOUT_MS);
        if !(MIN_EMBEDDING_TIMEOUT_MS..=MAX_EMBEDDING_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(PluginError::fail_closed(format!(
                "llm-semantic-cache embedding `timeout_ms` must be between \
                 {MIN_EMBEDDING_TIMEOUT_MS} and {MAX_EMBEDDING_TIMEOUT_MS}"
            )));
        }
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
                    "cannot build the semantic cache embedding client: {error}"
                ))
            })?;
        Ok(Self {
            endpoint: format!("{base_url}/embeddings"),
            model: model.to_string(),
            secret,
            timeout: Duration::from_millis(timeout_ms),
            client,
        })
    }
}

/// One cached response. Prompt text is never stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    key: String,
    scope: String,
    vector: Vec<f32>,
    response: ChatResponse,
    created_at: i64,
    last_used: u64,
    #[serde(default)]
    hits: u64,
}

/// Request-stage state handed to the final-response hook.
#[derive(Debug, Serialize, Deserialize)]
struct Pending {
    key: String,
    scope: String,
    vector: Vec<f32>,
}

/// How the lookup matched, for structured observability.
#[derive(Debug, Clone, Copy)]
enum CacheMatch {
    Exact,
    Similarity(f64),
}

#[derive(Debug)]
struct CacheState {
    entries: Vec<Entry>,
    /// Logical clock for least-recently-used ordering. Logical timestamps keep
    /// eviction deterministic even when several entries are touched within the
    /// same wall-clock second.
    sequence: u64,
}
impl CacheState {
    fn remove_expired(&mut self, now: i64, ttl_secs: i64) {
        self.entries
            .retain(|entry| now.saturating_sub(entry.created_at) <= ttl_secs);
    }

    fn touch(&mut self, index: usize) -> ChatResponse {
        self.sequence = self.sequence.saturating_add(1);
        let entry = &mut self.entries[index];
        entry.last_used = self.sequence;
        entry.hits = entry.hits.saturating_add(1);
        entry.response.clone()
    }

    fn best_match(&self, scope: &str, vector: &[f32], threshold: f64) -> Option<(usize, f64)> {
        let mut best: Option<(usize, f64)> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.scope != scope || entry.vector.len() != vector.len() {
                continue;
            }
            let score = cosine_similarity(&entry.vector, vector);
            if score >= threshold && best.is_none_or(|(_, best_score)| score > best_score) {
                best = Some((index, score));
            }
        }
        best
    }

    fn store(&mut self, pending: Pending, response: ChatResponse, now: i64, max_entries: usize) {
        self.sequence = self.sequence.saturating_add(1);
        let sequence = self.sequence;
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.key == pending.key && entry.scope == pending.scope)
        {
            entry.vector = pending.vector;
            entry.response = response;
            entry.created_at = now;
            entry.last_used = sequence;
        } else {
            self.entries.push(Entry {
                key: pending.key,
                scope: pending.scope,
                vector: pending.vector,
                response,
                created_at: now,
                last_used: sequence,
                hits: 0,
            });
        }
        self.evict(max_entries);
    }

    fn evict(&mut self, max_entries: usize) {
        while self.entries.len() > max_entries {
            let Some((index, _)) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
            else {
                break;
            };
            self.entries.remove(index);
        }
    }
}

pub struct SemanticCache {
    manifest: PluginManifest,
    max_entries: usize,
    ttl_secs: i64,
    similarity_threshold: f64,
    scope_by_consumer: bool,
    scope_by_virtual_key: bool,
    max_prompt_chars: usize,
    max_response_bytes: usize,
    embedding: Option<Embedding>,
    cache: Mutex<CacheState>,
    state: Arc<dyn LlmPluginStateStore>,
    state_key: String,
}

pub struct SemanticCacheFactory;

impl LlmPluginFactory for SemanticCacheFactory {
    fn manifest(&self) -> PluginManifest {
        Self::make_manifest()
    }

    fn supported_scopes(&self) -> &'static [LlmScope] {
        &REQUEST_LLM_SCOPES
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        // Direct construction (tests, embeddings) has no host state store;
        // keep the cache in a private in-memory store.
        Ok(Arc::new(self.build(
            config,
            "adhoc",
            Arc::new(MemoryPluginStateStore::default()),
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

impl SemanticCacheFactory {
    fn make_manifest() -> PluginManifest {
        let mut manifest = manifest(
            PLUGIN_NAME,
            "Serve repeated prompts from a bounded local similarity cache",
        );
        manifest.priority = -100;
        manifest.fail_policy = FailPolicy::Open;
        manifest.capabilities = vec![Capability::RequestBody];
        manifest.config_schema = Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "llm-semantic-cache config",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "max_entries": {
                    "type": "integer",
                    "minimum": MIN_MAX_ENTRIES,
                    "maximum": MAX_MAX_ENTRIES,
                    "default": DEFAULT_MAX_ENTRIES
                },
                "ttl_secs": {
                    "type": "integer",
                    "minimum": MIN_TTL_SECS,
                    "maximum": MAX_TTL_SECS,
                    "default": DEFAULT_TTL_SECS
                },
                "similarity_threshold": {
                    "type": "number",
                    "minimum": 0.0,
                    "maximum": 1.0,
                    "default": DEFAULT_SIMILARITY_THRESHOLD,
                    "description": "1.0 means exact canonical-match only."
                },
                "scope_by_consumer": {
                    "type": "boolean",
                    "default": true
                },
                "scope_by_virtual_key": {
                    "type": "boolean",
                    "default": true
                },
                "max_prompt_chars": {
                    "type": "integer",
                    "minimum": MIN_MAX_PROMPT_CHARS,
                    "maximum": MAX_MAX_PROMPT_CHARS,
                    "default": DEFAULT_MAX_PROMPT_CHARS
                },
                "max_response_bytes": {
                    "type": "integer",
                    "minimum": MIN_MAX_RESPONSE_BYTES,
                    "maximum": MAX_MAX_RESPONSE_BYTES,
                    "default": DEFAULT_MAX_RESPONSE_BYTES
                },
                "embedding": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["base_url", "model"],
                    "properties": {
                        "base_url": {
                            "type": "string",
                            "description": "HTTP(S) base URL; `/embeddings` is appended."
                        },
                        "model": {"type": "string", "minLength": 1},
                        "secret": {
                            "type": "string",
                            "description": "Optional Bearer token."
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "minimum": MIN_EMBEDDING_TIMEOUT_MS,
                            "maximum": MAX_EMBEDDING_TIMEOUT_MS,
                            "default": DEFAULT_EMBEDDING_TIMEOUT_MS
                        }
                    }
                }
            }
        }));
        manifest
    }

    fn build(
        &self,
        config: Value,
        instance_key: &str,
        state: Arc<dyn LlmPluginStateStore>,
    ) -> Result<SemanticCache, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid llm-semantic-cache config: {error}"))
        })?;
        if !(MIN_MAX_ENTRIES..=MAX_MAX_ENTRIES).contains(&config.max_entries) {
            return Err(PluginError::fail_closed(format!(
                "llm-semantic-cache `max_entries` must be between \
                 {MIN_MAX_ENTRIES} and {MAX_MAX_ENTRIES}"
            )));
        }
        if !(MIN_TTL_SECS..=MAX_TTL_SECS).contains(&config.ttl_secs) {
            return Err(PluginError::fail_closed(format!(
                "llm-semantic-cache `ttl_secs` must be between \
                 {MIN_TTL_SECS} and {MAX_TTL_SECS}"
            )));
        }
        if !(0.0..=1.0).contains(&config.similarity_threshold) {
            return Err(PluginError::fail_closed(
                "llm-semantic-cache `similarity_threshold` must be between 0.0 and 1.0",
            ));
        }
        if !(MIN_MAX_PROMPT_CHARS..=MAX_MAX_PROMPT_CHARS).contains(&config.max_prompt_chars) {
            return Err(PluginError::fail_closed(format!(
                "llm-semantic-cache `max_prompt_chars` must be between \
                 {MIN_MAX_PROMPT_CHARS} and {MAX_MAX_PROMPT_CHARS}"
            )));
        }
        if !(MIN_MAX_RESPONSE_BYTES..=MAX_MAX_RESPONSE_BYTES).contains(&config.max_response_bytes) {
            return Err(PluginError::fail_closed(format!(
                "llm-semantic-cache `max_response_bytes` must be between \
                 {MIN_MAX_RESPONSE_BYTES} and {MAX_MAX_RESPONSE_BYTES}"
            )));
        }
        let embedding = config.embedding.map(Embedding::from_config).transpose()?;

        let mut manifest = Self::make_manifest();
        if let Some(embedding) = &embedding {
            manifest.timeout_ms = (embedding.timeout.as_millis() as u64).saturating_add(200);
        }

        let state_key = format!("{STATE_PREFIX}:{instance_key}");
        let loaded = state
            .get(&state_key)
            .and_then(|value| serde_json::from_value::<Vec<Entry>>(value).ok())
            .unwrap_or_default();
        let sequence = loaded
            .iter()
            .map(|entry| entry.last_used)
            .max()
            .unwrap_or(0);
        let mut cache = CacheState {
            entries: loaded,
            sequence,
        };
        // A reload may lower `max_entries`; keep the most recently used.
        cache.evict(config.max_entries);

        Ok(SemanticCache {
            manifest,
            max_entries: config.max_entries,
            ttl_secs: config.ttl_secs,
            similarity_threshold: config.similarity_threshold,
            scope_by_consumer: config.scope_by_consumer,
            scope_by_virtual_key: config.scope_by_virtual_key,
            max_prompt_chars: config.max_prompt_chars,
            max_response_bytes: config.max_response_bytes,
            embedding,
            cache: Mutex::new(cache),
            state,
            state_key,
        })
    }
}

impl SemanticCache {
    fn scope(&self, meta: &LlmRequestMeta<'_>, model: &str) -> String {
        let mut scope = format!("model:{model}");
        if self.scope_by_consumer {
            scope.push_str(";consumer:");
            scope.push_str(meta.consumer_id.unwrap_or(""));
        }
        if self.scope_by_virtual_key {
            scope.push_str(";virtual_key:");
            scope.push_str(meta.virtual_key_id.unwrap_or(""));
        }
        scope
    }

    fn exact_hit(&self, key: &str, scope: &str, now: i64) -> Option<ChatResponse> {
        let mut cache = self.cache.lock();
        cache.remove_expired(now, self.ttl_secs);
        let index = cache
            .entries
            .iter()
            .position(|entry| entry.key == key && entry.scope == scope)?;
        Some(cache.touch(index))
    }

    /// Calls the optional embeddings endpoint. Any failure falls back to the
    /// local lexical vector so a slow or broken endpoint never fails the
    /// request.
    async fn embedding_vector(&self, prompt: &str) -> Option<Vec<f32>> {
        let embedding = self.embedding.as_ref()?;
        let body = json!({"input": prompt, "model": embedding.model});
        let mut request = embedding.client.post(&embedding.endpoint).json(&body);
        if let Some(secret) = &embedding.secret {
            request = request.bearer_auth(secret);
        }
        let result = tokio::time::timeout(embedding.timeout, async {
            let response = request.send().await.map_err(|error| error.to_string())?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("HTTP {status}"));
            }
            let value: Value = response.json().await.map_err(|error| error.to_string())?;
            parse_embedding(&value).ok_or_else(|| "response has no data[0].embedding".to_string())
        })
        .await;
        match result {
            Ok(Ok(vector)) => Some(vector),
            Ok(Err(reason)) => {
                tracing::debug!(
                    reason,
                    "semantic cache embedding request failed; using the local vector"
                );
                None
            }
            Err(_) => {
                tracing::debug!(
                    timeout_ms = embedding.timeout.as_millis() as u64,
                    "semantic cache embedding request timed out; using the local vector"
                );
                None
            }
        }
    }

    fn persist(&self) {
        let entries = self.cache.lock().entries.clone();
        match serde_json::to_value(&entries) {
            Ok(value) => self.state.put(&self.state_key, value),
            Err(error) => {
                tracing::debug!(%error, "cannot serialize the semantic cache");
            }
        }
    }
}

#[async_trait]
impl LlmPlugin for SemanticCache {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
        let key = cache_key(cx.request);
        let scope = self.scope(cx.meta, &cx.request.model);
        let now = Utc::now().timestamp();

        // Fast path: the canonical request already exists in this scope, so
        // no similarity work (or embedding call) is needed.
        if let Some(response) = self.exact_hit(&key, &scope, now) {
            tracing::debug!(
                cache = "hit",
                match_kind = "exact",
                model = %cx.request.model,
                "semantic cache lookup"
            );
            *cx.state = Value::Null;
            self.persist();
            cx.response_headers.insert(CACHE_HEADER, "hit");
            return Ok(LlmAction::Respond {
                response: Box::new(response),
                source: LlmResponseSource::Cache,
            });
        }

        let prompt = prompt_text(cx.request, self.max_prompt_chars);
        let vector = match self.embedding_vector(&prompt).await {
            Some(vector) => vector,
            None => local_vector(&prompt),
        };

        // The embedding call may have taken a while; re-read the clock so TTL
        // expiry is evaluated against the current lookup time.
        let now = Utc::now().timestamp();
        let hit = {
            let mut cache = self.cache.lock();
            cache.remove_expired(now, self.ttl_secs);
            let matched = cache
                .entries
                .iter()
                .position(|entry| entry.key == key && entry.scope == scope)
                .map(|index| (index, CacheMatch::Exact))
                .or_else(|| {
                    if self.similarity_threshold < 1.0 {
                        cache
                            .best_match(&scope, &vector, self.similarity_threshold)
                            .map(|(index, score)| (index, CacheMatch::Similarity(score)))
                    } else {
                        None
                    }
                });
            matched.map(|(index, matched)| (cache.touch(index), matched))
        };
        if let Some((response, matched)) = hit {
            match matched {
                CacheMatch::Exact => tracing::debug!(
                    cache = "hit",
                    match_kind = "exact",
                    model = %cx.request.model,
                    "semantic cache lookup"
                ),
                CacheMatch::Similarity(score) => tracing::debug!(
                    cache = "hit",
                    match_kind = "similarity",
                    score,
                    model = %cx.request.model,
                    "semantic cache lookup"
                ),
            }
            *cx.state = Value::Null;
            self.persist();
            cx.response_headers.insert(CACHE_HEADER, "hit");
            return Ok(LlmAction::Respond {
                response: Box::new(response),
                source: LlmResponseSource::Cache,
            });
        }

        let pending = Pending { key, scope, vector };
        *cx.state = serde_json::to_value(pending).unwrap_or(Value::Null);
        cx.response_headers.insert(CACHE_HEADER, "miss");
        tracing::debug!(
            cache = "miss",
            model = %cx.request.model,
            "semantic cache lookup"
        );
        Ok(LlmAction::Continue)
    }

    async fn on_final_response(&self, cx: &mut LlmFinalResponseContext<'_>) {
        let pending = std::mem::take(cx.state);
        let Ok(pending) = serde_json::from_value::<Pending>(pending) else {
            return;
        };
        if pending.key.is_empty() {
            return;
        }
        // A truncated, filtered, or failed response is not a safe cache entry:
        // replaying it would make the truncation look like a normal answer.
        if !matches!(
            cx.response.finish_reason,
            FinishReason::Stop | FinishReason::ToolCalls
        ) {
            return;
        }
        // Reversible PII redaction restores values in the response stage. Such
        // a response must never be persisted (and a cache hit would bypass the
        // restore stage entirely).
        if cx.response.extra.contains_key("octra_pii_restored") {
            return;
        }
        let Ok(serialized) = serde_json::to_vec(cx.response) else {
            tracing::debug!("cannot serialize the response for the semantic cache");
            return;
        };
        if serialized.len() > self.max_response_bytes {
            return;
        }

        let now = Utc::now().timestamp();
        {
            let mut cache = self.cache.lock();
            cache.remove_expired(now, self.ttl_secs);
            cache.store(pending, cx.response.clone(), now, self.max_entries);
        }
        self.persist();
    }
}

/// Stable FNV-1a 64-bit hash. Persisted cache keys must not depend on
/// `DefaultHasher`, whose output is not stable across processes.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn cache_key(request: &ChatRequest) -> String {
    // Only response-affecting fields participate. `stream` and
    // `include_usage` are deliberately excluded.
    let canonical = json!({
        "model": request.model,
        "system": request.system,
        "messages": request.messages,
        "tools": request.tools,
        "tool_choice": request.tool_choice,
        "parallel_tool_calls": request.parallel_tool_calls,
        "temperature": request.temperature,
        "top_p": request.top_p,
        "top_k": request.top_k,
        "max_tokens": request.max_tokens,
        "stop": request.stop,
        "response_format": request.response_format,
        "reasoning_effort": request.reasoning_effort,
        "user": request.user,
        "extra": request.extra,
    });
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    format!("{:016x}", fnv1a64(&bytes))
}

/// Concatenated system text plus text blocks in messages. Tool-call arguments
/// are structured values and intentionally excluded, as are images and files.
fn prompt_text(request: &ChatRequest, max_chars: usize) -> String {
    let mut out = String::new();
    let mut push = |text: &str| {
        if out.is_empty() {
            out.push_str(text);
        } else {
            out.push('\n');
            out.push_str(text);
        }
    };
    for block in &request.system.blocks {
        push(block);
    }
    for message in &request.messages {
        if let Some(content) = &message.content {
            for part in &content.parts {
                if let ContentPart::Text { text, .. } = part {
                    push(text);
                }
            }
        }
        for block in &message.assistant {
            if let AssistantContent::Text { text } = block {
                push(text);
            }
        }
        for result in &message.tool_results {
            for part in &result.content.parts {
                if let ContentPart::Text { text, .. } = part {
                    push(text);
                }
            }
        }
    }
    truncate_chars(&out, max_chars)
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

fn parse_embedding(value: &Value) -> Option<Vec<f32>> {
    let values = value.pointer("/data/0/embedding")?.as_array()?;
    let mut vector = Vec::with_capacity(values.len());
    for value in values {
        vector.push(value.as_f64()? as f32);
    }
    (!vector.is_empty()).then_some(vector)
}

fn add_feature(vector: &mut [f32], feature: &str) {
    let index = (fnv1a64(feature.as_bytes()) as usize) % vector.len();
    vector[index] += 1.0;
}

/// Lowercase alphanumeric words plus character trigrams, feature-hashed into
/// [`VECTOR_DIMS`] dimensions and L2 normalized.
fn local_vector(text: &str) -> Vec<f32> {
    let lower = text.to_lowercase();
    let mut vector = vec![0.0f32; VECTOR_DIMS];
    for word in lower
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        add_feature(&mut vector, word);
    }
    let characters: Vec<char> = lower.chars().collect();
    for window in characters.windows(3) {
        if window.iter().all(|character| character.is_whitespace()) {
            continue;
        }
        let trigram: String = window.iter().collect();
        add_feature(&mut vector, &trigram);
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in &mut vector {
            *value /= norm;
        }
    }
    vector
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> f64 {
    if left.is_empty() || left.len() != right.len() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut left_norm = 0.0f64;
    let mut right_norm = 0.0f64;
    for (left, right) in left.iter().zip(right) {
        let left = f64::from(*left);
        let right = f64::from(*right);
        dot += left * right;
        left_norm += left * left;
        right_norm += right * right;
    }
    if left_norm == 0.0 || right_norm == 0.0 {
        return 0.0;
    }
    dot / (left_norm.sqrt() * right_norm.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{Message, Protocol, Role};
    use octra_llm_plugin_types::LlmResponseHeaders;

    fn meta<'a>(consumer: Option<&'a str>, virtual_key: Option<&'a str>) -> LlmRequestMeta<'a> {
        LlmRequestMeta {
            request_id: "request-1",
            ingress: Protocol::Openai,
            consumer_id: consumer,
            virtual_key_id: virtual_key,
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        }
    }

    fn build(
        config: Value,
        state: Arc<dyn LlmPluginStateStore>,
        instance_key: &str,
    ) -> Arc<SemanticCache> {
        Arc::new(
            SemanticCacheFactory
                .build(config, instance_key, state)
                .expect("valid config"),
        )
    }

    fn plugin(config: Value) -> Arc<SemanticCache> {
        build(
            config,
            Arc::new(MemoryPluginStateStore::default()),
            "instance-1",
        )
    }

    fn response(text: &str) -> ChatResponse {
        let mut message = Message::new(Role::Assistant);
        message.assistant.push(AssistantContent::Text {
            text: text.to_string(),
        });
        ChatResponse::new("response-1", "m", message)
    }

    async fn call_request(
        plugin: &dyn LlmPlugin,
        meta: &LlmRequestMeta<'_>,
        request: &mut ChatRequest,
    ) -> (LlmAction, LlmResponseHeaders, Value) {
        let mut headers = LlmResponseHeaders::default();
        let mut state = Value::Null;
        let action = {
            let mut cx = LlmRequestContext {
                meta,
                response_headers: &mut headers,
                request,
                state: &mut state,
            };
            plugin.on_request(&mut cx).await.unwrap()
        };
        (action, headers, state)
    }

    async fn call_final(
        plugin: &dyn LlmPlugin,
        meta: &LlmRequestMeta<'_>,
        request_model: &str,
        response: &ChatResponse,
        state: &mut Value,
    ) {
        let mut cx = LlmFinalResponseContext {
            meta,
            request_model,
            response,
            state,
        };
        plugin.on_final_response(&mut cx).await;
    }

    fn cache_header(headers: &LlmResponseHeaders, expected: &str) {
        let value = headers
            .iter()
            .find(|(name, _)| name.as_str() == "x-octra-cache")
            .and_then(|(_, value)| value.to_str().ok());
        assert_eq!(value, Some(expected));
    }

    fn pending(state: &Value) -> bool {
        state.get("key").is_some()
    }

    fn respond_text(action: &LlmAction) -> String {
        match action {
            LlmAction::Respond { response, .. } => response.text(),
            _ => panic!("expected a cache hit, got a non-respond action"),
        }
    }

    #[tokio::test]
    async fn exact_request_is_served_from_cache() {
        let plugin = plugin(json!({"similarity_threshold": 1.0}));
        let meta = meta(None, None);
        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);

        let (action, headers, mut state) = call_request(plugin.as_ref(), &meta, &mut request).await;
        assert!(matches!(action, LlmAction::Continue));
        cache_header(&headers, "miss");
        assert!(pending(&state));

        let stored = response("cached answer");
        call_final(plugin.as_ref(), &meta, "m", &stored, &mut state).await;

        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        let (action, headers, _) = call_request(plugin.as_ref(), &meta, &mut request).await;
        cache_header(&headers, "hit");
        assert_eq!(respond_text(&action), "cached answer");
    }

    #[tokio::test]
    async fn different_prompt_misses() {
        let plugin = plugin(json!({"similarity_threshold": 1.0}));
        let meta = meta(None, None);
        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        let (_, _, mut state) = call_request(plugin.as_ref(), &meta, &mut request).await;
        call_final(
            plugin.as_ref(),
            &meta,
            "m",
            &response("cached answer"),
            &mut state,
        )
        .await;

        let mut request = ChatRequest::new("m", vec![Message::user("something else")]);
        let (action, headers, _) = call_request(plugin.as_ref(), &meta, &mut request).await;
        assert!(matches!(action, LlmAction::Continue));
        cache_header(&headers, "miss");
    }

    #[tokio::test]
    async fn similar_prompts_hit_but_unrelated_prompts_miss() {
        let plugin = plugin(json!({"similarity_threshold": 0.5}));
        let meta = meta(None, None);
        let mut first = ChatRequest::new(
            "m",
            vec![Message::user("The quick brown fox jumps over the lazy dog")],
        );
        let (_, _, mut state) = call_request(plugin.as_ref(), &meta, &mut first).await;
        call_final(
            plugin.as_ref(),
            &meta,
            "m",
            &response("cached answer"),
            &mut state,
        )
        .await;

        let mut similar = ChatRequest::new(
            "m",
            vec![Message::user(
                "The quick brown fox jumped over the lazy dog",
            )],
        );
        let (action, headers, _) = call_request(plugin.as_ref(), &meta, &mut similar).await;
        cache_header(&headers, "hit");
        assert_eq!(respond_text(&action), "cached answer");

        let mut unrelated = ChatRequest::new(
            "m",
            vec![Message::user(
                "Quantum chromodynamics describes gluon interactions",
            )],
        );
        let (action, headers, _) = call_request(plugin.as_ref(), &meta, &mut unrelated).await;
        assert!(matches!(action, LlmAction::Continue));
        cache_header(&headers, "miss");
    }

    #[tokio::test]
    async fn expired_entries_are_not_served() {
        let plugin = plugin(json!({"similarity_threshold": 1.0, "ttl_secs": 60}));
        let meta = meta(None, None);
        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        let (_, _, mut state) = call_request(plugin.as_ref(), &meta, &mut request).await;
        call_final(
            plugin.as_ref(),
            &meta,
            "m",
            &response("cached answer"),
            &mut state,
        )
        .await;
        {
            let mut cache = plugin.cache.lock();
            cache.entries[0].created_at = Utc::now().timestamp() - 120;
        }

        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        let (action, headers, _) = call_request(plugin.as_ref(), &meta, &mut request).await;
        assert!(matches!(action, LlmAction::Continue));
        cache_header(&headers, "miss");
        assert!(plugin.cache.lock().entries.is_empty());
    }

    #[tokio::test]
    async fn eviction_keeps_the_newest_entries() {
        let plugin = plugin(json!({"similarity_threshold": 1.0, "max_entries": 2}));
        let meta = meta(None, None);
        for (prompt, answer) in [("one", "first"), ("two", "second"), ("three", "third")] {
            let mut request = ChatRequest::new("m", vec![Message::user(prompt)]);
            let (_, _, mut state) = call_request(plugin.as_ref(), &meta, &mut request).await;
            call_final(plugin.as_ref(), &meta, "m", &response(answer), &mut state).await;
        }
        assert_eq!(plugin.cache.lock().entries.len(), 2);

        let mut request = ChatRequest::new("m", vec![Message::user("one")]);
        let (action, headers, _) = call_request(plugin.as_ref(), &meta, &mut request).await;
        assert!(matches!(action, LlmAction::Continue));
        cache_header(&headers, "miss");

        for prompt in ["two", "three"] {
            let mut request = ChatRequest::new("m", vec![Message::user(prompt)]);
            let (action, _, _) = call_request(plugin.as_ref(), &meta, &mut request).await;
            assert!(matches!(action, LlmAction::Respond { .. }));
        }
    }

    #[tokio::test]
    async fn consumer_scope_isolates_cached_responses() {
        let plugin = plugin(json!({"similarity_threshold": 1.0}));
        let consumer_a = meta(Some("consumer-a"), None);
        let consumer_b = meta(Some("consumer-b"), None);

        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        let (_, _, mut state) = call_request(plugin.as_ref(), &consumer_a, &mut request).await;
        call_final(
            plugin.as_ref(),
            &consumer_a,
            "m",
            &response("cached answer"),
            &mut state,
        )
        .await;

        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        let (action, headers, _) = call_request(plugin.as_ref(), &consumer_b, &mut request).await;
        assert!(matches!(action, LlmAction::Continue));
        cache_header(&headers, "miss");

        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        let (action, _, _) = call_request(plugin.as_ref(), &consumer_a, &mut request).await;
        assert_eq!(respond_text(&action), "cached answer");
    }

    #[tokio::test]
    async fn streaming_request_hits_an_exact_cache_entry() {
        let plugin = plugin(json!({"similarity_threshold": 1.0}));
        let meta = meta(None, None);
        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        let (_, _, mut state) = call_request(plugin.as_ref(), &meta, &mut request).await;
        call_final(
            plugin.as_ref(),
            &meta,
            "m",
            &response("cached answer"),
            &mut state,
        )
        .await;

        let mut streaming = ChatRequest::new("m", vec![Message::user("hello")]);
        streaming.stream = Some(true);
        let (action, headers, _) = call_request(plugin.as_ref(), &meta, &mut streaming).await;
        cache_header(&headers, "hit");
        assert_eq!(respond_text(&action), "cached answer");
    }

    #[tokio::test]
    async fn stream_final_response_stores_the_accumulated_response() {
        let plugin = plugin(json!({"similarity_threshold": 1.0}));
        let meta = meta(None, None);
        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        request.stream = Some(true);
        let (action, headers, mut state) = call_request(plugin.as_ref(), &meta, &mut request).await;
        assert!(matches!(action, LlmAction::Continue));
        cache_header(&headers, "miss");
        assert!(pending(&state));
        assert!(
            !headers
                .iter()
                .any(|(_, value)| value.to_str().ok() == Some("bypass")),
            "streaming requests must not bypass the cache"
        );

        call_final(
            plugin.as_ref(),
            &meta,
            "m",
            &response("cached answer"),
            &mut state,
        )
        .await;

        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        request.stream = Some(true);
        let (action, headers, _) = call_request(plugin.as_ref(), &meta, &mut request).await;
        cache_header(&headers, "hit");
        assert_eq!(respond_text(&action), "cached answer");
    }

    #[test]
    fn prompt_text_ignores_tool_call_arguments() {
        let mut request = ChatRequest::new("m", vec![Message::user("hello world")]);
        let mut assistant = Message::new(Role::Assistant);
        assistant.assistant.push(AssistantContent::ToolCall {
            id: "call-1".to_string(),
            name: "lookup".to_string(),
            arguments: json!({"secret": "arguments must not leak"}),
            kind: None,
        });
        request.messages.push(assistant);

        let text = prompt_text(&request, 8_000);
        assert!(text.contains("hello world"));
        assert!(!text.contains("arguments must not leak"));

        let other = ChatRequest::new("m", vec![Message::user("hello world")]);
        assert_eq!(
            local_vector(&text),
            local_vector(&prompt_text(&other, 8_000))
        );
    }

    #[tokio::test]
    async fn oversized_responses_are_not_cached() {
        let plugin = plugin(json!({
            "similarity_threshold": 1.0,
            "max_response_bytes": 1024
        }));
        let meta = meta(None, None);
        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        let (_, _, mut state) = call_request(plugin.as_ref(), &meta, &mut request).await;
        call_final(
            plugin.as_ref(),
            &meta,
            "m",
            &response(&"x".repeat(4_000)),
            &mut state,
        )
        .await;
        assert!(plugin.cache.lock().entries.is_empty());

        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        let (action, headers, _) = call_request(plugin.as_ref(), &meta, &mut request).await;
        assert!(matches!(action, LlmAction::Continue));
        cache_header(&headers, "miss");
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        let state: Arc<dyn LlmPluginStateStore> = Arc::new(MemoryPluginStateStore::default());
        for config in [
            json!({"max_entries": 0}),
            json!({"max_entries": 4097}),
            json!({"ttl_secs": 0}),
            json!({"ttl_secs": 604_801}),
            json!({"similarity_threshold": -0.1}),
            json!({"similarity_threshold": 1.1}),
            json!({"max_prompt_chars": 0}),
            json!({"max_prompt_chars": 64_001}),
            json!({"max_response_bytes": 1023}),
            json!({"max_response_bytes": 16_777_217}),
            json!({"unknown": true}),
            json!({"embedding": {"base_url": "ftp://example.invalid", "model": "m"}}),
            json!({"embedding": {"base_url": "http://example.invalid", "model": ""}}),
            json!({"embedding": {"base_url": "http://example.invalid", "model": "m", "timeout_ms": 99}}),
            json!({"embedding": {"base_url": "http://example.invalid", "model": "m", "timeout_ms": 10_001}}),
            json!({"embedding": {"base_url": "http://example.invalid", "model": "m", "unknown": true}}),
        ] {
            let error = SemanticCacheFactory
                .build(config, "instance-1", state.clone())
                .err()
                .expect("invalid config must be rejected");
            assert_eq!(error.fail_policy, FailPolicy::Closed);
        }
        SemanticCacheFactory
            .build(json!({}), "instance-1", state.clone())
            .expect("default config is valid");
        SemanticCacheFactory
            .build(
                json!({"embedding": {"base_url": "http://example.invalid/v1", "model": "text-embedding-3-small"}}),
                "instance-1",
                state,
            )
            .expect("embedding config is valid");
    }

    #[tokio::test]
    async fn persisted_entries_survive_a_new_instance() {
        let state: Arc<dyn LlmPluginStateStore> = Arc::new(MemoryPluginStateStore::default());
        let first = build(
            json!({"similarity_threshold": 1.0}),
            state.clone(),
            "shared",
        );
        let meta = meta(None, None);
        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        let (_, _, mut slot) = call_request(first.as_ref(), &meta, &mut request).await;
        call_final(
            first.as_ref(),
            &meta,
            "m",
            &response("cached answer"),
            &mut slot,
        )
        .await;

        let second = build(json!({"similarity_threshold": 1.0}), state, "shared");
        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        let (action, headers, _) = call_request(second.as_ref(), &meta, &mut request).await;
        cache_header(&headers, "hit");
        assert_eq!(respond_text(&action), "cached answer");
    }

    #[tokio::test]
    async fn embedding_endpoint_is_used_when_configured() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let body = json!({"data": [{"embedding": [0.1, 0.2, 0.3]}]}).to_string();
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
                let body = body.clone();
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
        let config = json!({
            "similarity_threshold": 1.0,
            "embedding": {"base_url": format!("http://{address}/v1"), "model": "embed"}
        });
        let plugin = plugin(config);
        assert_eq!(plugin.manifest.timeout_ms, 1_700);
        let meta = meta(None, None);
        let mut request = ChatRequest::new("m", vec![Message::user("hello")]);
        let (_, _, mut state) = call_request(plugin.as_ref(), &meta, &mut request).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        call_final(
            plugin.as_ref(),
            &meta,
            "m",
            &response("cached answer"),
            &mut state,
        )
        .await;
        assert_eq!(plugin.cache.lock().entries[0].vector, vec![0.1, 0.2, 0.3]);
    }
}
