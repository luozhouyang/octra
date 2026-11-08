//! Optional OpenAI-compatible embedding runtime for semantic routing.
//!
//! Local lexical vectors are the default and the fallback. When an embedding
//! endpoint is configured, this runtime embeds the request text and the target
//! descriptions over HTTP, caches successful vectors in memory, and applies a
//! process-wide cooldown after any failure so a broken endpoint is not retried
//! on every request.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use octra_config::SemanticEmbeddingConfig;
use serde_json::json;

/// Successful vectors are reused for one hour.
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// Process-wide pause after any embedding request or parse failure.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

/// Prompt and target vectors for one routing decision. Targets are keyed by
/// the exact semantic description they were embedded from.
#[derive(Debug, Clone, Default)]
pub struct SemanticVectors {
    pub prompt: Vec<f32>,
    pub targets: HashMap<String, Vec<f32>>,
}

/// OpenAI-compatible embedding client with an in-memory vector cache.
pub struct EmbeddingRuntime {
    endpoint: String,
    model: String,
    auth: Option<String>,
    client: reqwest::Client,
    timeout: Duration,
    cache_entries: usize,
    cache: Mutex<VectorCache>,
}

struct CachedVector {
    vector: Vec<f32>,
    stored_at: Instant,
}

#[derive(Default)]
struct VectorCache {
    entries: HashMap<u64, CachedVector>,
    /// Least-recently-used keys, oldest first.
    order: VecDeque<u64>,
}

impl VectorCache {
    fn get(&mut self, key: u64) -> Option<Vec<f32>> {
        let cached = self.entries.get(&key)?;
        if cached.stored_at.elapsed() > CACHE_TTL {
            self.entries.remove(&key);
            self.order.retain(|entry| *entry != key);
            return None;
        }
        let vector = cached.vector.clone();
        self.touch(key);
        Some(vector)
    }

    fn touch(&mut self, key: u64) {
        if let Some(position) = self.order.iter().position(|entry| *entry == key) {
            self.order.remove(position);
        }
        self.order.push_back(key);
    }

    fn insert(&mut self, key: u64, vector: Vec<f32>, max_entries: usize) {
        if max_entries == 0 {
            return;
        }
        if self.entries.contains_key(&key) {
            self.order.retain(|entry| *entry != key);
        } else if self.entries.len() >= max_entries
            && let Some(oldest) = self.order.pop_front()
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(
            key,
            CachedVector {
                vector,
                stored_at: Instant::now(),
            },
        );
        self.order.push_back(key);
    }
}

fn text_key(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn cooldown() -> &'static Mutex<Option<Instant>> {
    static COOLDOWN: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    COOLDOWN.get_or_init(|| Mutex::new(None))
}

fn cooldown_guard() -> MutexGuard<'static, Option<Instant>> {
    cooldown()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether a recent embedding failure is still within the cooldown window.
fn in_cooldown() -> bool {
    let mut until = cooldown_guard();
    match *until {
        Some(deadline) if Instant::now() < deadline => true,
        Some(_) => {
            *until = None;
            false
        }
        None => false,
    }
}

fn start_cooldown() {
    *cooldown_guard() = Some(Instant::now() + FAILURE_COOLDOWN);
}

#[cfg(test)]
fn clear_cooldown() {
    *cooldown_guard() = None;
}

