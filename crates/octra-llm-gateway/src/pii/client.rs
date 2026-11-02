//! PII engine clients built from `llm_pii_engines` records.
//!
//! External engines are user-managed services reachable over loopback HTTP or
//! a Unix domain socket. The host owns construction; plugins only receive the
//! [`PiiDetectorClient`] handle.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use octra_pii_types::{
    DetectRequest, Detection, EngineReadiness, PiiDetectorClient, PiiEngineError,
    PiiEngineErrorKind, PiiKind,
};
use octra_storage::LlmPiiEngineRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct EngineSettings {
    pub timeout: Duration,
}

impl EngineSettings {
    fn from_record(record: &LlmPiiEngineRecord) -> Self {
        let config: Value = serde_json::from_str(&record.config_json).unwrap_or(Value::Null);
        let timeout_ms = config
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(200)
            .max(1);
        Self {
            timeout: Duration::from_millis(timeout_ms),
        }
    }
}

pub(crate) fn readiness_code(readiness: EngineReadiness) -> u8 {
    readiness as u8
}

pub(crate) fn readiness_from_code(value: u8) -> EngineReadiness {
    match value {
        1 => EngineReadiness::Starting,
        2 => EngineReadiness::Degraded,
        3 => EngineReadiness::Failed,
        _ => EngineReadiness::Ready,
    }
}

pub(crate) fn build_uds_client(
    path: String,
    timeout: Duration,
) -> (Arc<dyn PiiDetectorClient>, Arc<AtomicU8>) {
    let readiness = Arc::new(AtomicU8::new(readiness_code(EngineReadiness::Starting)));
    let client = Arc::new(UdsEngineClient {
        path,
        timeout,
        readiness: readiness.clone(),
    });
    (client, readiness)
}

pub fn build_client(record: &LlmPiiEngineRecord) -> Result<Arc<dyn PiiDetectorClient>, String> {
    let settings = EngineSettings::from_record(record);
    match record.transport.as_str() {
        "http_loopback" => {
            let endpoint = record
                .endpoint
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "http_loopback engine requires `endpoint`".to_string())?;
            if !is_loopback_url(endpoint) {
                return Err(format!(
                    "PII engine endpoint '{endpoint}' must be a loopback http(s) URL"
                ));
            }
            let client = reqwest::Client::builder()
                .timeout(settings.timeout)
                .no_proxy()
                .build()
                .map_err(|error| format!("cannot build PII engine client: {error}"))?;
            Ok(Arc::new(HttpEngineClient {
                client,
                endpoint: endpoint.trim_end_matches('/').to_string(),
                timeout: settings.timeout,
                readiness: AtomicU8::new(readiness_code(EngineReadiness::Ready)),
            }))
        }
        "uds_jsonl" => {
            let endpoint = record
                .endpoint
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "uds_jsonl engine requires `endpoint`".to_string())?;
            let path = endpoint
                .strip_prefix("unix:")
                .unwrap_or(endpoint)
                .to_string();
            if !path.starts_with('/') {
                return Err("uds_jsonl `endpoint` must be an absolute socket path".to_string());
            }
            #[cfg(unix)]
            {
                let (client, readiness) = build_uds_client(path, settings.timeout);
                readiness.store(readiness_code(EngineReadiness::Ready), Ordering::Relaxed);
                Ok(client)
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                Err("uds_jsonl engines are only supported on Unix".to_string())
            }
        }
        other => Err(format!("unknown PII engine transport '{other}'")),
    }
}

/// Validates an engine endpoint for the given transport without building a
/// client. Control-plane writes use this to reject unusable definitions.
pub fn validate_endpoint(transport: &str, endpoint: &str) -> Result<(), String> {
    match transport {
        "http_loopback" => {
            if is_loopback_url(endpoint) {
                Ok(())
            } else {
                Err(format!(
                    "PII engine endpoint '{endpoint}' must be a loopback http(s) URL"
                ))
            }
        }
        "uds_jsonl" => {
            let path = endpoint.strip_prefix("unix:").unwrap_or(endpoint);
            if path.starts_with('/') {
                Ok(())
            } else {
                Err("uds_jsonl `endpoint` must be an absolute socket path".to_string())
            }
        }
        other => Err(format!("unknown PII engine transport '{other}'")),
    }
}

fn is_loopback_url(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    matches!(
        url.host_str(),
        Some("127.0.0.1") | Some("localhost") | Some("::1") | Some("[::1]")
    )
}

