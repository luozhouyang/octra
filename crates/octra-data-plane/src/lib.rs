//! Shared helpers for the unified Octra data plane.
//!
//! The API, LLM, and MCP gateways all forward HTTP traffic. This crate keeps
//! the common request hygiene, forwarding headers, error shape, body limits,
//! and Server-Sent Events decoding in one place.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use octra_config::OctraConfig;
use reqwest::Client;

pub mod sse;

pub const HEADER_REQUEST_ID: &str = "x-request-id";

/// Hop-by-hop headers that must never be forwarded by a proxy.
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "proxy-connection",
];

/// Builds the shared upstream HTTP client. A failure to initialize TLS falls
/// back to the default client so the daemon can still start and report errors.
pub fn http_client(config: &OctraConfig) -> Client {
    Client::builder()
        .connect_timeout(Duration::from_secs(
            config.gateway.connect_timeout_secs.max(1),
        ))
        .pool_idle_timeout(Duration::from_secs(90))
        // The data plane is a proxy: upstream redirects are returned to the
        // caller (or treated as upstream errors) rather than followed with
        // the same credentials. This also prevents an SSRF allow-list from
        // being bypassed by a redirect to a metadata endpoint.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "failed to build the configured HTTP client; using defaults");
            Client::new()
        })
}

pub fn new_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Client address forwarded to an in-process upstream.
#[derive(Debug, Clone)]
pub struct ClientIp(pub Option<IpAddr>);

/// Request identity forwarded to an in-process upstream.
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

/// Removes hop-by-hop headers, including headers named by `Connection`.
pub fn sanitize_request_headers(headers: &mut HeaderMap) {
    sanitize_hop_by_hop(headers);
}

pub fn sanitize_response_headers(headers: &mut HeaderMap) {
    sanitize_hop_by_hop(headers);
}

fn sanitize_hop_by_hop(headers: &mut HeaderMap) {
    let connection_tokens: Vec<String> = headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect();

    for name in HOP_BY_HOP {
        headers.remove(name);
    }

    for token in connection_tokens {
        if let Ok(name) = HeaderName::from_bytes(token.as_bytes()) {
            headers.remove(name);
        }
    }
}

/// Adds reverse-proxy forwarding headers for the upstream request.
pub fn apply_forwarded_headers(
    headers: &mut HeaderMap,
    client_addr: Option<SocketAddr>,
    original_host: Option<&str>,
    forwarded_proto: &str,
    request_id: &str,
) {
    if let Some(addr) = client_addr {
        let client_ip = addr.ip().to_string();
        let value = match headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
        {
            Some(existing) => format!("{existing}, {client_ip}"),
            None => client_ip,
        };
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(HeaderName::from_static("x-forwarded-for"), value);
        }
    }

    if let Some(host) = original_host
        && let Ok(value) = HeaderValue::from_str(host)
    {
        headers.insert(HeaderName::from_static("x-forwarded-host"), value);
    }

    if let Ok(value) = HeaderValue::from_str(forwarded_proto) {
        headers.insert(HeaderName::from_static("x-forwarded-proto"), value);
    }

    if let Ok(value) = HeaderValue::from_str(request_id) {
        headers.insert(HeaderName::from_static(HEADER_REQUEST_ID), value);
    }
}

/// Returns `true` when a request path contains a dot segment.
///
/// Both raw `.`/`..` segments and percent-encoded forms are detected. This
/// matters for proxy forwarding because `url::Url::set_path` normalizes dot
/// segments, which could otherwise escape the matched route prefix and bypass
/// its plugins. The whole path is decoded before splitting, so encoded
/// separators such as `%2F..` cannot smuggle a dot segment past the check.
/// Backslashes are treated as separators because the URL parser normalizes
/// them to `/` for special schemes such as `http` and `https`.
pub fn path_has_dot_segments(path: &str) -> bool {
    // Decode percent escapes before splitting so `%2F..` or `%5C..` cannot
    // smuggle a dot segment past the check for an upstream that decodes the
    // path again.
    let decoded = percent_decode_for_path_check(path);
    decoded
        .split(['/', '\\'])
        .any(|segment| segment == "." || segment == "..")
}

fn percent_decode_for_path_check(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            out.push(high * 16 + low);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Reads a request body while enforcing the configured size limit.
pub async fn body_bytes_limited(body: Body, limit: usize) -> Result<bytes::Bytes, axum::Error> {
    axum::body::to_bytes(body, limit).await
}

/// Consistent JSON error envelope for the data plane.
pub fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "code": code,
            "message": message,
        }
    });
    (status, axum::Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_raw_and_encoded_dot_segments() {
        for path in [
            "/public/../admin",
            "/public/%2e%2e/admin",
            "/public/%2E%2E/admin",
            "/public/.%2e/admin",
            "/public/%2e./admin",
            "/%2e/admin",
            "/public/..",
            "/public/%2F..",
            "/public/%5C..",
            "..",
        ] {
            assert!(path_has_dot_segments(path), "expected rejection for {path}");
        }
    }

    #[test]
    fn detects_backslash_dot_segments() {
        for path in ["/public\\..\\admin", "/public/..\\admin"] {
            assert!(path_has_dot_segments(path), "expected rejection for {path}");
        }
    }

    #[test]
    fn accepts_harmless_paths_and_encoded_slashes() {
        for path in ["/a.b", "/api/v1/users", "/v1.0/x", "/public/a%2Fb", "/", ""] {
            assert!(
                !path_has_dot_segments(path),
                "expected acceptance for {path}"
            );
        }
    }

    #[test]
    fn strips_hop_by_hop_and_connection_listed_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", HeaderValue::from_static("keep-alive, x-drop"));
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert("x-drop", HeaderValue::from_static("1"));
        headers.insert("x-keep", HeaderValue::from_static("1"));

        sanitize_request_headers(&mut headers);

        assert!(headers.get("connection").is_none());
        assert!(headers.get("keep-alive").is_none());
        assert!(headers.get("x-drop").is_none());
        assert_eq!(headers.get("x-keep").unwrap(), "1");
    }

    #[test]
    fn appends_forwarded_for_and_sets_request_id() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("10.0.0.1"));
        let addr: SocketAddr = "127.0.0.1:4242".parse().unwrap();

        apply_forwarded_headers(
            &mut headers,
            Some(addr),
            Some("example.test"),
            "http",
            "req-1",
        );

        assert_eq!(
            headers.get("x-forwarded-for").unwrap(),
            "10.0.0.1, 127.0.0.1"
        );
        assert_eq!(headers.get("x-forwarded-host").unwrap(), "example.test");
        assert_eq!(headers.get("x-forwarded-proto").unwrap(), "http");
        assert_eq!(headers.get(HEADER_REQUEST_ID).unwrap(), "req-1");
    }
}