impl EmbeddingRuntime {
    /// Builds the runtime from optional configuration. Invalid configuration
    /// or an unresolvable `auth_ref` is reported so startup can disable
    /// embedding and keep lexical routing.
    pub fn from_config(
        config: Option<&SemanticEmbeddingConfig>,
    ) -> Result<Option<Arc<Self>>, String> {
        let Some(config) = config else {
            return Ok(None);
        };
        let base_url = config.base_url.trim().trim_end_matches('/');
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(
                "semantic embedding base_url must start with http:// or https://".to_string(),
            );
        }
        if config.model.trim().is_empty() {
            return Err("semantic embedding model must not be empty".to_string());
        }
        if !(100..=10_000).contains(&config.timeout_ms) {
            return Err("semantic embedding timeout_ms must be between 100 and 10000".to_string());
        }
        if !(1..=65_536).contains(&config.cache_entries) {
            return Err("semantic embedding cache_entries must be between 1 and 65536".to_string());
        }
        let auth = config
            .auth_ref
            .as_deref()
            .map(crate::upstream::resolve_secret_ref)
            .transpose()
            .map_err(|error| format!("semantic embedding auth_ref: {error}"))?;
        let timeout = Duration::from_millis(config.timeout_ms);
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()
            .map_err(|error| format!("cannot build semantic embedding client: {error}"))?;
        Ok(Some(Arc::new(Self {
            endpoint: format!("{base_url}/embeddings"),
            model: config.model.trim().to_string(),
            auth,
            client,
            timeout,
            cache_entries: config.cache_entries,
            cache: Mutex::new(VectorCache::default()),
        })))
    }

    /// Embeds the prompt and every distinct target description. Returns `None`
    /// when the prompt cannot be embedded or no target vector succeeds, so the
    /// caller falls back to lexical scoring.
    pub async fn routing_vectors(
        &self,
        prompt: &str,
        descriptions: impl Iterator<Item = &str>,
    ) -> Option<SemanticVectors> {
        let prompt_vector = self.embed(prompt).await?;
        if prompt_vector.is_empty() {
            return None;
        }
        let mut seen = HashSet::new();
        let mut targets = HashMap::new();
        for description in descriptions {
            if !seen.insert(description) {
                continue;
            }
            if let Some(vector) = self.embed(description).await {
                targets.insert(description.to_string(), vector);
            }
        }
        if targets.is_empty() {
            return None;
        }
        Some(SemanticVectors {
            prompt: prompt_vector,
            targets,
        })
    }

    async fn embed(&self, text: &str) -> Option<Vec<f32>> {
        let key = text_key(text);
        let cached = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key);
        if cached.is_some() {
            return cached;
        }
        if in_cooldown() {
            return None;
        }
        let mut request = self.client.post(&self.endpoint).json(&json!({
            "input": text,
            "model": self.model,
        }));
        if let Some(auth) = &self.auth {
            request = request.bearer_auth(auth);
        }
        let result = tokio::time::timeout(self.timeout, async {
            let response = request.send().await.map_err(|error| error.to_string())?;
            if !response.status().is_success() {
                return Err(format!(
                    "embedding endpoint returned status {}",
                    response.status()
                ));
            }
            let value: serde_json::Value =
                response.json().await.map_err(|error| error.to_string())?;
            parse_embedding(&value)
        })
        .await;
        let vector = match result {
            Ok(Ok(vector)) => vector,
            Ok(Err(error)) => {
                start_cooldown();
                tracing::debug!(
                    %error,
                    "semantic embedding request failed; using lexical fallback"
                );
                return None;
            }
            Err(_) => {
                start_cooldown();
                tracing::debug!("semantic embedding request timed out; using lexical fallback");
                return None;
            }
        };
        if vector.is_empty() {
            start_cooldown();
            return None;
        }
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, vector.clone(), self.cache_entries);
        Some(vector)
    }
}

