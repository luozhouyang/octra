//! Data plane: catch-all reverse proxy.
//!
//! Every request that reaches this router is matched against the compiled
//! runtime snapshot. The request body streams to the selected upstream target
//! and the upstream response streams back to the client.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use chrono::Utc;
use http_body::Body as _;
use octra_data_plane::{
    HEADER_REQUEST_ID, apply_forwarded_headers, error_response, new_request_id,
    sanitize_request_headers, sanitize_response_headers,
};
use octra_gateway_runtime::{
    CompiledRoute, CompiledService, CredentialEntry, MatchInput, RuntimeSnapshot, TargetPool,
    normalize_host, strip_matched_prefix,
};

use crate::ApiDataState;

const HEADER_API_KEY: &str = "x-api-key";

/// Catch-all proxy handler.
pub async fn proxy(
    State(state): State<ApiDataState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let headers = request.headers().clone();

    let started = Instant::now();
    let snapshot = state.runtime.snapshot().await;

    let request_id = headers
        .get(HEADER_REQUEST_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(new_request_id);

    let matched = snapshot
        .matcher
        .resolve(&MatchInput {
            host: host.as_deref(),
            path: &path,
            method: method.as_str(),
            headers: &headers,
        })
        .map(|(route, service)| Dispatch {
            route: route.clone(),
            service: service.clone(),
        });

    let Some(dispatch) = matched else {
        let response = not_found();
        let elapsed = started.elapsed();
        state.metrics.record(response.status().as_u16(), elapsed, 0);
        tracing::info!(
            %method,
            %path,
            status = response.status().as_u16(),
            latency_ms = elapsed.as_millis() as u64,
            request_id = %request_id,
            "api gateway: no route"
        );
        return with_request_id(response, &request_id);
    };

    let route_id = &dispatch.route.route.id;
    let service_id = &dispatch.service.service.id;

    let consumer = match authenticate(&snapshot, &headers) {
        Some(entry) => Some(entry.consumer_id.clone()),
        None if dispatch.service.limits.require_auth => {
            let response = unauthorized();
            let elapsed = started.elapsed();
            state.metrics.record(response.status().as_u16(), elapsed, 0);
            tracing::info!(
                %method,
                %path,
                status = response.status().as_u16(),
                route = %route_id,
                service = %service_id,
                request_id = %request_id,
                "api gateway: unauthorized"
            );
            return with_request_id(response, &request_id);
        }
        None => None,
    };

    let Some(pool) = snapshot.pool_for_service(service_id.as_str()).cloned() else {
        let response = bad_gateway("no upstream configured for service");
        let elapsed = started.elapsed();
        state.metrics.record_upstream_error();
        state.metrics.record(response.status().as_u16(), elapsed, 0);
        tracing::warn!(
            %method,
            %path,
            route = %route_id,
            service = %service_id,
            request_id = %request_id,
            "api gateway: service has no upstream"
        );
        return with_request_id(response, &request_id);
    };

    let context = ForwardContext {
        client_addr: Some(peer),
        host: host.as_deref(),
        scheme: forwarded_scheme(&headers),
        request_id: &request_id,
    };

    let outcome = forward(&state, &pool, &dispatch, request, context).await;
    let elapsed = started.elapsed();

    match outcome {
        Ok(response) => {
            let status = response.status().as_u16();
            state.metrics.record(status, elapsed, 0);
            tracing::info!(
                %method,
                %path,
                status,
                latency_ms = elapsed.as_millis() as u64,
                route = %route_id,
                service = %service_id,
                target = %response_target(&response),
                consumer = consumer.as_deref().unwrap_or("-"),
                request_id = %request_id,
                "api gateway request"
            );
            with_request_id(response, &request_id)
        }
        Err(failure) => {
            state.metrics.record_upstream_error();
            let response = failure.response();
            state
                .metrics
                .record(response.status().as_u16(), elapsed, failure.retries);
            tracing::warn!(
                %method,
                %path,
                status = response.status().as_u16(),
                latency_ms = elapsed.as_millis() as u64,
                route = %route_id,
                service = %service_id,
                target = %failure.target,
                attempts = failure.retries + 1,
                consumer = consumer.as_deref().unwrap_or("-"),
                request_id = %request_id,
                error = %failure.message,
                "api gateway upstream failed"
            );
            with_request_id(response, &request_id)
        }
    }
}

#[derive(Clone)]
struct Dispatch {
    route: CompiledRoute,
    service: CompiledService,
}

struct ForwardContext<'a> {
    client_addr: Option<SocketAddr>,
    host: Option<&'a str>,
    scheme: &'a str,
    request_id: &'a str,
}

