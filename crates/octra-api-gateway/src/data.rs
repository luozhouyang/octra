//! Data plane: catch-all reverse proxy.
//!
//! Every request that reaches this router is matched against the compiled
//! runtime snapshot. The request body streams to the selected upstream target
//! and the upstream response streams back to the client.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use chrono::Utc;
use http_body::Body as _;
use http_body_util::BodyExt as _;
use octra_api_core::{Route, Service};
use octra_data_plane::{
    HEADER_REQUEST_ID, apply_forwarded_headers, error_response, new_request_id,
    sanitize_request_headers, sanitize_response_headers,
};
use octra_gateway_runtime::balancer::Endpoint;
use octra_gateway_runtime::{
    CompiledRoute, CompiledService, CredentialEntry, MatchInput, RuntimeSnapshot, normalize_host,
    strip_matched_prefix,
};
use octra_plugin_api::{
    BodyContext, ErrorContext, LogContext, PluginError, RequestContext, ResponseContext,
    UpstreamRequestContext,
};
use octra_plugin_runtime::PluginChain;

use crate::ApiDataState;

const HEADER_API_KEY: &str = "x-api-key";

/// Catch-all proxy handler.
pub async fn proxy(
    State(state): State<ApiDataState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let started = Instant::now();
    let (mut parts, body) = request.into_parts();
    let method = parts.method.clone();
    let path = parts.uri.path().to_string();
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let request_id = parts
        .headers
        .get(HEADER_REQUEST_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(new_request_id);

    let snapshot = state.runtime.snapshot().await;

    let matched = snapshot
        .matcher
        .resolve(&MatchInput {
            host: host.as_deref(),
            path: &path,
            method: method.as_str(),
            headers: &parts.headers,
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
    let route = &dispatch.route.route;
    let service = &dispatch.service.service;

    // Authentication runs before the plugin chain so consumer-scoped plugins
    // can be selected, but rejection is deferred until `on_request` has run:
    // CORS preflight must be answered without credentials.
    let credential = authenticate(&snapshot, &parts.headers);
    let consumer_id = credential.as_ref().map(|entry| entry.consumer_id.clone());
    let chain = state
        .plugins
        .chain_for(route_id, service_id, consumer_id.as_deref());
    let subject = LogSubject {
        request_id: &request_id,
        method: &method,
        path: &path,
        route,
        service,
        consumer: consumer_id.as_deref(),
    };

    let mut request_cx = RequestContext {
        request_id: &request_id,
        method: &parts.method,
        uri: &mut parts.uri,
        headers: &mut parts.headers,
        client_ip: Some(peer.ip()),
        consumer_id: consumer_id.as_deref(),
        credential_id: credential.as_ref().map(|entry| entry.id.as_str()),
        route,
        service,
    };
    match chain.on_request(&mut request_cx).await {
        Ok(None) => {}
        Ok(Some(response)) => {
            let elapsed = started.elapsed();
            state.metrics.record(response.status().as_u16(), elapsed, 0);
            return complete(
                &chain,
                &subject,
                Finish {
                    latency: elapsed,
                    target: None,
                    error: None,
                },
                response,
            )
            .await;
        }
        Err(error) => {
            return failed_plugin(&state, &subject, started, error);
        }
    }

    if dispatch.service.limits.require_auth && credential.is_none() {
        let response = unauthorized();
        let elapsed = started.elapsed();
        state.metrics.record(response.status().as_u16(), elapsed, 0);
        return complete(
            &chain,
            &subject,
            Finish {
                latency: elapsed,
                target: None,
                error: None,
            },
            response,
        )
        .await;
    }

    let Some(pool) = snapshot.pool_for_service(service_id.as_str()).cloned() else {
        let response = bad_gateway("no upstream configured for service");
        let elapsed = started.elapsed();
        state.metrics.record_upstream_error();
        state.metrics.record(response.status().as_u16(), elapsed, 0);
        return complete(
            &chain,
            &subject,
            Finish {
                latency: elapsed,
                target: None,
                error: Some("service has no upstream".to_string()),
            },
            response,
        )
        .await;
    };

    let mut attempts = pool.ordered_attempts();
    let retry_limit = 1 + usize::try_from(dispatch.service.limits.retries).unwrap_or(usize::MAX);
    attempts.truncate(attempts.len().min(retry_limit));
    // Only idempotent requests are retried, and only across distinct targets.
    if !is_idempotent(method.as_str()) {
        attempts.truncate(1);
    }

    let Some(target_url) = attempts.first().map(|endpoint| endpoint.url.clone()) else {
        let response = bad_gateway("no eligible upstream target");
        let elapsed = started.elapsed();
        state.metrics.record_upstream_error();
        state.metrics.record(response.status().as_u16(), elapsed, 0);
        return complete(
            &chain,
            &subject,
            Finish {
                latency: elapsed,
                target: None,
                error: Some("no eligible upstream target".to_string()),
            },
            response,
        )
        .await;
    };

    let mut upstream_cx = UpstreamRequestContext {
        request_id: &request_id,
        method: &parts.method,
        uri: &mut parts.uri,
        headers: &mut parts.headers,
        consumer_id: consumer_id.as_deref(),
        route,
        service,
        target_url: &target_url,
    };
    match chain.on_upstream_request(&mut upstream_cx).await {
        Ok(None) => {}
        Ok(Some(response)) => {
            let elapsed = started.elapsed();
            state.metrics.record(response.status().as_u16(), elapsed, 0);
            return complete(
                &chain,
                &subject,
                Finish {
                    latency: elapsed,
                    target: Some(target_url),
                    error: None,
                },
                response,
            )
            .await;
        }
        Err(error) => {
            return failed_plugin(&state, &subject, started, error);
        }
    }

    let request_headers = parts.headers.clone();
    let original_uri = parts.uri.clone();
    let context = ForwardContext {
        client_addr: Some(peer),
        host: host.as_deref(),
        scheme: forwarded_scheme(&request_headers),
        request_id: &request_id,
    };

    let max_body = state.config.gateway.max_body_bytes;
    let declared_length = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if declared_length.is_some_and(|length| length > max_body) {
        let response = payload_too_large();
        let elapsed = started.elapsed();
        state.metrics.record(response.status().as_u16(), elapsed, 0);
        return complete(
            &chain,
            &subject,
            Finish {
                latency: elapsed,
                target: None,
                error: None,
            },
            response,
        )
        .await;
    }

    // Retries need the body more than once, so those requests are buffered
    // within the configured limit. Single-attempt requests stay streaming and
    // are bounded by a limited body.
    let payload = if attempts.len() > 1 {
        match axum::body::to_bytes(body, max_body).await {
            Ok(bytes) => RequestBody::Buffered(bytes),
            Err(_) => {
                let response = payload_too_large();
                let elapsed = started.elapsed();
                state.metrics.record(response.status().as_u16(), elapsed, 0);
                return complete(
                    &chain,
                    &subject,
                    Finish {
                        latency: elapsed,
                        target: None,
                        error: None,
                    },
                    response,
                )
                .await;
            }
        }
    } else {
        RequestBody::Stream(Body::new(http_body_util::Limited::new(body, max_body)))
    };

    let outcome = forward(&state, attempts, &dispatch, parts, payload, context).await;
    let elapsed = started.elapsed();

    match outcome {
        Ok(response) => {
            let status = response.status().as_u16();
            state.metrics.record(status, elapsed, 0);

            let (mut response_parts, response_body) = response.into_parts();
            let target = response_target(&response_parts);
            let response_action = {
                let mut response_cx = ResponseContext {
                    request_id: &request_id,
                    request_headers: &request_headers,
                    status: &mut response_parts.status,
                    headers: &mut response_parts.headers,
                    consumer_id: consumer_id.as_deref(),
                    route,
                    service,
                    target_url: &target,
                };
                chain.on_response(&mut response_cx).await
            };
            match response_action {
                Ok(None) => {}
                Ok(Some(replacement)) => {
                    let elapsed = started.elapsed();
                    state
                        .metrics
                        .record(replacement.status().as_u16(), elapsed, 0);
                    return complete(
                        &chain,
                        &subject,
                        Finish {
                            latency: elapsed,
                            target: Some(target),
                            error: None,
                        },
                        replacement,
                    )
                    .await;
                }
                Err(error) => {
                    return failed_plugin(&state, &subject, started, error);
                }
            }

            let filtered = filter_response_body(
                chain.clone(),
                OwnedBodyContext {
                    request_id: request_id.clone(),
                    route: route.clone(),
                    service: service.clone(),
                    target_url: target.clone(),
                },
                response_body,
            );
            let response = Response::from_parts(response_parts, filtered);
            complete(
                &chain,
                &subject,
                Finish {
                    latency: elapsed,
                    target: Some(target),
                    error: None,
                },
                response,
            )
            .await
        }
        Err(failure) => {
            state.metrics.record_upstream_error();
            let elapsed = started.elapsed();
            let fallback = failure.response();
            state
                .metrics
                .record(fallback.status().as_u16(), elapsed, failure.retries);

            let mut error_cx = ErrorContext {
                request_id: &request_id,
                method: &method,
                uri: &original_uri,
                headers: &request_headers,
                consumer_id: consumer_id.as_deref(),
                route,
                service,
                target_url: &failure.target,
                error: &failure.message,
            };
            let response = match chain.on_error(&mut error_cx).await {
                Ok(Some(response)) => response,
                Ok(None) => fallback,
                Err(error) => plugin_failure(error),
            };
            complete(
                &chain,
                &subject,
                Finish {
                    latency: elapsed,
                    target: Some(failure.target),
                    error: Some(failure.message),
                },
                response,
            )
            .await
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

struct Finish {
    latency: Duration,
    target: Option<String>,
    error: Option<String>,
}

struct LogSubject<'a> {
    request_id: &'a str,
    method: &'a Method,
    path: &'a str,
    route: &'a Route,
    service: &'a Service,
    consumer: Option<&'a str>,
}

struct OwnedBodyContext {
    request_id: String,
    route: Route,
    service: Service,
    target_url: String,
}

async fn complete(
    chain: &PluginChain,
    subject: &LogSubject<'_>,
    finish: Finish,
    response: Response,
) -> Response {
    let LogSubject {
        request_id,
        method,
        path,
        route,
        service,
        consumer,
    } = *subject;
    let status = response.status().as_u16();
    let log_cx = LogContext {
        request_id,
        method,
        path,
        status,
        latency: finish.latency,
        consumer_id: consumer,
        route,
        service,
        target_url: finish.target.as_deref(),
        error: finish.error.as_deref(),
    };
    chain.on_log(&log_cx).await;

    if let Some(error) = finish.error.as_deref() {
        tracing::warn!(
            %method,
            %path,
            status,
            latency_ms = finish.latency.as_millis() as u64,
            route = %route.id,
            service = %service.id,
            target = finish.target.as_deref().unwrap_or("-"),
            consumer = consumer.unwrap_or("-"),
            request_id = %request_id,
            error,
            "api gateway upstream failed"
        );
    } else {
        tracing::info!(
            %method,
            %path,
            status,
            latency_ms = finish.latency.as_millis() as u64,
            route = %route.id,
            service = %service.id,
            target = finish.target.as_deref().unwrap_or("-"),
            consumer = consumer.unwrap_or("-"),
            request_id = %request_id,
            "api gateway request"
        );
    }

    with_request_id(response, request_id)
}

fn plugin_failure(error: PluginError) -> Response {
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "plugin_error",
        &error.message,
    )
}

fn failed_plugin(
    state: &ApiDataState,
    subject: &LogSubject<'_>,
    started: Instant,
    error: PluginError,
) -> Response {
    let LogSubject {
        request_id,
        method,
        path,
        route,
        service,
        consumer: _,
    } = *subject;
    let response = plugin_failure(error);
    let elapsed = started.elapsed();
    state.metrics.record(response.status().as_u16(), elapsed, 0);
    tracing::warn!(
        %method,
        %path,
        status = response.status().as_u16(),
        latency_ms = elapsed.as_millis() as u64,
        route = %route.id,
        service = %service.id,
        request_id = %request_id,
        "api gateway plugin failed"
    );
    with_request_id(response, request_id)
}

fn filter_response_body(chain: Arc<PluginChain>, cx: OwnedBodyContext, body: Body) -> Body {
    let stream =
        futures_util::stream::unfold((body, chain, cx), |(mut body, chain, cx)| async move {
            loop {
                let frame = match body.frame().await {
                    Some(Ok(frame)) => frame,
                    Some(Err(error)) => {
                        tracing::warn!(%error, "api gateway: upstream response body failed");
                        return None;
                    }
                    None => return None,
                };
                let Ok(chunk) = frame.into_data() else {
                    continue;
                };
                let body_cx = BodyContext {
                    request_id: &cx.request_id,
                    route: &cx.route,
                    service: &cx.service,
                    target_url: &cx.target_url,
                };
                let next: Result<bytes::Bytes, axum::Error> =
                    match chain.on_response_body(&body_cx, chunk).await {
                        Ok(next) => Ok(next),
                        Err(error) => {
                            tracing::warn!(
                                plugin_error = %error,
                                "api gateway: response body plugin failed"
                            );
                            Err(axum::Error::new(error))
                        }
                    };
                return Some((next, (body, chain, cx)));
            }
        });
    Body::from_stream(stream)
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

fn payload_too_large() -> Response {
    json_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
        "request body exceeds gateway.max_body_bytes",
    )
}

enum RequestBody {
    Buffered(bytes::Bytes),
    Stream(Body),
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

fn response_target(parts: &axum::http::response::Parts) -> String {
    parts
        .extensions
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
    attempts: Vec<Arc<Endpoint>>,
    dispatch: &Dispatch,
    parts: axum::http::request::Parts,
    mut payload: RequestBody,
    context: ForwardContext<'_>,
) -> Result<Response, UpstreamFailure> {
    let limits = dispatch.service.limits;

    if attempts.is_empty() {
        return Err(UpstreamFailure {
            status: StatusCode::BAD_GATEWAY,
            code: "upstream_unavailable",
            message: "no eligible upstream target".into(),
            target: String::new(),
            retries: 0,
        });
    }

    let method = parts.method.clone();
    let path_and_query = parts.uri.path_and_query().map(|pq| pq.as_str().to_string());
    let base_headers = parts.headers.clone();

    let total_attempts = attempts.len();
    let mut last_error: Option<String> = None;

    // reqwest exposes `connect_timeout` only on the client builder. Services
    // that override the gateway-wide value get a dedicated client; all others
    // reuse the shared, pooled one.
    let default_connect_timeout = Duration::from_secs(state.config.gateway.connect_timeout_secs);
    let client = if limits.connect_timeout == default_connect_timeout {
        state.client.clone()
    } else {
        reqwest::Client::builder()
            .connect_timeout(limits.connect_timeout)
            .timeout(limits.request_timeout)
            .build()
            .unwrap_or_else(|_| state.client.clone())
    };

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

        let upstream_body = match &mut payload {
            RequestBody::Buffered(bytes) => reqwest::Body::from(bytes.clone()),
            RequestBody::Stream(_) if !is_last => reqwest::Body::from(Vec::new()),
            RequestBody::Stream(body) => {
                let body = std::mem::replace(body, Body::empty());
                reqwest::Body::wrap_stream(ChunkStream {
                    body,
                    finished: false,
                })
            }
        };

        let request_method =
            reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
        let builder = client
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

/// Streams request chunks for the attempt that carries the body.
struct ChunkStream {
    body: Body,
    finished: bool,
}

impl futures_util::Stream for ChunkStream {
    type Item = Chunk;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            if self.finished {
                return std::task::Poll::Ready(None);
            }
            let frame = std::pin::Pin::new(&mut self.body).poll_frame(cx);
            match frame {
                std::task::Poll::Ready(Some(Ok(frame))) => {
                    // Trailers are not forwarded, so keep polling.
                    if let Ok(data) = frame.into_data() {
                        return std::task::Poll::Ready(Some(Ok(data)));
                    }
                }
                std::task::Poll::Ready(Some(Err(err))) => {
                    self.finished = true;
                    return std::task::Poll::Ready(Some(Err(err)));
                }
                std::task::Poll::Ready(None) => {
                    self.finished = true;
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