fn parse_embedding(value: &serde_json::Value) -> Result<Vec<f32>, String> {
    let embedding = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .and_then(|data| data.first())
        .and_then(|entry| entry.get("embedding"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "embedding response has no data[0].embedding".to_string())?;
    if embedding.is_empty() {
        return Err("embedding response is empty".to_string());
    }
    embedding
        .iter()
        .map(serde_json::Value::as_f64)
        .map(|number| number.map(|number| number as f32))
        .collect::<Option<Vec<f32>>>()
        .ok_or_else(|| "embedding response contains a non-numeric value".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::response::Response;
    use axum::routing::any;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Copy)]
    enum Behavior {
        Echo,
        Fail,
    }

    #[derive(Clone)]
    struct MockState {
        requests: Arc<AtomicUsize>,
        behavior: Behavior,
    }

    struct MockServer {
        base_url: String,
        requests: Arc<AtomicUsize>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    impl MockServer {
        async fn start(behavior: Behavior) -> Self {
            let requests = Arc::new(AtomicUsize::new(0));
            let app = Router::new()
                .route("/{*path}", any(mock_embeddings))
                .with_state(MockState {
                    requests: requests.clone(),
                    behavior,
                });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let handle = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self {
                base_url: format!("http://{address}/v1"),
                requests,
                handle,
            }
        }

        fn request_count(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    async fn mock_embeddings(State(state): State<MockState>, body: axum::body::Bytes) -> Response {
        state.requests.fetch_add(1, Ordering::SeqCst);
        match state.behavior {
            Behavior::Fail => Response::builder()
                .status(500)
                .body(Body::from("{}"))
                .unwrap(),
            Behavior::Echo => {
                let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let input = value
                    .get("input")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let vector = if input.contains("vision") || input.contains("image") {
                    vec![1.0, 0.0]
                } else {
                    vec![0.0, 1.0]
                };
                Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"data": [{"embedding": vector}]}).to_string(),
                    ))
                    .unwrap()
            }
        }
    }

    async fn test_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        LOCK.lock().await
    }

    fn config(base_url: &str) -> SemanticEmbeddingConfig {
        SemanticEmbeddingConfig {
            base_url: base_url.to_string(),
            model: "test-embedding".into(),
            auth_ref: None,
            timeout_ms: 1_000,
            cache_entries: 64,
        }
    }

    #[tokio::test]
    async fn parses_embedding_response() {
        let _guard = test_lock().await;
        clear_cooldown();
        let server = MockServer::start(Behavior::Echo).await;
        let runtime = EmbeddingRuntime::from_config(Some(&config(&server.base_url)))
            .unwrap()
            .unwrap();

        let vectors = runtime
            .routing_vectors(
                "describe this vision image",
                ["vision and image analysis", "code generation"].into_iter(),
            )
            .await
            .expect("vectors");
        assert_eq!(vectors.prompt, vec![1.0, 0.0]);
        assert_eq!(
            vectors.targets.get("vision and image analysis"),
            Some(&vec![1.0, 0.0])
        );
        assert_eq!(
            vectors.targets.get("code generation"),
            Some(&vec![0.0, 1.0])
        );
        assert_eq!(server.request_count(), 3);
    }

    #[tokio::test]
    async fn reuses_cached_vectors() {
        let _guard = test_lock().await;
        clear_cooldown();
        let server = MockServer::start(Behavior::Echo).await;
        let runtime = EmbeddingRuntime::from_config(Some(&config(&server.base_url)))
            .unwrap()
            .unwrap();

        runtime
            .routing_vectors("prompt", ["first target", "second target"].into_iter())
            .await
            .expect("first vectors");
        assert_eq!(server.request_count(), 3);

        runtime
            .routing_vectors("prompt", ["first target", "second target"].into_iter())
            .await
            .expect("cached vectors");
        assert_eq!(server.request_count(), 3);
    }

    #[tokio::test]
    async fn failure_starts_a_cooldown() {
        let _guard = test_lock().await;
        clear_cooldown();
        let server = MockServer::start(Behavior::Fail).await;
        let runtime = EmbeddingRuntime::from_config(Some(&config(&server.base_url)))
            .unwrap()
            .unwrap();

        assert!(
            runtime
                .routing_vectors("prompt", ["target"].into_iter())
                .await
                .is_none()
        );
        assert_eq!(server.request_count(), 1);

        // The cooldown suppresses every further attempt, including for a
        // different prompt.
        assert!(
            runtime
                .routing_vectors("another prompt", ["other target"].into_iter())
                .await
                .is_none()
        );
        assert_eq!(server.request_count(), 1);
        clear_cooldown();
    }

    #[tokio::test]
    async fn rejects_invalid_config_and_secrets() {
        let _guard = test_lock().await;
        clear_cooldown();
        assert!(EmbeddingRuntime::from_config(None).unwrap().is_none());

        let valid = config("http://127.0.0.1:11434/v1");
        assert!(EmbeddingRuntime::from_config(Some(&valid)).is_ok());

        let mut invalid = valid.clone();
        invalid.base_url = "ftp://127.0.0.1/v1".into();
        assert!(EmbeddingRuntime::from_config(Some(&invalid)).is_err());

        let mut invalid = valid.clone();
        invalid.model = "  ".into();
        assert!(EmbeddingRuntime::from_config(Some(&invalid)).is_err());

        let mut invalid = valid.clone();
        invalid.timeout_ms = 99;
        assert!(EmbeddingRuntime::from_config(Some(&invalid)).is_err());

        let mut invalid = valid.clone();
        invalid.cache_entries = 0;
        assert!(EmbeddingRuntime::from_config(Some(&invalid)).is_err());

        let mut invalid = valid.clone();
        invalid.auth_ref = Some("plaintext-secret".into());
        assert!(EmbeddingRuntime::from_config(Some(&invalid)).is_err());

        let mut invalid = valid;
        invalid.auth_ref = Some("env:OCTRA_TEST_MISSING_EMBEDDING_KEY".into());
        assert!(EmbeddingRuntime::from_config(Some(&invalid)).is_err());
    }
}