fn with_request_id(mut response: Response, request_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response
            .headers_mut()
            .insert(header::HeaderName::from_static(HEADER_REQUEST_ID), value);
    }
    response
}

fn json_error(status: StatusCode, code: &'static str, message: &str) -> Response {
    error_response(status, code, message)
}

fn not_found() -> Response {
    json_error(StatusCode::NOT_FOUND, "no_route", "no matching route")
}

fn unauthorized() -> Response {
    let mut response = json_error(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "a valid API key is required",
    );
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

fn bad_gateway(message: &str) -> Response {
    json_error(StatusCode::BAD_GATEWAY, "upstream_unavailable", message)
}

/// Resolves an API key from `Authorization: Bearer` or `X-API-Key`.
fn authenticate(snapshot: &RuntimeSnapshot, headers: &HeaderMap) -> Option<CredentialEntry> {
    let candidate = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("bearer")
                .then(|| token.trim())
                .filter(|token| !token.is_empty())
        })
        .or_else(|| {
            headers
                .get(HEADER_API_KEY)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|key| !key.is_empty())
        })?;

    snapshot.authenticate(candidate, Utc::now()).cloned()
}

fn forwarded_scheme(headers: &HeaderMap) -> &'static str {
    const TLS_HINTS: [&str; 3] = ["x-forwarded-proto", "x-forwarded-scheme", "x-forwarded-ssl"];
    for name in TLS_HINTS {
        let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) else {
            continue;
        };
        let value = value.split(',').next().unwrap_or_default().trim();
        if value.eq_ignore_ascii_case("https") || value.eq_ignore_ascii_case("on") {
            return "https";
        }
    }
    "http"
}

struct UpstreamFailure {
    status: StatusCode,
    code: &'static str,
    message: String,
    target: String,
    retries: u64,
}

impl UpstreamFailure {
    fn response(&self) -> Response {
        json_error(self.status, self.code, &self.message)
    }
}

fn response_target(response: &Response) -> String {
    response
        .extensions()
        .get::<UpstreamTarget>()
        .map(|target| target.0.clone())
        .unwrap_or_default()
}

/// Marker carried on proxied responses so logs can name the target.
#[derive(Clone)]
struct UpstreamTarget(String);

type Chunk = Result<bytes::Bytes, axum::Error>;