#[derive(Serialize)]
struct WireRequest<'a> {
    id: String,
    text: &'a str,
    locale: Option<&'a str>,
    kinds: Vec<&'a str>,
    max_bytes: usize,
}

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    entities: Vec<WireEntity>,
}

#[derive(Deserialize)]
struct WireEntity {
    kind: String,
    start: usize,
    end: usize,
    #[serde(default)]
    score: Option<f32>,
}

fn wire_request<'a>(request: DetectRequest<'a>) -> WireRequest<'a> {
    WireRequest {
        id: uuid::Uuid::new_v4().to_string(),
        text: request.text,
        locale: request.locale,
        kinds: request.kinds.iter().map(PiiKind::label).collect(),
        max_bytes: request.max_bytes,
    }
}

fn kind_from_label(label: &str) -> PiiKind {
    match label {
        "email" => PiiKind::Email,
        "phone" => PiiKind::Phone,
        "id_card" => PiiKind::IdCard,
        "bank_card" => PiiKind::BankCard,
        "api_key" => PiiKind::ApiKey,
        "ip" => PiiKind::Ip,
        other => PiiKind::Custom(other.to_string()),
    }
}

fn decode_entities(response: WireResponse, text: &str) -> Result<Vec<Detection>, PiiEngineError> {
    let mut detections = Vec::with_capacity(response.entities.len());
    for entity in response.entities {
        if entity.start >= entity.end
            || entity.end > text.len()
            || !text.is_char_boundary(entity.start)
            || !text.is_char_boundary(entity.end)
        {
            return Err(PiiEngineError::new(
                PiiEngineErrorKind::Protocol,
                "PII engine returned an invalid span",
            ));
        }
        detections.push(Detection {
            kind: kind_from_label(&entity.kind),
            start: entity.start,
            end: entity.end,
            confidence: entity.score.unwrap_or(0.9),
            detector: "engine",
        });
    }
    Ok(detections)
}

struct HttpEngineClient {
    client: reqwest::Client,
    endpoint: String,
    timeout: Duration,
    readiness: AtomicU8,
}

#[async_trait]
impl PiiDetectorClient for HttpEngineClient {
    fn readiness(&self) -> EngineReadiness {
        readiness_from_code(self.readiness.load(Ordering::Relaxed))
    }

    async fn detect(&self, request: DetectRequest<'_>) -> Result<Vec<Detection>, PiiEngineError> {
        let text = request.text;
        let body = wire_request(request);
        let response = self
            .client
            .post(format!("{}/detect", self.endpoint))
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    PiiEngineError::new(PiiEngineErrorKind::Timeout, "PII engine timed out")
                } else {
                    PiiEngineError::new(
                        PiiEngineErrorKind::Unavailable,
                        "PII engine is unreachable",
                    )
                }
            })?;
        if !response.status().is_success() {
            return Err(PiiEngineError::new(
                PiiEngineErrorKind::Unavailable,
                format!("PII engine returned {}", response.status()),
            ));
        }
        let parsed: WireResponse = response.json().await.map_err(|error| {
            PiiEngineError::new(
                PiiEngineErrorKind::Protocol,
                format!("PII engine returned invalid JSON: {error}"),
            )
        })?;
        decode_entities(parsed, text)
    }
}

#[cfg(unix)]
struct UdsEngineClient {
    path: String,
    timeout: Duration,
    readiness: Arc<AtomicU8>,
}

#[cfg(unix)]
#[async_trait]
impl PiiDetectorClient for UdsEngineClient {
    fn readiness(&self) -> EngineReadiness {
        readiness_from_code(self.readiness.load(Ordering::Relaxed))
    }

