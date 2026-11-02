//! Data plane: catch-all reverse proxy.
//!
//! Every request is matched against the compiled runtime snapshot. Plugins run
//! in ordered phases while keeping the existing Octra plugin names
//! (`request-id`, `cors`, `rate-limit`, `header-rewrite`, `circuit-breaker`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use chrono::Utc;
use http_body::Body as _;
use http_body_util::BodyExt as _;
use octra_api_core::{Route, Service};
use octra_api_plugin_runtime::PluginChain;
use octra_api_plugin_types::{
    AuthBasic, AuthJwt, AuthKey, ConsumerInfo, ErrorContext, GatewayState, LogContext, PluginError,
    RequestContext, ResponseContext, UpstreamRequestContext,
};
use octra_data_plane::{
    HEADER_REQUEST_ID, apply_forwarded_headers, error_response, new_request_id,
    sanitize_request_headers, sanitize_response_headers,
};
use octra_runtime::balancer::Endpoint;
use octra_runtime::{
    AuthSnapshot, CompiledRoute, CompiledService, MatchInput, RuntimeSnapshot, ServiceLimits,
    normalize_host, strip_matched_prefix,
};
use octra_storage::NewRequestLog;

use crate::ApiDataState;

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

    let scheme = forwarded_scheme(&parts.headers);
    let input = MatchInput {
        scheme,
        host: host.as_deref(),
        path: &path,
        method: method.as_str(),
        headers: &parts.headers,
    };
    let Some(matched) = snapshot.matcher.match_route(&input) else {
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
        persist_request_log(
            &state,
            NewRequestLog {
                request_id: request_id.clone(),
                method: method.as_str().to_string(),
                path: path.clone(),
                status: response.status().as_u16(),
                latency_ms: elapsed.as_millis() as u64,
                route_id: None,
                service_id: None,
                target_url: None,
                consumer_id: None,
                error: None,
            },
        );
        return with_request_id(response, &request_id);
    };
    let Some(compiled_route) = snapshot.matcher.route(matched.route_index).cloned() else {
        return with_request_id(not_found(), &request_id);
    };
    let dispatch = Dispatch {
        route: compiled_route,
        service: matched
            .service_index
            .and_then(|index| snapshot.matcher.service(index))
            .cloned(),
        matched_path: matched.matched_path,
    };

    let placeholder = placeholder_service();
    let route_id = dispatch.route.route.id.clone();
    let service_id = dispatch
        .service
        .as_ref()
        .map(|service| service.service.id.clone());
    let route = &dispatch.route.route;
    let service = dispatch
        .service
        .as_ref()
        .map(|service| &service.service)
        .unwrap_or(&placeholder);
    let matched_path = dispatch.matched_path.clone();

    let mut auth_state = build_gateway_state(snapshot.auth_snapshot());
    let pre_chain = state
        .plugins
        .pre_auth_chain(Some(&route_id), service_id.as_deref());

    let subject = LogSubject {
        request_id: &request_id,
        method: &method,
        path: &path,
        route,
        service,
        consumer: None,
    };

    let mut request_cx = RequestContext {
        request_id: &request_id,
        state: &mut auth_state,
        matched_path: &matched_path,
        method: &parts.method,
        uri: &mut parts.uri,
        headers: &mut parts.headers,
        client_ip: Some(peer.ip()),
        consumer_id: None,
        credential_id: None,
        route,
        service,
    };
    match pre_chain.on_request(&mut request_cx).await {
        Ok(None) => {}
        Ok(Some(response)) => {
            let elapsed = started.elapsed();
            state.metrics.record(response.status().as_u16(), elapsed, 0);
            return complete(
                &state,
                &pre_chain,
                &state
                    .plugins
                    .pre_auth_chain(Some(&route_id), service_id.as_deref()),
                &subject,
                &mut auth_state,
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

    let consumer_id = auth_state
        .consumer
        .as_ref()
        .map(|consumer| consumer.id.clone());

    let post_chain = consumer_id
        .as_deref()
        .map(|consumer_id| {
            state
                .plugins
                .post_auth_chain(Some(&route_id), service_id.as_deref(), consumer_id)
        })
        .unwrap_or_else(|| Arc::new(PluginChain::default()));

    if let Some(consumer_id) = consumer_id.as_deref() {
        let credential_id = auth_state.credential.as_ref().map(|c| c.id.clone());
        let mut post_cx = RequestContext {
            request_id: &request_id,
            state: &mut auth_state,
            matched_path: &matched_path,
            method: &parts.method,
            uri: &mut parts.uri,
            headers: &mut parts.headers,
            client_ip: Some(peer.ip()),
            consumer_id: Some(consumer_id),
            credential_id: credential_id.as_deref(),
            route,
            service,
        };
        match post_chain.on_request(&mut post_cx).await {
            Ok(None) => {}
            Ok(Some(response)) => {
                let elapsed = started.elapsed();
                state.metrics.record(response.status().as_u16(), elapsed, 0);
                return complete(
                    &state,
                    &pre_chain,
                    &post_chain,
                    &subject,
                    &mut auth_state,
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
    }

    inject_consumer_headers(&mut parts.headers, &auth_state);

    let consumer_id = auth_state
        .consumer
        .as_ref()
        .map(|consumer| consumer.id.clone());
    let subject = LogSubject {
        consumer: consumer_id.clone(),
        ..subject
    };

    let Some(service_id) = service_id.as_deref() else {
        let response = bad_gateway("route has no service");
        let elapsed = started.elapsed();
        state.metrics.record_upstream_error();
        state.metrics.record(response.status().as_u16(), elapsed, 0);
        return complete(
            &state,
            &pre_chain,
            &post_chain,
            &subject,
            &mut auth_state,
            Finish {
                latency: elapsed,
                target: None,
                error: Some("route has no service".to_string()),
            },
            response,
        )
        .await;
    };
    let Some(endpoint) = snapshot.endpoint_for_service(service_id) else {
        let response = bad_gateway("no upstream configured for service");
        let elapsed = started.elapsed();
        state.metrics.record_upstream_error();
        state.metrics.record(response.status().as_u16(), elapsed, 0);
        return complete(
            &state,
            &pre_chain,
            &post_chain,
            &subject,
            &mut auth_state,
            Finish {
                latency: elapsed,
                target: None,
                error: Some("service has no upstream".to_string()),
            },
            response,
        )
        .await;
    };

    let mut attempts = endpoint.attempts();
    let retry_limit = 1 + usize::try_from(
        dispatch
            .service
            .as_ref()
            .map(|service| service.limits.retries)
            .unwrap_or(0),
    )
    .unwrap_or(usize::MAX);
    attempts.truncate(attempts.len().min(retry_limit));
    if !is_idempotent(method.as_str()) {
        attempts.truncate(1);
    }

    let Some(target_url) = attempts.first().map(|endpoint| endpoint.url.clone()) else {
        let response = bad_gateway("no eligible upstream target");
        let elapsed = started.elapsed();
        state.metrics.record_upstream_error();
        state.metrics.record(response.status().as_u16(), elapsed, 0);
        return complete(
            &state,
            &pre_chain,
            &post_chain,
            &subject,
            &mut auth_state,
            Finish {
                latency: elapsed,
                target: None,
                error: Some("no eligible upstream target".to_string()),
            },
            response,
        )
        .await;
    };

    if let Some(method) = auth_state.method_override.take() {
        parts.method = method;
    }

    let mut upstream_cx = UpstreamRequestContext {
        request_id: &request_id,
        state: &mut auth_state,
        method: &parts.method,
        uri: &mut parts.uri,
        headers: &mut parts.headers,
        consumer_id: consumer_id.as_deref(),
        route,
        service,
        target_url: &target_url,
    };
    for chain in [&pre_chain, &post_chain] {
        match chain.on_upstream_request(&mut upstream_cx).await {
            Ok(None) => {}
            Ok(Some(response)) => {
                let elapsed = started.elapsed();
                state.metrics.record(response.status().as_u16(), elapsed, 0);
                return complete(
                    &state,
                    &pre_chain,
                    &post_chain,
                    &subject,
                    &mut auth_state,
                    Finish {
                        latency: elapsed,
                        target: Some(target_url.clone()),
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
            &state,
            &pre_chain,
            &post_chain,
            &subject,
            &mut auth_state,
            Finish {
                latency: elapsed,
                target: None,
                error: None,
            },
            response,
        )
        .await;
    }

    let payload = if attempts.len() > 1 {
        match axum::body::to_bytes(body, max_body).await {
            Ok(bytes) => RequestBody::Buffered(bytes),
            Err(_) => {
                let response = payload_too_large();
                let elapsed = started.elapsed();
                state.metrics.record(response.status().as_u16(), elapsed, 0);
                return complete(
                    &state,
                    &pre_chain,
                    &post_chain,
                    &subject,
                    &mut auth_state,
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

    let outcome = forward(
        &state,
        attempts,
        &dispatch,
        parts,
        payload,
        context,
        &matched_path,
    )
    .await;
    let elapsed = started.elapsed();

    match outcome {
        Ok(response) => {
            let status = response.status().as_u16();
            state.metrics.record(status, elapsed, 0);

            let (mut response_parts, response_body) = response.into_parts();
            let target = response_target(&response_parts);
            for chain in [&pre_chain, &post_chain] {
                let mut response_cx = ResponseContext {
                    request_id: &request_id,
                    state: &mut auth_state,
                    request_headers: &request_headers,
                    status: &mut response_parts.status,
                    headers: &mut response_parts.headers,
                    consumer_id: consumer_id.as_deref(),
                    route,
                    service,
                    target_url: &target,
                };
                match chain.on_response(&mut response_cx).await {
                    Ok(None) => {}
                    Ok(Some(replacement)) => {
                        let elapsed = started.elapsed();
                        state
                            .metrics
                            .record(replacement.status().as_u16(), elapsed, 0);
                        return complete(
                            &state,
                            &pre_chain,
                            &post_chain,
                            &subject,
                            &mut auth_state,
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
            }

            let response = Response::from_parts(response_parts, response_body);
            complete(
                &state,
                &pre_chain,
                &post_chain,
                &subject,
                &mut auth_state,
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
                state: &mut auth_state,
                method: &method,
                uri: &original_uri,
                headers: &request_headers,
                consumer_id: consumer_id.as_deref(),
                route,
                service,
                target_url: &failure.target,
                error: &failure.message,
            };
            let mut response = fallback;
            for chain in [&pre_chain, &post_chain] {
                match chain.on_error(&mut error_cx).await {
                    Ok(Some(replacement)) => response = replacement,
                    Ok(None) => {}
                    Err(error) => response = plugin_failure(error),
                }
            }
            complete(
                &state,
                &pre_chain,
                &post_chain,
                &subject,
                &mut auth_state,
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

fn build_gateway_state(auth: AuthSnapshot) -> GatewayState {
    let mut state = GatewayState::default();
    state.consumers = auth
        .consumers
        .into_iter()
        .map(|(id, consumer)| {
            (
                id,
                ConsumerInfo {
                    id: consumer.id,
                    username: consumer.username,
                    custom_id: consumer.custom_id,
                    acl_groups: consumer.acl_groups,
                },
            )
        })
        .collect();
    state.consumer_by_username = auth.consumer_by_username;
    state.consumer_by_custom_id = auth.consumer_by_custom_id;
    state.keyauth = auth
        .keyauth
        .into_iter()
        .map(|(key, entry)| {
            (
                key,
                AuthKey {
                    credential_id: entry.id,
                    consumer_id: entry.consumer_id,
                },
            )
        })
        .collect();
    state.basicauth = auth
        .basicauth
        .into_iter()
        .map(|(username, entry)| {
            (
                username,
                AuthBasic {
                    credential_id: entry.id,
                    consumer_id: entry.consumer_id,
                    username: entry.username,
                    password: entry.password,
                },
            )
        })
        .collect();
    state.jwt = auth
        .jwt
        .into_iter()
        .map(|(key, entry)| {
            (
                key,
                AuthJwt {
                    credential_id: entry.id,
                    consumer_id: entry.consumer_id,
                    key: entry.key,
                    secret: entry.secret,
                    algorithm: entry.algorithm,
                    rsa_public_key: entry.rsa_public_key,
                },
            )
        })
        .collect();
    state
}

fn inject_consumer_headers(headers: &mut HeaderMap, state: &GatewayState) {
    let Some(consumer) = &state.consumer else {
        return;
    };
    insert_header(headers, "x-consumer-id", &consumer.id);
    if let Some(username) = &consumer.username {
        insert_header(headers, "x-consumer-username", username);
    }
    if let Some(custom_id) = &consumer.custom_id {
        insert_header(headers, "x-consumer-custom-id", custom_id);
    }
    if let Some(credential) = &state.credential {
        insert_header(headers, "x-credential-identifier", &credential.id);
    }
    if state.anonymous {
        insert_header(headers, "x-anonymous-consumer", "true");
    }
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::from_str(value)) {
        headers.insert(name, value);
    }
}

fn placeholder_service() -> Service {
    Service {
        id: String::new(),
        name: String::new(),
        protocol: "http".into(),
        host: "localhost".into(),
        port: 80,
        path: None,
        retries: 0,
        connect_timeout: 60_000,
        write_timeout: 60_000,
        read_timeout: 60_000,
        enabled: true,
        description: String::new(),
        created_at: 0,
        updated_at: 0,
    }
}

#[derive(Clone)]
struct Dispatch {
    route: CompiledRoute,
    service: Option<CompiledService>,
    matched_path: String,
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
    consumer: Option<String>,
}

async fn complete(
    state: &ApiDataState,
    pre_chain: &PluginChain,
    post_chain: &PluginChain,
    subject: &LogSubject<'_>,
    auth_state: &mut GatewayState,
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
    } = subject;
    let status = response.status().as_u16();
    let mut log_cx = LogContext {
        request_id,
        state: auth_state,
        method,
        path,
        status,
        latency: finish.latency,
        consumer_id: consumer.as_deref(),
        route,
        service,
        target_url: finish.target.as_deref(),
        error: finish.error.as_deref(),
    };
    pre_chain.on_log(&mut log_cx).await;
    post_chain.on_log(&mut log_cx).await;

    if let Some(error) = finish.error.as_deref() {
        tracing::warn!(
            %method,
            %path,
            status,
            latency_ms = finish.latency.as_millis() as u64,
            route = %route.id,
            service = %service.id,
            target = finish.target.as_deref().unwrap_or("-"),
            consumer = consumer.as_deref().unwrap_or("-"),
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
            consumer = consumer.as_deref().unwrap_or("-"),
            request_id = %request_id,
            "api gateway request"
        );
    }

    persist_request_log(
        state,
        NewRequestLog {
            request_id: request_id.to_string(),
            method: method.as_str().to_string(),
            path: path.to_string(),
            status,
            latency_ms: finish.latency.as_millis() as u64,
            route_id: Some(route.id.clone()),
            service_id: (!service.id.is_empty()).then(|| service.id.clone()),
            target_url: finish.target.clone(),
            consumer_id: consumer.clone(),
            error: finish.error.clone(),
        },
    );

    with_request_id(response, request_id)
}

fn persist_request_log(state: &ApiDataState, log: NewRequestLog) {
    let Some(storage) = state.storage.clone() else {
        return;
    };
    tokio::spawn(async move {
        if let Err(error) = storage.logs.insert_request_log(&log).await {
            tracing::warn!(%error, "api gateway: cannot persist request log");
        }
    });
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
    let response = plugin_failure(error);
    let elapsed = started.elapsed();
    state.metrics.record(response.status().as_u16(), elapsed, 0);
    tracing::warn!(
        %subject.method,
        %subject.path,
        status = response.status().as_u16(),
        latency_ms = elapsed.as_millis() as u64,
        route = %subject.route.id,
        service = %subject.service.id,
        request_id = %subject.request_id,
        "api gateway plugin failed"
    );
    with_request_id(response, subject.request_id)
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

fn forbidden() -> Response {
    json_error(
        StatusCode::FORBIDDEN,
        "forbidden",
        "consumer policy does not grant this route",
    )
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
    matched_path: &str,
) -> Result<Response, UpstreamFailure> {
    let limits = dispatch
        .service
        .as_ref()
        .map(|service| service.limits)
        .unwrap_or(ServiceLimits {
            connect_timeout: Duration::from_secs(state.config.gateway.connect_timeout_secs),
            request_timeout: Duration::from_secs(state.config.gateway.request_timeout_secs),
            retries: 0,
        });

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

        let service_path = dispatch
            .service
            .as_ref()
            .and_then(|service| service.service.path.as_deref());
        let upstream_url = match build_upstream_url(
            endpoint.url.as_str(),
            path_and_query.as_deref(),
            &dispatch.route,
            matched_path,
            service_path,
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

fn build_upstream_url(
    base: &str,
    path_and_query: Option<&str>,
    route: &CompiledRoute,
    matched_path: &str,
    service_path: Option<&str>,
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

    let rewritten = if route.route.strip_path && !matched_path.is_empty() {
        strip_matched_prefix(&path, matched_path)
    } else {
        path
    };

    let service_path = service_path.unwrap_or_default().trim_end_matches('/');
    let joined = format!("{base_path}{service_path}{rewritten}");
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
