//! Optional OTLP/HTTP JSON metrics export for the Octra daemon.
//!
//! The exporter is deliberately small: it converts one in-memory snapshot into
//! the OTLP JSON encoding and posts it to `{endpoint}/v1/metrics`. Failures are
//! returned to the caller so the daemon can log them without stopping.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use octra_config::TelemetryConfig;
use serde_json::{Value, json};

const SCOPE_NAME: &str = "octra";
const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;
/// OTLP cumulative temporality.
const CUMULATIVE_TEMPORALITY: u8 = 2;

/// API data-plane counters captured from the gateway metrics registry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApiSnapshot {
    pub requests_total: u64,
    pub status_2xx: u64,
    pub status_3xx: u64,
    pub status_4xx: u64,
    pub status_5xx: u64,
    pub upstream_errors: u64,
    pub retries: u64,
    pub latency_micros_total: u64,
}

/// Event outbox counters captured from storage.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventSnapshot {
    pub pending_events: i64,
    pub delivered_events: i64,
    pub failed_events: i64,
    pub pending_deliveries: i64,
    pub failed_deliveries: i64,
    pub retrying_deliveries: i64,
    pub oldest_pending_age_secs: i64,
    pub total_attempts: i64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TelemetrySnapshot {
    pub api: ApiSnapshot,
    pub events: EventSnapshot,
}

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("telemetry.otlp is not configured")]
    MissingOtlp,
    #[error("invalid telemetry.otlp.endpoint '{endpoint}': {reason}")]
    InvalidEndpoint { endpoint: String, reason: String },
    #[error("cannot send OTLP metrics: {0}")]
    Http(#[from] reqwest::Error),
    #[error("OTLP collector returned HTTP {status}: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
    },
}

/// Posts OTLP/HTTP JSON metrics to a collector.
pub struct OtlpExporter {
    endpoint: String,
    headers: BTreeMap<String, String>,
    service_name: String,
    client: reqwest::Client,
    started_at_unix_nanos: u128,
}

impl OtlpExporter {
    /// Creates an exporter when `config.otlp` is configured.
    pub fn new(
        config: &TelemetryConfig,
        http_client: reqwest::Client,
    ) -> Result<Self, TelemetryError> {
        let otlp = config.otlp.as_ref().ok_or(TelemetryError::MissingOtlp)?;
        let endpoint = otlp.endpoint.trim().trim_end_matches('/');
        let parsed =
            url::Url::parse(endpoint).map_err(|error| TelemetryError::InvalidEndpoint {
                endpoint: endpoint.to_string(),
                reason: error.to_string(),
            })?;
        if !(parsed.scheme() == "http" || parsed.scheme() == "https") || parsed.host_str().is_none()
        {
            return Err(TelemetryError::InvalidEndpoint {
                endpoint: endpoint.to_string(),
                reason: "expected an absolute http:// or https:// URL".to_string(),
            });
        }

        Ok(Self {
            endpoint: endpoint.to_string(),
            headers: otlp.headers.clone(),
            service_name: config.service_name.clone(),
            client: http_client,
            started_at_unix_nanos: unix_nanos(),
        })
    }

    /// Full metrics URL, joining the configured endpoint safely.
    pub fn metrics_endpoint(&self) -> String {
        format!("{}/v1/metrics", self.endpoint)
    }

    /// Builds the OTLP JSON payload for one snapshot.
    pub fn payload(&self, snapshot: &TelemetrySnapshot) -> Value {
        let now = unix_nanos();
        let start = self.started_at_unix_nanos;
        let api = &snapshot.api;
        let events = &snapshot.events;

        let response_points = [
            ("2xx", api.status_2xx),
            ("3xx", api.status_3xx),
            ("4xx", api.status_4xx),
            ("5xx", api.status_5xx),
        ]
        .into_iter()
        .map(|(class, value)| {
            let mut point = int_point(value, start, now);
            point["attributes"] = json!([
                {"key": "http.status_class", "value": {"stringValue": class}}
            ]);
            point
        })
        .collect();

        let metrics = vec![
            sum_metric(
                "octra.api.requests",
                vec![int_point(api.requests_total, start, now)],
            ),
            sum_metric("octra.api.responses", response_points),
            sum_metric(
                "octra.api.upstream_errors",
                vec![int_point(api.upstream_errors, start, now)],
            ),
            sum_metric(
                "octra.api.retries",
                vec![int_point(api.retries, start, now)],
            ),
            sum_metric(
                "octra.api.request.latency",
                vec![double_point(
                    api.latency_micros_total as f64 / 1_000_000.0,
                    start,
                    now,
                )],
            ),
            sum_metric(
                "octra.api.request.count",
                vec![int_point(api.requests_total, start, now)],
            ),
            gauge_metric("octra.event.pending", events.pending_events, start, now),
            gauge_metric("octra.event.delivered", events.delivered_events, start, now),
            gauge_metric("octra.event.failed", events.failed_events, start, now),
            gauge_metric(
                "octra.event.deliveries.pending",
                events.pending_deliveries,
                start,
                now,
            ),
            gauge_metric(
                "octra.event.deliveries.failed",
                events.failed_deliveries,
                start,
                now,
            ),
            gauge_metric(
                "octra.event.deliveries.retrying",
                events.retrying_deliveries,
                start,
                now,
            ),
            gauge_metric(
                "octra.event.oldest_pending_age_seconds",
                events.oldest_pending_age_secs,
                start,
                now,
            ),
            gauge_metric("octra.event.attempts", events.total_attempts, start, now),
        ];

        json!({
            "resourceMetrics": [{
                "resource": {
                    "attributes": [{
                        "key": "service.name",
                        "value": {"stringValue": self.service_name},
                    }],
                },
                "scopeMetrics": [{
                    "scope": {"name": SCOPE_NAME},
                    "metrics": metrics,
                }],
            }],
        })
    }