    async fn detect(&self, request: DetectRequest<'_>) -> Result<Vec<Detection>, PiiEngineError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let text = request.text;
        let body = wire_request(request);
        let mut line = serde_json::to_vec(&body).map_err(|error| {
            PiiEngineError::new(
                PiiEngineErrorKind::Protocol,
                format!("cannot encode PII engine request: {error}"),
            )
        })?;
        line.push(b'\n');

        let exchange = async {
            let stream = tokio::net::UnixStream::connect(&self.path)
                .await
                .map_err(|error| {
                    PiiEngineError::new(
                        PiiEngineErrorKind::Unavailable,
                        format!("cannot connect to PII engine socket: {error}"),
                    )
                })?;
            let mut reader = BufReader::new(stream);
            let mut response = String::new();
            reader.get_mut().write_all(&line).await.map_err(|error| {
                PiiEngineError::new(
                    PiiEngineErrorKind::Unavailable,
                    format!("cannot write to PII engine socket: {error}"),
                )
            })?;
            reader.read_line(&mut response).await.map_err(|error| {
                PiiEngineError::new(
                    PiiEngineErrorKind::Unavailable,
                    format!("cannot read from PII engine socket: {error}"),
                )
            })?;
            Ok::<String, PiiEngineError>(response)
        };

        let response = tokio::time::timeout(self.timeout, exchange)
            .await
            .map_err(|_| {
                PiiEngineError::new(PiiEngineErrorKind::Timeout, "PII engine timed out")
            })??;
        let parsed: WireResponse = serde_json::from_str(response.trim()).map_err(|error| {
            PiiEngineError::new(
                PiiEngineErrorKind::Protocol,
                format!("PII engine returned invalid JSON: {error}"),
            )
        })?;
        decode_entities(parsed, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::routing::post;
    use serde_json::{Value, json};

    fn record(endpoint: String, transport: &str) -> LlmPiiEngineRecord {
        LlmPiiEngineRecord {
            id: "eng".into(),
            name: "engine".into(),
            mode: "external".into(),
            transport: transport.into(),
            endpoint: Some(endpoint),
            command: None,
            args: Vec::new(),
            env_allow: Vec::new(),
            model_dir: None,
            config_json: "{\"timeout_ms\":500}".into(),
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    async fn spawn_engine() -> (String, tokio::task::JoinHandle<()>) {
        async fn detect(Json(body): Json<Value>) -> Json<Value> {
            let text = body["text"].as_str().unwrap_or_default();
            let start = text.find("Alice").unwrap_or(0);
            Json(json!({
                "entities": [
                    {"kind": "person", "start": start, "end": start + 5, "score": 0.93}
                ]
            }))
        }

        let app = axum::Router::new().route("/detect", post(detect));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), handle)
    }

    #[tokio::test]
    async fn http_engine_decodes_entities() {
        let (endpoint, handle) = spawn_engine().await;
        let client = build_client(&record(endpoint, "http_loopback")).unwrap();
        let detections = client
            .detect(DetectRequest {
                text: "hi Alice",
                locale: None,
                kinds: &[],
                max_bytes: 1024,
            })
            .await
            .unwrap();
        handle.abort();
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].kind, PiiKind::Custom("person".into()));
        assert_eq!(detections[0].start, 3);
        assert_eq!(detections[0].end, 8);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uds_engine_exchanges_json_lines() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let path = std::path::PathBuf::from(format!(
            "/tmp/octra-pii-{}.sock",
            uuid::Uuid::new_v4().simple()
        ));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let value: Value = serde_json::from_str(line.trim()).unwrap();
            let text = value["text"].as_str().unwrap();
            let start = text.find("Alice").unwrap();
            let response = json!({
                "entities": [{"kind": "person", "start": start, "end": start + 5}]
            });
            reader
                .get_mut()
                .write_all(format!("{}\n", response).as_bytes())
                .await
                .unwrap();
        });

        let (client, readiness) =
            build_uds_client(path.to_string_lossy().into_owned(), Duration::from_secs(2));
        readiness.store(readiness_code(EngineReadiness::Ready), Ordering::Relaxed);
        let detections = client
            .detect(DetectRequest {
                text: "hi Alice",
                locale: None,
                kinds: &[],
                max_bytes: 1024,
            })
            .await
            .unwrap();
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(detections.len(), 1);
        assert_eq!(detections[0].kind, PiiKind::Custom("person".into()));
    }

    #[test]
    fn endpoint_validation_rejects_remote_and_relative_paths() {
        assert!(validate_endpoint("http_loopback", "http://127.0.0.1:9000").is_ok());
        assert!(validate_endpoint("http_loopback", "http://localhost:9000").is_ok());
        assert!(validate_endpoint("http_loopback", "https://example.com").is_err());
        assert!(validate_endpoint("http_loopback", "ftp://127.0.0.1/x").is_err());
        assert!(validate_endpoint("uds_jsonl", "/run/pii.sock").is_ok());
        assert!(validate_endpoint("uds_jsonl", "run/pii.sock").is_err());
        assert!(validate_endpoint("tcp", "127.0.0.1:1").is_err());
    }
}
