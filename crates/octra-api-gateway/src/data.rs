//! Data plane: catch-all reverse proxy.
//!
//! Every request is matched against the compiled runtime snapshot. Plugins run
//! in ordered phases while keeping the existing Octra plugin names
//! (`correlation-id`, `cors`, `rate-limiting`, `header-rewrite`, `circuit-breaker`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use http_body::Body as _;
use octra_api_core::{Route, Service};
use octra_api_plugin_runtime::PluginChain;
use octra_api_plugin_types::{
    AuthBasic, AuthJwt, AuthKey, BodyContext, ConsumerInfo, ErrorContext, GatewayState, LogContext,
    PluginError, RequestBodyContext, RequestContext, ResponseBodyContext, ResponseContext,
    UpstreamRequestContext,
};
use octra_data_plane::{
    HEADER_REQUEST_ID, apply_forwarded_headers, error_response, new_request_id,
    path_has_dot_segments, sanitize_request_headers, sanitize_response_headers,
};
use octra_runtime::balancer::Endpoint;
use octra_runtime::{
    AuthSnapshot, CompiledRoute, CompiledService, MatchInput, ServiceLimits, normalize_host,
    strip_matched_prefix,
};
use octra_storage::NewRequestLog;
use serde_json::json;

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

    // Reject dot segments before route matching. `url::Url::set_path`
    // normalizes them, which could forward a path outside the matched route
    // prefix and bypass that route's plugins.
    if path_has_dot_segments(&path) {
        let response = invalid_path();
        let elapsed = started.elapsed();
        state.metrics.record(response.status().as_u16(), elapsed, 0);
        tracing::warn!(
            %method,
            %path,
            status = response.status().as_u16(),
            latency_ms = elapsed.as_millis() as u64,
            request_id = %request_id,
            "api gateway: invalid path"
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
                error: Some("invalid_path".to_string()),
            },
        );
        publish_request_event(
            &state,
            json!({
                "request_id": request_id,
                "method": method.as_str(),
                "path": path,
                "status": response.status().as_u16(),
                "latency_ms": elapsed.as_millis() as u64,
                "route_id": null,
                "service_id": null,
                "target_url": null,
                "consumer_id": null,
                "error": "invalid_path",
            }),
        );
        return with_request_id(response, &request_id);
    }

    let client_ip = state.client_ip.resolve(peer, &parts.headers);

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
        publish_request_event(
            &state,
            json!({
                "request_id": request_id,
                "method": method.as_str(),
                "path": path,
                "status": response.status().as_u16(),
                "latency_ms": elapsed.as_millis() as u64,
                "route_id": null,
                "service_id": null,
                "target_url": null,
                "consumer_id": null,
                "error": null,
            }),
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
        uri_captures: matched.uri_captures,
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
        .pre_auth_chain(Some(&route_id), service_id.as_deref(), scheme);

    let subject = LogSubject {
        request_id: &request_id,
        method: &method,
        path: &path,
        route,
        service,
        consumer: None,
    };

    let max_body = state.config.gateway.max_body_bytes;
    let mut body = Some(body);
    let mut early_body = None;
    if pre_chain.needs_request_body() {
        let raw = body.take().expect("request body is available");
        match axum::body::to_bytes(raw, max_body).await {
            Ok(bytes) => {
                auth_state.request_body = Some(bytes.clone());
                early_body = Some(bytes);
            }
            Err(_) => {
                let response = payload_too_large();
                let elapsed = started.elapsed();
                state.metrics.record(response.status().as_u16(), elapsed, 0);
                return complete(
                    &state,
                    &pre_chain,
                    &PluginChain::default(),
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
    }

    let mut request_cx = RequestContext {
        request_id: &request_id,
        state: &mut auth_state,
        matched_path: &matched_path,
        method: &parts.method,
        uri: &mut parts.uri,
        headers: &mut parts.headers,
        client_ip: Some(client_ip),
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
            let request_headers = parts.headers.clone();
            let response = match apply_response_plugins(
                ResponsePluginContext {
                    chains: &[pre_chain.as_ref()],
                    request_id: &request_id,
                    auth_state: &mut auth_state,
                    request_headers: &request_headers,
                    consumer_id: None,
                    route,
                    service,
                    target_url: "",
                },
                response,
            )
            .await
            {
                Ok(response) => response,
                Err(error) => return failed_plugin(&state, &subject, started, error),
            };
            return complete(
                &state,
                &pre_chain,
                &state
                    .plugins
                    .pre_auth_chain(Some(&route_id), service_id.as_deref(), scheme),
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
    let (group_ids, groups) = auth_state
        .consumer
        .as_ref()
        .map(|consumer| (consumer.group_ids.as_slice(), consumer.groups.as_slice()))
        .unwrap_or_default();
    let post_chain = state.plugins.post_auth_chain(
        Some(&route_id),
        service_id.as_deref(),
        consumer_id.as_deref(),
        group_ids,
        groups,
        scheme,
    );

    let credential_id = auth_state.credential.as_ref().map(|c| c.id.clone());
    let mut post_cx = RequestContext {
        request_id: &request_id,
        state: &mut auth_state,
        matched_path: &matched_path,
        method: &parts.method,
        uri: &mut parts.uri,
        headers: &mut parts.headers,
        client_ip: Some(client_ip),
        consumer_id: consumer_id.as_deref(),
        credential_id: credential_id.as_deref(),
        route,
        service,
    };
    match post_chain.on_request(&mut post_cx).await {
        Ok(None) => {}
        Ok(Some(response)) => {
            let elapsed = started.elapsed();
            state.metrics.record(response.status().as_u16(), elapsed, 0);
            let request_headers = parts.headers.clone();
            let response = match apply_response_plugins(
                ResponsePluginContext {
                    chains: &[pre_chain.as_ref(), post_chain.as_ref()],
                    request_id: &request_id,
                    auth_state: &mut auth_state,
                    request_headers: &request_headers,
                    consumer_id: consumer_id.as_deref(),
                    route,
                    service,
                    target_url: "",
                },
                response,
            )
            .await
            {
                Ok(response) => response,
                Err(error) => return failed_plugin(&state, &subject, started, error),
            };
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

    let hash_path = parts
        .uri
        .path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());
    let hash_input = octra_runtime::HashInput {
        headers: &parts.headers,
        path: &hash_path,
        client_ip: Some(client_ip),
        consumer_id: consumer_id.as_deref(),
        uri_captures: &dispatch.uri_captures,
    };
    let mut attempts = endpoint.attempts(Some(&hash_input));
    // Set (or reset) the sticky cookie when the request has no cookie or the
    // presented value does not name an eligible target.
    let sticky_cookie = endpoint.sticky_cookie().filter(|cookie| {
        let signature_name = format!("{}.sig", cookie.name);
        request_cookie(&parts.headers, &cookie.name)
            .and_then(|target| {
                request_cookie(&parts.headers, &signature_name)
                    .filter(|signature| {
                        verify_sticky_signature(&state.sticky_secret, target, signature)
                    })
                    .map(|_| target)
            })
            .is_none_or(|target| !attempts.iter().any(|endpoint| endpoint.id == target))
    });
    let retry_limit = 1 + usize::try_from(
        dispatch
            .service
            .as_ref()
            .map(|service| service.limits.retries)
            .unwrap_or(0),
    )
    .unwrap_or(usize::MAX);
    attempts.truncate(attempts.len().min(retry_limit));
    // Without request buffering the body cannot be replayed, so keep one
    // attempt. Non-idempotent methods are buffered and may retry only a
    // connection failure, before any request bytes reached the upstream.
    if !dispatch.route.route.request_buffering {
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
    let upstream_host = endpoint.host_header().map(str::to_string);
    let context = ForwardContext {
        client_addr: Some(peer),
        host: host.as_deref(),
        upstream_host: upstream_host.as_deref(),
        scheme: forwarded_scheme(&request_headers),
        request_id: &request_id,
    };

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

    let request_body_plugins = dispatch.route.route.request_buffering
        && (pre_chain.needs_request_body() || post_chain.needs_request_body());
    if !dispatch.route.route.request_buffering
        && (pre_chain.needs_request_body() || post_chain.needs_request_body())
    {
        tracing::warn!(
            route = %dispatch.route.route.id,
            "route disables request buffering; body-mutating plugins are skipped"
        );
    }
    let payload = if let Some(bytes) = early_body {
        RequestBody::Buffered(bytes)
    } else if attempts.len() > 1 || request_body_plugins {
        let raw = body.take().expect("request body is available");
        match axum::body::to_bytes(raw, max_body).await {
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
        RequestBody::Stream(Body::new(http_body_util::Limited::new(
            body.take().expect("request body is available"),
            max_body,
        )))
    };

    let payload = if request_body_plugins {
        match payload {
            RequestBody::Buffered(bytes) => {
                let bytes = {
                    let mut body_cx = RequestBodyContext {
                        request_id: &request_id,
                        state: &mut auth_state,
                        method: &parts.method,
                        uri: &mut parts.uri,
                        headers: &mut parts.headers,
                        consumer_id: consumer_id.as_deref(),
                        route,
                        service,
                    };
                    let bytes = match pre_chain.on_request_buffer(&mut body_cx, bytes).await {
                        Ok(bytes) => bytes,
                        Err(error) => return failed_plugin(&state, &subject, started, error),
                    };
                    match post_chain.on_request_buffer(&mut body_cx, bytes).await {
                        Ok(bytes) => bytes,
                        Err(error) => return failed_plugin(&state, &subject, started, error),
                    }
                };
                if let Ok(value) = HeaderValue::from_str(&bytes.len().to_string()) {
                    parts.headers.insert(header::CONTENT_LENGTH, value);
                }
                RequestBody::Buffered(bytes)
            }
            stream => stream,
        }
    } else {
        payload
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

            if let Some(cookie) = &sticky_cookie
                && let Some(target) = response_parts.extensions.get::<UpstreamTarget>()
            {
                if let Ok(value) = HeaderValue::from_str(&format!(
                    "{}={}; Path={}; HttpOnly; SameSite=Lax",
                    cookie.name, target.id, cookie.path
                )) {
                    response_parts.headers.append(header::SET_COOKIE, value);
                }
                if let Ok(value) = HeaderValue::from_str(&format!(
                    "{}.sig={}; Path={}; HttpOnly; SameSite=Lax",
                    cookie.name,
                    sticky_signature(&state.sticky_secret, &target.id),
                    cookie.path
                )) {
                    response_parts.headers.append(header::SET_COOKIE, value);
                }
            }

            let response_body_plugins = dispatch.route.route.response_buffering
                && (pre_chain.needs_response_buffer() || post_chain.needs_response_buffer());
            let response_body_stream = dispatch.route.route.response_buffering
                && !response_body_plugins
                && (pre_chain.streams_response_body() || post_chain.streams_response_body());
            if !dispatch.route.route.response_buffering
                && (pre_chain.needs_response_body()
                    || post_chain.needs_response_body()
                    || pre_chain.streams_response_body()
                    || post_chain.streams_response_body())
            {
                tracing::warn!(
                    route = %dispatch.route.route.id,
                    "route disables response buffering; response-body plugins are skipped"
                );
            }
            let response_body = if response_body_plugins {
                match axum::body::to_bytes(response_body, max_body).await {
                    Ok(bytes) => {
                        let bytes = {
                            let mut body_cx = ResponseBodyContext {
                                request_id: &request_id,
                                state: &mut auth_state,
                                status: response_parts.status,
                                headers: &response_parts.headers,
                                consumer_id: consumer_id.as_deref(),
                                route,
                                service,
                                target_url: &target,
                            };
                            let bytes =
                                match pre_chain.on_response_buffer(&mut body_cx, bytes).await {
                                    Ok(bytes) => bytes,
                                    Err(error) => {
                                        return failed_plugin(&state, &subject, started, error);
                                    }
                                };
                            match post_chain.on_response_buffer(&mut body_cx, bytes).await {
                                Ok(bytes) => bytes,
                                Err(error) => {
                                    return failed_plugin(&state, &subject, started, error);
                                }
                            }
                        };
                        response_parts.headers.remove(header::CONTENT_LENGTH);
                        if response_parts.status != StatusCode::NO_CONTENT
                            && response_parts.status != StatusCode::NOT_MODIFIED
                            && let Ok(value) = HeaderValue::from_str(&bytes.len().to_string())
                        {
                            response_parts.headers.insert(header::CONTENT_LENGTH, value);
                        }
                        Body::from(bytes)
                    }
                    Err(_) => {
                        return failed_plugin(
                            &state,
                            &subject,
                            started,
                            PluginError::fail_closed("response body exceeds gateway limit"),
                        );
                    }
                }
            } else if response_body_stream
                && response_parts.status != StatusCode::NO_CONTENT
                && response_parts.status != StatusCode::NOT_MODIFIED
            {
                response_parts.headers.remove(header::CONTENT_LENGTH);
                streaming_response_body(
                    response_body,
                    auth_state.clone(),
                    Arc::clone(&pre_chain),
                    Arc::clone(&post_chain),
                    route.clone(),
                    service.clone(),
                    request_id.clone(),
                    target.clone(),
                    max_body,
                )
            } else {
                response_body
            };

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
    GatewayState {
        consumers: auth
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
                        group_ids: consumer.group_ids,
                        groups: consumer.groups,
                    },
                )
            })
            .collect(),
        consumer_by_username: auth.consumer_by_username,
        consumer_by_custom_id: auth.consumer_by_custom_id,
        keyauth: auth
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
            .collect(),
        basicauth: auth
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
            .collect(),
        jwt: auth
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
            .collect(),
        ..GatewayState::default()
    }
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
        tags: Vec::new(),
        created_at: 0,
        updated_at: 0,
    }
}

#[derive(Clone)]
struct Dispatch {
    route: CompiledRoute,
    service: Option<CompiledService>,
    matched_path: String,
    uri_captures: Vec<(String, String)>,
}

struct ForwardContext<'a> {
    client_addr: Option<SocketAddr>,
    host: Option<&'a str>,
    upstream_host: Option<&'a str>,
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

struct ResponsePluginContext<'a> {
    chains: &'a [&'a PluginChain],
    request_id: &'a str,
    auth_state: &'a mut GatewayState,
    request_headers: &'a HeaderMap,
    consumer_id: Option<&'a str>,
    route: &'a Route,
    service: &'a Service,
    target_url: &'a str,
}

/// Runs response phases for a short-circuited response, mirroring the response
/// pipeline used for upstream responses.
async fn apply_response_plugins(
    cx: ResponsePluginContext<'_>,
    response: Response,
) -> std::result::Result<Response, PluginError> {
    let ResponsePluginContext {
        chains,
        request_id,
        auth_state,
        request_headers,
        consumer_id,
        route,
        service,
        target_url,
    } = cx;
    let mut response = response;
    for chain in chains {
        let (mut parts, body) = response.into_parts();
        let mut cx = ResponseContext {
            request_id,
            state: auth_state,
            request_headers,
            status: &mut parts.status,
            headers: &mut parts.headers,
            consumer_id,
            route,
            service,
            target_url,
        };
        match chain.on_response(&mut cx).await? {
            None => response = Response::from_parts(parts, body),
            Some(replacement) => return Ok(replacement),
        }
    }
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
fn streaming_response_body(
    body: Body,
    state: GatewayState,
    pre_chain: Arc<PluginChain>,
    post_chain: Arc<PluginChain>,
    route: Route,
    service: Service,
    request_id: String,
    target_url: String,
    max_body: usize,
) -> Body {
    let upstream = body.into_data_stream();
    let stream = futures_util::stream::unfold(
        (
            upstream, state, pre_chain, post_chain, route, service, request_id, target_url, 0usize,
        ),
        move |(
            mut upstream,
            mut state,
            pre_chain,
            post_chain,
            route,
            service,
            request_id,
            target_url,
            mut total,
        )| async move {
            match upstream.next().await {
                Some(Ok(chunk)) => {
                    total = total.saturating_add(chunk.len());
                    if total > max_body {
                        let error = std::io::Error::other(
                            "response body exceeds gateway limit while streaming",
                        );
                        return Some((
                            Err(error),
                            (
                                upstream, state, pre_chain, post_chain, route, service, request_id,
                                target_url, total,
                            ),
                        ));
                    }
                    let mut cx = BodyContext {
                        request_id: &request_id,
                        state: &mut state,
                        route: &route,
                        service: &service,
                        target_url: &target_url,
                    };
                    let chunk: Bytes = match pre_chain.on_response_body(&mut cx, chunk).await {
                        Ok(chunk) => match post_chain.on_response_body(&mut cx, chunk).await {
                            Ok(chunk) => chunk,
                            Err(error) => {
                                tracing::warn!(%request_id, %error, "response stream plugin failed");
                                return Some((
                                    Err(std::io::Error::other(error.to_string())),
                                    (
                                        upstream, state, pre_chain, post_chain, route, service,
                                        request_id, target_url, total,
                                    ),
                                ));
                            }
                        },
                        Err(error) => {
                            tracing::warn!(%request_id, %error, "response stream plugin failed");
                            return Some((
                                Err(std::io::Error::other(error.to_string())),
                                (
                                    upstream, state, pre_chain, post_chain, route, service,
                                    request_id, target_url, total,
                                ),
                            ));
                        }
                    };
                    Some((
                        Ok(chunk),
                        (
                            upstream, state, pre_chain, post_chain, route, service, request_id,
                            target_url, total,
                        ),
                    ))
                }
                Some(Err(error)) => Some((
                    Err(std::io::Error::other(error.to_string())),
                    (
                        upstream, state, pre_chain, post_chain, route, service, request_id,
                        target_url, total,
                    ),
                )),
                None => None,
            }
        },
    );
    Body::from_stream(stream)
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

    publish_request_event(
        state,
        json!({
            "request_id": request_id,
            "method": method.as_str(),
            "path": path,
            "status": status,
            "latency_ms": finish.latency.as_millis() as u64,
            "route_id": route.id.clone(),
            "service_id": (!service.id.is_empty()).then(|| service.id.clone()),
            "target_url": finish.target.clone(),
            "consumer_id": consumer.clone(),
            "error": finish.error.clone(),
        }),
    );

    with_request_id(response, request_id)
}

fn publish_request_event(state: &ApiDataState, payload: serde_json::Value) {
    if let Some(events) = &state.events {
        events.publish("api.request.completed", "api-gateway", payload);
    }
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

fn invalid_path() -> Response {
    json_error(
        StatusCode::BAD_REQUEST,
        "invalid_path",
        "path must not contain dot segments",
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
        .map(|target| target.url.clone())
        .unwrap_or_default()
}

/// Reads one cookie value from the request `Cookie` header(s).
fn sticky_signature(secret: &[u8; 32], target: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key");
    mac.update(target.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn verify_sticky_signature(secret: &[u8; 32], target: &str, signature: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let Ok(signature) = hex::decode(signature) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key");
    mac.update(target.as_bytes());
    mac.verify_slice(&signature).is_ok()
}

fn request_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|header| {
            header.split(';').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key.trim() == name).then(|| value.trim())
            })
        })
}

#[derive(Clone)]
struct UpstreamTarget {
    id: String,
    url: String,
}

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
    let non_idempotent = !is_idempotent(method.as_str());
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
        let host_header = if dispatch.route.route.preserve_host {
            context
                .host
                .and_then(|host| HeaderValue::from_str(host).ok())
        } else if let Some(upstream_host) = context.upstream_host {
            HeaderValue::from_str(upstream_host).ok()
        } else {
            HeaderValue::from_str(&endpoint_authority(&endpoint.url)).ok()
        };
        if let Some(value) = host_header {
            forward_headers.insert(header::HOST, value);
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

        let attempt_started = Instant::now();
        // The guard covers connect plus wait-for-headers. For streaming
        // responses the body keeps flowing after the guard is dropped, so the
        // in-flight count is an approximation of upstream load.
        let _in_flight = endpoint.begin_request();

        match builder.send().await {
            Ok(response) => {
                endpoint.report_latency(attempt_started.elapsed());
                let status = response.status();
                endpoint.report_status(status.as_u16());
                let mut response_headers = response.headers().clone();
                sanitize_response_headers(&mut response_headers);
                response_headers.remove(header::CONTENT_LENGTH);
                response_headers.remove(header::TRANSFER_ENCODING);

                let mut out = Response::new(Body::from_stream(response.bytes_stream()));
                *out.status_mut() = status;
                *out.headers_mut() = response_headers;
                out.extensions_mut().insert(UpstreamTarget {
                    id: endpoint.id.clone(),
                    url: endpoint.url.clone(),
                });
                return Ok(out);
            }
            Err(err) => {
                endpoint.report_latency(attempt_started.elapsed());
                tracing::debug!(
                    target = %endpoint.url,
                    attempt = index + 1,
                    error = %err,
                    "api gateway upstream attempt failed"
                );
                if err.is_timeout() {
                    endpoint.report_timeout();
                } else {
                    endpoint.report_tcp_failure();
                }
                last_error = Some(classify_reqwest_error(&err));
                if non_idempotent && !err.is_connect() {
                    break;
                }
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