    /// Sends one snapshot. Non-2xx responses are reported as errors.
    pub async fn export(&self, snapshot: &TelemetrySnapshot) -> Result<(), TelemetryError> {
        let mut request = self
            .client
            .post(self.metrics_endpoint())
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }

        let response = request.json(&self.payload(snapshot)).send().await?;
        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = read_bounded_body(response).await;
        Err(TelemetryError::Status { status, body })
    }
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn non_negative(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn int_point(value: u64, start: u128, time: u128) -> Value {
    json!({
        "asInt": value.to_string(),
        "startTimeUnixNano": start.to_string(),
        "timeUnixNano": time.to_string(),
    })
}

fn double_point(value: f64, start: u128, time: u128) -> Value {
    json!({
        "asDouble": value,
        "startTimeUnixNano": start.to_string(),
        "timeUnixNano": time.to_string(),
    })
}

fn sum_metric(name: &str, points: Vec<Value>) -> Value {
    json!({
        "name": name,
        "sum": {
            "dataPoints": points,
            "aggregationTemporality": CUMULATIVE_TEMPORALITY,
            "isMonotonic": true,
        },
    })
}

fn gauge_metric(name: &str, value: i64, start: u128, time: u128) -> Value {
    json!({
        "name": name,
        "gauge": {
            "dataPoints": [int_point(non_negative(value), start, time)],
        },
    })
}

async fn read_bounded_body(mut response: reqwest::Response) -> String {
    let mut body = Vec::new();
    while body.len() < MAX_ERROR_BODY_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = MAX_ERROR_BODY_BYTES - body.len();
                body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            Ok(None) | Err(_) => break,
        }
    }
    String::from_utf8_lossy(&body).into_owned()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    struct CapturedRequest {
        request_line: String,
        headers: String,
        body: Vec<u8>,
    }

    fn telemetry_config(endpoint: &str) -> TelemetryConfig {
        TelemetryConfig {
            enabled: true,
            interval_secs: 30,
            service_name: "octra-test".to_string(),
            otlp: Some(octra_config::OtlpConfig {
                endpoint: endpoint.to_string(),
                headers: BTreeMap::from([("x-test".to_string(), "yes".to_string())]),
            }),
        }
    }

    fn sample_snapshot() -> TelemetrySnapshot {
        TelemetrySnapshot {
            api: ApiSnapshot {
                requests_total: 7,
                status_2xx: 4,
                status_3xx: 1,
                status_4xx: 1,
                status_5xx: 1,
                upstream_errors: 2,
                retries: 3,
                latency_micros_total: 1_500_000,
            },
            events: EventSnapshot {
                pending_events: 5,
                delivered_events: 9,
                failed_events: 2,
                pending_deliveries: 4,
                failed_deliveries: 1,
                retrying_deliveries: 3,
                oldest_pending_age_secs: 120,
                total_attempts: 11,
            },
        }
    }

    fn metric<'a>(metrics: &'a [Value], name: &str) -> &'a Value {
        metrics
            .iter()
            .find(|metric| metric["name"] == name)
            .unwrap_or_else(|| panic!("missing metric {name}"))
    }

    #[test]
    fn payload_contains_cumulative_sums_and_gauges() {
        let exporter = OtlpExporter::new(
            &telemetry_config("http://127.0.0.1:4318"),
            reqwest::Client::new(),
        )
        .unwrap();
        let payload = exporter.payload(&sample_snapshot());

        let resource = &payload["resourceMetrics"][0]["resource"]["attributes"][0];
        assert_eq!(resource["key"], "service.name");
        assert_eq!(resource["value"]["stringValue"], "octra-test");
        assert_eq!(
            payload["resourceMetrics"][0]["scopeMetrics"][0]["scope"]["name"],
            "octra"
        );

        let metrics = payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap();

        let requests = metric(metrics, "octra.api.requests");
        assert_eq!(requests["sum"]["aggregationTemporality"], 2);
        assert_eq!(requests["sum"]["isMonotonic"], true);
        assert_eq!(requests["sum"]["dataPoints"][0]["asInt"], "7");
        assert!(
            requests["sum"]["dataPoints"][0]["startTimeUnixNano"]
                .as_str()
                .unwrap()
                .parse::<u128>()
                .is_ok()
        );
        assert!(
            requests["sum"]["dataPoints"][0]["timeUnixNano"]
                .as_str()
                .unwrap()
                .parse::<u128>()
                .is_ok()
        );

        let responses = metric(metrics, "octra.api.responses");
        let classes: Vec<_> = responses["sum"]["dataPoints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|point| {
                (
                    point["attributes"][0]["value"]["stringValue"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                    point["asInt"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            classes,
            vec![
                ("2xx".to_string(), "4".to_string()),
                ("3xx".to_string(), "1".to_string()),
                ("4xx".to_string(), "1".to_string()),
                ("5xx".to_string(), "1".to_string()),
            ]
        );

        let latency = metric(metrics, "octra.api.request.latency");
        assert_eq!(latency["sum"]["dataPoints"][0]["asDouble"], 1.5);
        assert_eq!(
            metric(metrics, "octra.api.request.count")["sum"]["dataPoints"][0]["asInt"],
            "7"
        );

        let pending = metric(metrics, "octra.event.pending");
        assert!(pending.get("sum").is_none());
        assert_eq!(pending["gauge"]["dataPoints"][0]["asInt"], "5");
        assert_eq!(
            metric(metrics, "octra.event.deliveries.retrying")["gauge"]["dataPoints"][0]["asInt"],
            "3"
        );
        assert_eq!(
            metric(metrics, "octra.event.oldest_pending_age_seconds")["gauge"]["dataPoints"][0]["asInt"],
            "120"
        );
        assert_eq!(
            metric(metrics, "octra.event.attempts")["gauge"]["dataPoints"][0]["asInt"],
            "11"
        );
    }

    #[test]
    fn endpoint_join_handles_trailing_slash() {
        let exporter = OtlpExporter::new(
            &telemetry_config("http://127.0.0.1:4318/"),
            reqwest::Client::new(),
        )
        .unwrap();
        assert_eq!(
            exporter.metrics_endpoint(),
            "http://127.0.0.1:4318/v1/metrics"
        );

        let exporter = OtlpExporter::new(
            &telemetry_config("http://127.0.0.1:4318/otlp/"),
            reqwest::Client::new(),
        )
        .unwrap();
        assert_eq!(
            exporter.metrics_endpoint(),
            "http://127.0.0.1:4318/otlp/v1/metrics"
        );
    }

    #[test]
    fn rejects_missing_or_invalid_otlp_config() {
        let mut config = telemetry_config("http://127.0.0.1:4318");
        config.otlp = None;
        assert!(matches!(
            OtlpExporter::new(&config, reqwest::Client::new()),
            Err(TelemetryError::MissingOtlp)
        ));

        let config = telemetry_config("ftp://127.0.0.1:4318");
        assert!(matches!(
            OtlpExporter::new(&config, reqwest::Client::new()),
            Err(TelemetryError::InvalidEndpoint { .. })
        ));
    }

    #[tokio::test]
    async fn export_posts_otlp_json_and_succeeds_on_2xx() {
        let (endpoint, receiver) = spawn_collector(200, "{}");
        let exporter = OtlpExporter::new(&telemetry_config(&endpoint), test_client()).unwrap();

        exporter.export(&sample_snapshot()).await.unwrap();

        let captured = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(captured.request_line, "POST /v1/metrics HTTP/1.1");
        assert!(
            captured
                .headers
                .to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        assert!(
            captured
                .headers
                .to_ascii_lowercase()
                .contains("x-test: yes")
        );

        let payload: Value = serde_json::from_slice(&captured.body).unwrap();
        assert_eq!(
            payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["name"],
            "octra.api.requests"
        );
    }

    #[tokio::test]
    async fn export_reports_non_2xx_responses() {
        let (endpoint, receiver) = spawn_collector(503, "collector unavailable");
        let exporter = OtlpExporter::new(&telemetry_config(&endpoint), test_client()).unwrap();

        let error = exporter.export(&sample_snapshot()).await.unwrap_err();
        receiver.recv_timeout(Duration::from_secs(5)).unwrap();

        match error {
            TelemetryError::Status { status, body } => {
                assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
                assert!(body.contains("collector unavailable"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    fn spawn_collector(
        status: u16,
        response_body: &'static str,
    ) -> (String, mpsc::Receiver<CapturedRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();

            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4_096];
            let header_end = loop {
                let read = stream.read(&mut chunk).unwrap();
                assert!(read > 0, "connection closed before request headers");
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position + 4;
                }
            };

            let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if !name.eq_ignore_ascii_case("content-length") {
                        return None;
                    }
                    value.trim().parse::<usize>().ok()
                })
                .unwrap_or(0);
            while buffer.len() < header_end + content_length {
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }

            let request_line = headers.lines().next().unwrap_or_default().to_string();
            let body_end = (header_end + content_length).min(buffer.len());
            sender
                .send(CapturedRequest {
                    request_line,
                    headers,
                    body: buffer[header_end..body_end].to_vec(),
                })
                .unwrap();

            let response = format!(
                "HTTP/1.1 {status} Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}"), receiver)
    }
}