async fn forward(
    state: &ApiDataState,
    pool: &Arc<TargetPool>,
    dispatch: &Dispatch,
    request: Request,
    context: ForwardContext<'_>,
) -> Result<Response, UpstreamFailure> {
    let limits = dispatch.service.limits;

    let ordered = pool.ordered_attempts();
    let max_attempts = ordered
        .len()
        .min(1 + usize::try_from(limits.retries).unwrap_or(usize::MAX));
    let attempts: Vec<_> = ordered.into_iter().take(max_attempts).collect();

    if attempts.is_empty() {
        return Err(UpstreamFailure {
            status: StatusCode::BAD_GATEWAY,
            code: "upstream_unavailable",
            message: "no eligible upstream target".into(),
            target: String::new(),
            retries: 0,
        });
    }

    let method = request.method().clone();
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string());
    let base_headers = request.headers().clone();
    let body_stream = ChunkStream {
        body: request.into_body(),
        finished: false,
    };

    // Only idempotent requests are retried, and only across distinct targets.
    let attempts = if is_idempotent(method.as_str()) {
        attempts
    } else {
        attempts.into_iter().take(1).collect()
    };
    let total_attempts = attempts.len();
    let mut last_error: Option<String> = None;
    let mut body_stream: Option<ChunkStream> = Some(body_stream);

    for (index, endpoint) in attempts.iter().enumerate() {
        let is_last = index + 1 == total_attempts;

        let mut forward_headers = base_headers.clone();
        sanitize_request_headers(&mut forward_headers);
        // The client sends the gateway's own host; the upstream must see the
        // target authority instead.
        if let Ok(authority) = HeaderValue::from_str(&endpoint_authority(&endpoint.url)) {
            forward_headers.insert(header::HOST, authority);
        }
        apply_forwarded_headers(
            &mut forward_headers,
            context.client_addr,
            context.host,
            context.scheme,
            context.request_id,
        );

        let upstream_url = match build_upstream_url(
            endpoint.url.as_str(),
            path_and_query.as_deref(),
            &dispatch.route,
        ) {
            Ok(url) => url,
            Err(err) => {
                last_error = Some(format!("invalid upstream URL: {err}"));
                continue;
            }
        };

        // The client body can only be streamed once, so earlier attempts send
        // an empty body and the final attempt carries the real one.
        let payload = if is_last {
            body_stream
                .take()
                .map_or(Chunks::Empty, ChunkStream::chunk_stream)
        } else {
            Chunks::Empty
        };
        let upstream_body = reqwest::Body::wrap_stream(payload);

        let request_method =
            reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
        let builder = state
            .client
            .request(request_method, upstream_url)
            .timeout(limits.request_timeout)
            .headers(forward_headers)
            .body(upstream_body);

        match builder.send().await {
            Ok(response) => {
                let status = response.status();
                let mut response_headers = response.headers().clone();
                sanitize_response_headers(&mut response_headers);
                response_headers.remove(header::CONTENT_LENGTH);
                response_headers.remove(header::TRANSFER_ENCODING);

                let mut out = Response::new(Body::from_stream(response.bytes_stream()));
                *out.status_mut() = status;
                *out.headers_mut() = response_headers;
                out.extensions_mut()
                    .insert(UpstreamTarget(endpoint.url.clone()));
                return Ok(out);
            }
            Err(err) => {
                tracing::debug!(
                    target = %endpoint.url,
                    attempt = index + 1,
                    error = %err,
                    "api gateway upstream attempt failed"
                );
                last_error = Some(classify_reqwest_error(&err));
            }
        }
    }

    let message = last_error.unwrap_or_else(|| "upstream request failed".to_string());
    let (status, code) = if message.starts_with("timeout") {
        (StatusCode::GATEWAY_TIMEOUT, "upstream_timeout")
    } else {
        (StatusCode::BAD_GATEWAY, "upstream_error")
    };

    Err(UpstreamFailure {
        status,
        code,
        message,
        target: attempts
            .last()
            .map(|endpoint| endpoint.url.clone())
            .unwrap_or_default(),
        retries: total_attempts.saturating_sub(1) as u64,
    })
}

/// Streams request chunks for the single attempt that carries the body.
struct ChunkStream {
    body: Body,
    finished: bool,
}

impl ChunkStream {
    fn chunk_stream(self) -> Chunks {
        Chunks::Body(self)
    }
}

enum Chunks {
    Body(ChunkStream),
    Empty,
}

impl futures_util::Stream for Chunks {
    type Item = Chunk;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            let frame = match &mut *self {
                Self::Empty => return std::task::Poll::Ready(None),
                Self::Body(stream) if stream.finished => return std::task::Poll::Ready(None),
                Self::Body(stream) => std::pin::Pin::new(&mut stream.body).poll_frame(cx),
            };

            match frame {
                std::task::Poll::Ready(Some(Ok(frame))) => {
                    // Trailers are not forwarded, so keep polling.
                    if let Ok(data) = frame.into_data() {
                        return std::task::Poll::Ready(Some(Ok(data)));
                    }
                }
                std::task::Poll::Ready(Some(Err(err))) => {
                    if let Self::Body(stream) = &mut *self {
                        stream.finished = true;
                    }
                    return std::task::Poll::Ready(Some(Err(err)));
                }
                std::task::Poll::Ready(None) => {
                    if let Self::Body(stream) = &mut *self {
                        stream.finished = true;
                    }
                    return std::task::Poll::Ready(None);
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

fn classify_reqwest_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        format!("timeout: {err}")
    } else if err.is_connect() {
        format!("connect: {err}")
    } else {
        format!("request: {err}")
    }
}

fn is_idempotent(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "OPTIONS" | "TRACE" | "PUT" | "DELETE"
    )
}

/// Joins the target base URL with the request path. When the route strips its
/// prefix, the matched prefix is removed first; the query is preserved.
fn build_upstream_url(
    base: &str,
    path_and_query: Option<&str>,
    route: &CompiledRoute,
) -> Result<url::Url, url::ParseError> {
    let mut url = url::Url::parse(base)?;
    let base_path = url.path().trim_end_matches('/').to_string();

    let (path, query) = match path_and_query {
        Some(value) => match value.split_once('?') {
            Some((path, query)) => (path.to_string(), Some(query.to_string())),
            None => (value.to_string(), None),
        },
        None => ("/".to_string(), None),
    };

    let rewritten = if route.route.strip_prefix {
        strip_matched_prefix(&path, &route.route.path_prefix)
    } else {
        path
    };

    let joined = format!("{base_path}{rewritten}");
    let joined = if joined.is_empty() {
        "/".to_string()
    } else if joined.starts_with('/') {
        joined
    } else {
        format!("/{joined}")
    };

    url.set_path(&joined);
    url.set_query(query.as_deref());
    url.set_fragment(None);
    Ok(url)
}

/// The authority used for the upstream `Host` header.
pub fn endpoint_authority(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            let host = parsed.host_str()?.to_string();
            Some(match parsed.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        })
        .unwrap_or_else(|| normalize_host(url))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn compiled_route(prefix: &str, strip: bool) -> CompiledRoute {
        use octra_gateway_runtime::RouteMatcher;
        let now = Utc::now();
        let service = octra_api_core::Service {
            id: "s".into(),
            name: "s".into(),
            protocol: "http".into(),
            description: String::new(),
            enabled: true,
            connect_timeout_ms: 100,
            request_timeout_ms: 1000,
            retries: 0,
            require_auth: false,
            created_at: now,
            updated_at: now,
        };
        let route = octra_api_core::Route {
            id: "r".into(),
            service_id: "s".into(),
            host: None,
            path_prefix: prefix.into(),
            methods: vec![],
            headers: BTreeMap::new(),
            protocol: "http".into(),
            priority: 0,
            strip_prefix: strip,
            enabled: true,
            created_at: now,
            updated_at: now,
        };
        let matcher = RouteMatcher::compile(&[service], &[route]);
        matcher.route(0).unwrap().clone()
    }

    #[test]
    fn upstream_url_joins_and_rewrites() {
        let route = compiled_route("/api", true);
        let url = build_upstream_url(
            "http://127.0.0.1:9000/base",
            Some("/api/v1/users?limit=2"),
            &route,
        )
        .unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:9000/base/v1/users?limit=2");

        let url = build_upstream_url("http://127.0.0.1:9000", Some("/api"), &route).unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:9000/");

        let url = build_upstream_url("http://127.0.0.1:9000", Some("/api?x=1"), &route).unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:9000/?x=1");
    }

    #[test]
    fn upstream_url_keeps_prefix_when_not_stripping() {
        let route = compiled_route("/api", false);
        let url = build_upstream_url("http://up.test", Some("/api/x"), &route).unwrap();
        assert_eq!(url.as_str(), "http://up.test/api/x");
    }

    #[test]
    fn authority_includes_port() {
        assert_eq!(
            endpoint_authority("http://127.0.0.1:9000"),
            "127.0.0.1:9000"
        );
        assert_eq!(endpoint_authority("https://example.test"), "example.test");
    }
}
