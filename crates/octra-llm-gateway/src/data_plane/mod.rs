//! LLM data plane: protocol translation, routing, fallback and usage.

pub mod remote;
pub mod stream;
pub mod translate;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use octra_llm_core::{
    ChatRequest, ChatResponse, ContentPart, GatewayError, Protocol, Usage, estimate_tokens,
};
use octra_llm_plugin_runtime::{LlmPluginChain, LlmRequestOutcome};
use octra_llm_plugin_types::{
    LlmLogEvent, LlmProviderInfo, LlmRequestMeta, LlmRequestState, LlmResponseHeaders,
};
use octra_llm_protocol::decode_request;
use serde_json::Value;
use serde_json::json;
use tokio::time::timeout;

use crate::accounts::{self, AccountRuntime};
use crate::config::{ProviderConfig, is_local_url};
use crate::cost;
use crate::error::{LlmResult, chain_error, protocol_response};
use crate::keys::{self, KeyOutcome, presented_key};
use crate::routing::{RoutingIntent, Target, TargetSignal, rank_targets};
use crate::runtime::{LlmDataState, RuntimeSnapshot};
use crate::similarity;
use crate::upstream::Upstream;
use crate::usage::{self, UsageTracker};
use crate::vk_policy;

use remote::{Incoming, LlmResponse, RemoteBody, json_response};

/// Hard ceiling on upstream attempts per request (first try + fallbacks).
const MAX_ATTEMPTS: usize = 3;

pub fn router(state: LlmDataState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/models", get(list_models))
        .route("/v1beta/models", get(gemini_list_models))
        .route("/v1beta/models/{model_action}", post(gemini_model_action))
        .with_state(state)
}

async fn chat_completions(
    state: State<LlmDataState>,
    incoming: Incoming,
    body: RemoteBody,
) -> LlmResponse {
    LlmResponse(handle(state.0, incoming, body, Protocol::Openai, None, None).await)
}

async fn responses(
    state: State<LlmDataState>,
    incoming: Incoming,
    body: RemoteBody,
) -> LlmResponse {
    LlmResponse(
        handle(
            state.0,
            incoming,
            body,
            Protocol::OpenaiResponses,
            None,
            None,
        )
        .await,
    )
}

async fn messages(state: State<LlmDataState>, incoming: Incoming, body: RemoteBody) -> LlmResponse {
    LlmResponse(handle(state.0, incoming, body, Protocol::Anthropic, None, None).await)
}

async fn gemini_model_action(
    state: State<LlmDataState>,
    Path(model_action): Path<String>,
    incoming: Incoming,
    body: RemoteBody,
) -> LlmResponse {
    let Some((model, action)) = model_action.rsplit_once(':') else {
        let error = GatewayError::not_found("missing Gemini model action").with_code("not_found");
        return LlmResponse(protocol_response(Protocol::Gemini, &error));
    };
    let model = model.strip_prefix("models/").unwrap_or(model).to_string();
    match action {
        "generateContent" => LlmResponse(
            handle(
                state.0,
                incoming,
                body,
                Protocol::Gemini,
                Some(model),
                Some(false),
            )
            .await,
        ),
        "streamGenerateContent" => LlmResponse(
            handle(
                state.0,
                incoming,
                body,
                Protocol::Gemini,
                Some(model),
                Some(true),
            )
            .await,
        ),
        "countTokens" => LlmResponse(gemini_count_tokens(state.0, incoming, body, model).await),
        other => {
            let error = GatewayError::not_found(format!("unknown Gemini action '{other}'"));
            LlmResponse(protocol_response(Protocol::Gemini, &error))
        }
    }
}

async fn gemini_count_tokens(
    state: LlmDataState,
    incoming: Incoming,
    body: RemoteBody,
    model: String,
) -> Response {
    let snapshot = state.runtime.snapshot().await;
    let key = match authorize(&snapshot, &incoming).await {
        Ok(key) => key,
        Err(error) => return protocol_response(Protocol::Gemini, &error),
    };
    let consumer_id = key.as_ref().and_then(|key| key.consumer_id.as_deref());
    let virtual_key_id = key.as_ref().map(|key| key.id.as_str());
    if !snapshot.permits_model(consumer_id, &model) {
        let error =
            GatewayError::forbidden(format!("consumer is not allowed to use model '{model}'"))
                .with_code("model_forbidden");
        return protocol_response(Protocol::Gemini, &error);
    }
    if !snapshot.virtual_key_permits_model(virtual_key_id, &model) {
        let error =
            GatewayError::forbidden(format!("virtual key is not allowed to use model '{model}'"))
                .with_code("model_forbidden");
        return protocol_response(Protocol::Gemini, &error);
    }
    let body = match body.0 {
        Ok(body) => body,
        Err(response) => return response.0,
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return protocol_response(
                Protocol::Gemini,
                &GatewayError::invalid_request(format!("invalid JSON body: {error}")),
            );
        }
    };
    match decode_request(Protocol::Gemini, value) {
        Ok(ir) => json_response(json!({"totalTokens": estimate_tokens(&ir)})),
        Err(error) => protocol_response(Protocol::Gemini, &error.into_gateway()),
    }
}

async fn gemini_list_models(state: State<LlmDataState>, request: Incoming) -> LlmResponse {
    let state = state.0;
    let snapshot = state.runtime.snapshot().await;
    let (consumer_id, virtual_key_id) = match authorize(&snapshot, &request).await {
        Ok(Some(key)) => {
            touch(&state, &key.id).await;
            (key.consumer_id.clone(), Some(key.id))
        }
        Ok(None) => (None, None),
        Err(error) => return LlmResponse(protocol_response(Protocol::Gemini, &error)),
    };
    let models: Vec<Value> = snapshot
        .model_listing()
        .into_iter()
        .filter(|model| {
            snapshot.permits_model(consumer_id.as_deref(), &model.id)
                && snapshot.virtual_key_permits_model(virtual_key_id.as_deref(), &model.id)
        })
        .map(|model| {
            json!({
                "name": format!("models/{}", model.id),
                "displayName": model.description,
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent", "countTokens"],
            })
        })
        .collect();
    LlmResponse(json_response(json!({"models": models})))
}

async fn list_models(state: State<LlmDataState>, request: Incoming) -> LlmResponse {
    let state = state.0;
    let snapshot = state.runtime.snapshot().await;
    let (consumer_id, virtual_key_id) = match authorize(&snapshot, &request).await {
        Ok(Some(key)) => {
            touch(&state, &key.id).await;
            (key.consumer_id.clone(), Some(key.id))
        }
        Ok(None) => (None, None),
        Err(error) => {
            return LlmResponse(protocol_response(error_protocol(&request), &error));
        }
    };
    let model_api = wants_anthropic(&request);
    let data: Vec<Value> = snapshot
        .model_listing()
        .into_iter()
        .filter(|model| {
            snapshot.permits_model(consumer_id.as_deref(), &model.id)
                && snapshot.virtual_key_permits_model(virtual_key_id.as_deref(), &model.id)
        })
        .map(|model| {
            if model_api {
                json!({
                    "type": "model",
                    "id": model.id,
                    "display_name": model.description,
                    "created_at": DateTime::<Utc>::from_timestamp(model.created, 0)
                        .map(|value| value.to_rfc3339())
                        .unwrap_or_default(),
                })
            } else {
                json!({
                    "id": model.id,
                    "object": "model",
                    "created": model.created,
                    "owned_by": model.owned_by,
                })
            }
        })
        .collect();
    let body = if model_api {
        json!({
            "data": data,
            "has_more": false,
            "first_id": data.first().map(|d| d["id"].clone()).unwrap_or(Value::Null),
            "last_id": data.last().map(|d| d["id"].clone()).unwrap_or(Value::Null),
        })
    } else {
        json!({"object": "list", "data": data})
    };
    LlmResponse(json_response(body))
}

async fn count_tokens(
    state: State<LlmDataState>,
    request: Incoming,
    body: RemoteBody,
) -> LlmResponse {
    let state = state.0;
    let snapshot = state.runtime.snapshot().await;
    match authorize(&snapshot, &request).await {
        Ok(Some(key)) => touch(&state, &key.id).await,
        Ok(None) => {}
        Err(error) => {
            return LlmResponse(protocol_response(Protocol::Anthropic, &error));
        }
    }
    let body = match body.0 {
        Ok(body) => body,
        Err(response) => return response,
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            let error = GatewayError::invalid_request(format!("invalid JSON body: {error}"));
            return LlmResponse(protocol_response(Protocol::Anthropic, &error));
        }
    };
    let requested_model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let ir = match decode_request(Protocol::Anthropic, value) {
        Ok(ir) => ir,
        Err(error) => {
            return LlmResponse(protocol_response(
                Protocol::Anthropic,
                &error.into_gateway(),
            ));
        }
    };

    if let Some(tokens) = upstream_count(&state, &snapshot, &requested_model, &ir).await {
        return LlmResponse(json_response(json!({"input_tokens": tokens})));
    }
    LlmResponse(json_response(json!({"input_tokens": estimate_tokens(&ir)})))
}

/// Asks a local Anthropic-compatible upstream for the exact count. Token
/// counts never leave the machine for non-local providers.
async fn upstream_count(
    state: &LlmDataState,
    snapshot: &RuntimeSnapshot,
    requested_model: &str,
    ir: &ChatRequest,
) -> Option<u64> {
    let plan = snapshot.resolve(requested_model)?;
    let target = plan.attempts(0).first()?.clone();
    let provider = snapshot.provider(&target.provider_id)?;
    if provider.protocol != Protocol::Anthropic || !is_local_url(&provider.base_url) {
        return None;
    }
    let upstream_body = octra_llm_protocol::encode_request(Protocol::Anthropic, ir).ok()?;
    let upstream = state.upstream();
    let url = upstream.endpoint(&provider, &target.model_id, false).ok()?;
    let mut body = upstream_body;
    body["model"] = json!(target.model_id);
    let builder = upstream
        .post_json(&provider, &format!("{url}/count_tokens"), &body, false)
        .ok()?;
    let result = timeout(upstream.request_timeout(), upstream.send_json(builder))
        .await
        .ok()?;
    let result = result.ok()?;
    result.get("input_tokens").and_then(Value::as_u64)
}

/// Ingress protocol inferred from request headers; used to shape auth errors
/// on endpoints that accept both protocols (such as `GET /v1/models`).
fn error_protocol(request: &Incoming) -> Protocol {
    if wants_anthropic(request) {
        Protocol::Anthropic
    } else {
        Protocol::Openai
    }
}

fn wants_anthropic(request: &Incoming) -> bool {
    request.headers.contains_key("anthropic-version")
        || request
            .headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|agent| {
                let agent = agent.to_ascii_lowercase();
                agent.contains("anthropic") || agent.contains("claude")
            })
}

fn header_str<'a>(request: &'a Incoming, name: &str) -> Option<&'a str> {
    request
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
}

/// Virtual key authentication. `Ok(None)` means anonymous local access;
/// `Ok(Some(key))` is a matched, live key.
async fn authorize(
    snapshot: &RuntimeSnapshot,
    request: &Incoming,
) -> Result<Option<octra_storage::LlmVirtualKeyRecord>, GatewayError> {
    let presented = presented_key(
        header_str(request, header::AUTHORIZATION.as_str()),
        header_str(request, "x-api-key"),
    );
    match keys::authenticate(&snapshot.data.virtual_keys, presented, Utc::now()) {
        KeyOutcome::Anonymous => Ok(None),
        KeyOutcome::Authorized(key) => Ok(Some(*key)),
        KeyOutcome::Rejected(reason) => {
            Err(GatewayError::unauthorized(reason).with_code("invalid_api_key"))
        }
    }
}

async fn touch(state: &LlmDataState, key_id: &str) {
    if let Err(error) = state.storage.llm.touch_virtual_key(key_id).await {
        tracing::debug!(error = ?error, key_id, "cannot update virtual key last_used_at");
    }
}

/// Rejects before any provider call when a virtual key has spent its token or
/// cost budget for a window. Usage is read from persisted rows, so limits
/// survive restarts; concurrent requests may overshoot by roughly one call.
async fn enforce_virtual_key_limits(
    state: &LlmDataState,
    snapshot: &RuntimeSnapshot,
    key: &octra_storage::LlmVirtualKeyRecord,
) -> Result<(), GatewayError> {
    let Some(policy) = snapshot.virtual_key_policy(&key.id) else {
        return Ok(());
    };
    if policy.limits().is_empty() {
        return Ok(());
    }
    let now = Utc::now();
    let mut usage = BTreeMap::new();
    let family = if key.key_family.trim().is_empty() {
        key.id.as_str()
    } else {
        key.key_family.as_str()
    };
    for limit in policy.limits() {
        let since = vk_policy::window_start(limit.window, now);
        let totals = state
            .storage
            .llm
            .virtual_key_usage_since_family(family, &since)
            .await
            .map_err(|error| {
                tracing::warn!(
                    error = ?error,
                    virtual_key_id = %key.id,
                    "cannot read virtual key usage"
                );
                GatewayError::new(
                    503,
                    octra_llm_core::ErrorKind::Config,
                    "cannot read virtual key usage",
                )
                .with_code("virtual_key_usage_unavailable")
            })?;
        usage.insert(limit.window, totals);
    }
    let Some(exceeded) = policy.exceeded(|window| usage.get(&window).cloned().unwrap_or_default())
    else {
        return Ok(());
    };
    let message = match exceeded.limit.tokens {
        Some(tokens) if exceeded.usage.tokens.max(0) as u64 >= tokens => format!(
            "virtual key {} token limit exceeded (used {} of {})",
            exceeded.window.as_str(),
            exceeded.usage.tokens,
            tokens
        ),
        _ => format!(
            "virtual key {} cost limit exceeded (used {} of {} micros)",
            exceeded.window.as_str(),
            exceeded.usage.cost_micros,
            exceeded.limit.cost_micros.unwrap_or_default()
        ),
    };
    Err(GatewayError::upstream(429, message).with_code("virtual_key_limit_exceeded"))
}

async fn handle(
    state: LlmDataState,
    request: Incoming,
    body: RemoteBody,
    ingress: Protocol,
    model_hint: Option<String>,
    stream_override: Option<bool>,
) -> Response {
    let snapshot = state.runtime.snapshot().await;
    let key = match authorize(&snapshot, &request).await {
        Ok(key) => key,
        Err(error) => return protocol_response(ingress, &error),
    };
    if let Some(key) = &key {
        touch(&state, &key.id).await;
    }
    let body = match body.0 {
        Ok(body) => body,
        Err(response) => return response.0,
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return protocol_response(
                ingress,
                &GatewayError::invalid_request(format!("invalid JSON body: {error}")),
            );
        }
    };

    let requested_model = model_hint
        .or_else(|| {
            value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let request_id = octra_data_plane::new_request_id();
    let mut tracker = UsageTracker::new(request_id.clone(), requested_model.clone());
    tracker.record.protocol = Some(ingress.to_string());
    tracker.record.api_key_id = key.as_ref().map(|key| key.id.clone());
    let consumer_id = key.as_ref().and_then(|key| key.consumer_id.as_deref());
    let virtual_key_id = key.as_ref().map(|key| key.id.clone());

    if let Some(key) = &key
        && let Err(error) = enforce_virtual_key_limits(&state, &snapshot, key).await
    {
        tracker.record.error = Some(error.message.clone());
        tracker.finish_with_status(error.status);
        usage::spawn_record(state.storage.clone(), tracker.record.clone());
        return with_request_id(protocol_response(ingress, &error), &request_id);
    }

    let model_error = if !snapshot.permits_model(consumer_id, &requested_model) {
        Some(
            GatewayError::forbidden(format!(
                "consumer is not allowed to use model '{requested_model}'"
            ))
            .with_code("model_forbidden"),
        )
    } else if !snapshot.virtual_key_permits_model(virtual_key_id.as_deref(), &requested_model) {
        Some(
            GatewayError::forbidden(format!(
                "virtual key is not allowed to use model '{requested_model}'"
            ))
            .with_code("model_forbidden"),
        )
    } else {
        None
    };
    if let Some(error) = model_error {
        tracker.record.error = Some(error.message.clone());
        tracker.finish_with_status(error.status);
        usage::spawn_record(state.storage.clone(), tracker.record.clone());
        return with_request_id(protocol_response(ingress, &error), &request_id);
    }

    let mut ir = match decode_request(ingress, value) {
        Ok(ir) => ir,
        Err(error) => {
            let error = error.into_gateway();
            tracker.record.error = Some(error.message.clone());
            tracker.finish_with_status(error.status);
            usage::spawn_record(state.storage.clone(), tracker.record.clone());
            return protocol_response(ingress, &error);
        }
    };

    if ir.model.is_empty() || ingress == Protocol::Gemini {
        ir.model = requested_model.clone();
    }
    if let Some(stream) = stream_override {
        ir.stream = Some(stream);
    }

    let session = header_str(&request, &state.config.llm.session_header).map(str::to_string);
    let user_agent = header_str(&request, "user-agent");
    let intent = header_str(&request, &state.config.llm.intent_header);
    let consumer_group_ids = consumer_id
        .map(|id| snapshot.consumer_group_ids(id))
        .unwrap_or(&[]);
    let meta = LlmRequestMeta {
        request_id: &request_id,
        ingress,
        consumer_id,
        virtual_key_id: virtual_key_id.as_deref(),
        virtual_key_family: key.as_ref().map(|key| key.key_family.as_str()),
        consumer_group_ids,
        session_id: session.as_deref(),
        client_ip: request.client_ip,
        user_agent,
        intent,
    };
    let mut request_state = LlmRequestState::new();
    let mut response_headers = LlmResponseHeaders::new();
    let request_chain = state
        .plugins
        .as_ref()
        .map(|plugins| plugins.request_chain(consumer_id, virtual_key_id.as_deref(), &ir.model));
    let mut cached_response: Option<ChatResponse> = None;
    if let Some(chain) = &request_chain {
        let rejection = match chain
            .on_request_outcome(&meta, &mut response_headers, &mut ir, &mut request_state)
            .await
        {
            Ok(LlmRequestOutcome::Continue) => None,
            Ok(LlmRequestOutcome::Reject(error)) => Some(error),
            Ok(LlmRequestOutcome::Respond(response)) => {
                cached_response = Some(response);
                None
            }
            Err(error) => Some(chain_error(error)),
        };
        if let Some(error) = rejection {
            log_rejection(
                chain,
                &meta,
                &ir.model,
                &tracker,
                &error,
                &mut request_state,
            )
            .await;
            tracker.record.error = Some(error.message.clone());
            tracker.finish_with_status(error.status);
            usage::spawn_record(state.storage.clone(), tracker.record.clone());
            return finish(
                protocol_response(ingress, &error),
                &response_headers,
                &request_id,
            );
        }
        // A plugin may have rewritten the model; policy applies to the
        // effective model so an alias cannot escape an allow-list.
        if ir.model.trim().is_empty() {
            let error = GatewayError::invalid_request("model is required");
            tracker.record.error = Some(error.message.clone());
            tracker.finish_with_status(error.status);
            usage::spawn_record(state.storage.clone(), tracker.record.clone());
            return finish(
                protocol_response(ingress, &error),
                &response_headers,
                &request_id,
            );
        }
        let effective_error = if !snapshot.permits_model(consumer_id, &ir.model) {
            Some(
                GatewayError::forbidden(format!(
                    "consumer is not allowed to use model '{}'",
                    ir.model
                ))
                .with_code("model_forbidden"),
            )
        } else if !snapshot.virtual_key_permits_model(virtual_key_id.as_deref(), &ir.model) {
            Some(
                GatewayError::forbidden(format!(
                    "virtual key is not allowed to use model '{}'",
                    ir.model
                ))
                .with_code("model_forbidden"),
            )
        } else {
            None
        };
        if let Some(error) = effective_error {
            log_rejection(
                chain,
                &meta,
                &ir.model,
                &tracker,
                &error,
                &mut request_state,
            )
            .await;
            tracker.record.error = Some(error.message.clone());
            tracker.finish_with_status(error.status);
            usage::spawn_record(state.storage.clone(), tracker.record.clone());
            return finish(
                protocol_response(ingress, &error),
                &response_headers,
                &request_id,
            );
        }
    }
    let effective_model = ir.model.clone();

    // A request-stage plugin may answer from a cache. Model policy above ran
    // against the effective model, so a short-circuit cannot bypass consumer
    // or virtual-key restrictions.
    if let Some(mut response) = cached_response {
        response.model = requested_model.clone();
        if ir.stream == Some(true) {
            match stream::cached_response(&response, ingress, ir.include_usage.unwrap_or(false)) {
                Ok(cached) => {
                    tracker.record.model_id = Some(effective_model.clone());
                    tracker.record.finish_reason = Some("stop".to_string());
                    // A cache hit costs no upstream tokens; the stored body
                    // keeps the response it was cached with.
                    tracker.record.usage = Usage::default();
                    tracker.finish_with_status(200);
                    if let Some(chain) = &request_chain {
                        let event = LlmLogEvent {
                            model: &effective_model,
                            requested_model: Some(&requested_model),
                            provider_id: None,
                            provider_name: None,
                            account_id: None,
                            account_name: None,
                            status: 200,
                            latency_ms: tracker.elapsed_ms(),
                            usage: &tracker.record.usage,
                            finish_reason: tracker.record.finish_reason.as_deref(),
                            streaming: true,
                            error: None,
                        };
                        chain.on_log(&meta, &event, &mut request_state).await;
                    }
                    usage::spawn_record(state.storage.clone(), tracker.record.clone());
                    return finish(cached, &response_headers, &request_id);
                }
                Err(error) => {
                    tracker.record.error = Some(error.message.clone());
                    tracker.finish_with_status(error.status);
                    usage::spawn_record(state.storage.clone(), tracker.record.clone());
                    return finish(
                        protocol_response(ingress, &error),
                        &response_headers,
                        &request_id,
                    );
                }
            }
        } else {
            match translate::encode_response(ingress, &response) {
                Ok(body) => {
                    tracker.record.model_id = Some(effective_model.clone());
                    tracker.record.finish_reason = Some("stop".to_string());
                    // A cache hit costs no upstream tokens; the stored body
                    // keeps the response it was cached with.
                    tracker.record.usage = Usage::default();
                    tracker.finish_with_status(200);
                    if let Some(chain) = &request_chain {
                        let event = LlmLogEvent {
                            model: &effective_model,
                            requested_model: Some(&requested_model),
                            provider_id: None,
                            provider_name: None,
                            account_id: None,
                            account_name: None,
                            status: 200,
                            latency_ms: tracker.elapsed_ms(),
                            usage: &tracker.record.usage,
                            finish_reason: tracker.record.finish_reason.as_deref(),
                            streaming: false,
                            error: None,
                        };
                        chain.on_log(&meta, &event, &mut request_state).await;
                    }
                    usage::spawn_record(state.storage.clone(), tracker.record.clone());
                    return finish(json_response(body), &response_headers, &request_id);
                }
                Err(error) => {
                    tracker.record.error = Some(error.message.clone());
                    tracker.finish_with_status(error.status);
                    usage::spawn_record(state.storage.clone(), tracker.record.clone());
                    return finish(
                        protocol_response(ingress, &error),
                        &response_headers,
                        &request_id,
                    );
                }
            }
        }
    }

    let mut trace = ExecTrace::default();
    let response = execute(
        &state,
        &snapshot,
        &meta,
        &requested_model,
        &effective_model,
        &ir,
        tracker,
        ingress,
        &mut request_state,
        &mut trace,
    )
    .await;
    match response {
        Ok(response) => finish(response, &response_headers, &request_id),
        Err(mut error) => {
            // A bound attempt already ran `on_error` on its provider chain
            // before logging. Fall back to the request chain only when the
            // request failed before any attempt was made.
            if trace.chain.is_none()
                && let Some(chain) = request_chain.as_ref()
                && let Err(chain_error) = chain
                    .on_error(&meta, None, None, &mut error, &mut request_state)
                    .await
            {
                error = crate::error::chain_error(chain_error);
            }
            finish(
                protocol_response(ingress, &error),
                &response_headers,
                &request_id,
            )
        }
    }
}

/// Attaches plugin-provided headers, then the host request id.
fn finish(mut response: Response, headers: &LlmResponseHeaders, request_id: &str) -> Response {
    for (name, value) in headers.iter() {
        response.headers_mut().insert(name, value.clone());
    }
    with_request_id(response, request_id)
}

fn with_request_id(mut response: Response, request_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response
            .headers_mut()
            .insert(octra_data_plane::HEADER_REQUEST_ID, value);
    }
    response
}

fn fail_request(
    state: &LlmDataState,
    tracker: &mut UsageTracker,
    error: GatewayError,
) -> GatewayError {
    tracker.record.error = Some(error.message.clone());
    tracker.finish_with_status(error.status);
    usage::spawn_record(state.storage.clone(), tracker.record.clone());
    error
}

/// Routing facts needed after `execute` returns, most importantly which
/// plugin chain and provider/account actually handled the final attempt.
#[derive(Default)]
struct ExecTrace {
    chain: Option<Arc<LlmPluginChain>>,
    provider_id: Option<String>,
    account_id: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    state: &LlmDataState,
    snapshot: &RuntimeSnapshot,
    meta: &LlmRequestMeta<'_>,
    requested_model: &str,
    effective_model: &str,
    ir: &ChatRequest,
    mut tracker: UsageTracker,
    ingress: Protocol,
    request_state: &mut LlmRequestState,
    trace: &mut ExecTrace,
) -> LlmResult<Response> {
    let Some(plan) = snapshot.resolve(effective_model) else {
        let error = GatewayError::not_found(format!(
            "model '{effective_model}' is not exposed by this gateway"
        ))
        .with_code("model_not_found");
        return Err(fail_request(state, &mut tracker, error));
    };
    if plan.is_empty() {
        let error = GatewayError::not_found(format!(
            "model '{effective_model}' has no enabled routing target"
        ));
        return Err(fail_request(state, &mut tracker, error));
    }

    let stateful_responses =
        ingress == Protocol::OpenaiResponses && ir.extra.contains_key("previous_response_id");
    let upstream = state.upstream();
    let now_ms = Utc::now().timestamp_millis();
    let tick = snapshot.round_robin().next(plan.group_slot());
    let intent = ir
        .extra
        .get("octra_intent")
        .and_then(Value::as_str)
        .and_then(RoutingIntent::parse);
    let estimated_tokens = estimate_tokens(ir);
    let routing_text = similarity::request_text(ir, 8_000);
    let (mut attempts, fallback) = plan.parts(tick);
    rank_targets(
        &mut attempts,
        &plan.strategy,
        intent.as_ref(),
        estimated_tokens,
        &routing_text,
        |target| target_signal(snapshot, target, now_ms),
    );
    if matches!(
        plan.strategy.as_str(),
        "smart" | "intent" | "pace" | "usage" | "quota" | "semantic"
    ) {
        tracing::debug!(
            strategy = %plan.strategy,
            intent = ?intent,
            tokens = estimated_tokens,
            targets = ?attempts.iter().map(Target::label).collect::<Vec<_>>(),
            "llm target order"
        );
    }
    attempts.extend(fallback);
    let mut last_error: Option<GatewayError> = None;
    let mut budget = 0usize;
    let virtual_key_policy = meta
        .virtual_key_id
        .and_then(|id| snapshot.virtual_key_policy(id));

    'targets: for target in attempts.iter() {
        if budget >= MAX_ATTEMPTS {
            break;
        }
        let Some(provider) = snapshot.provider(&target.provider_id) else {
            continue;
        };
        if stateful_responses && provider.protocol != Protocol::OpenaiResponses {
            last_error = Some(
                GatewayError::invalid_request(
                    "previous_response_id requires an OpenAI Responses upstream",
                )
                .with_code("previous_response_not_supported"),
            );
            continue;
        }
        if !snapshot.permits_provider(meta.consumer_id, &provider.id, &provider.name) {
            last_error = Some(
                GatewayError::forbidden(format!(
                    "consumer is not allowed to use provider '{}'",
                    provider.name
                ))
                .with_code("provider_forbidden"),
            );
            continue;
        }
        if let Some(policy) = virtual_key_policy
            && !policy.permits_provider(&provider.id, &provider.name)
        {
            last_error = Some(
                GatewayError::forbidden(format!(
                    "virtual key is not allowed to use provider '{}'",
                    provider.name
                ))
                .with_code("provider_forbidden"),
            );
            continue;
        }

        let accounts = snapshot.accounts_for(&provider.id);
        let restricted_accounts =
            virtual_key_policy.is_some_and(|policy| policy.has_account_restrictions());
        let selected: Vec<Option<Arc<AccountRuntime>>> = if accounts.is_empty() {
            if restricted_accounts {
                last_error = Some(
                    GatewayError::forbidden(format!(
                        "virtual key is not allowed to use provider '{}'",
                        provider.name
                    ))
                    .with_code("account_forbidden"),
                );
                continue;
            }
            // No account pool configured: use the provider credential.
            vec![None]
        } else {
            let candidates = accounts::candidates(
                accounts,
                target.account_id.as_deref(),
                &plan.strategy,
                meta.session_id,
                tick,
                now_ms,
            );
            if candidates.is_empty() {
                last_error = Some(
                    GatewayError::new(
                        503,
                        octra_llm_core::ErrorKind::Upstream,
                        format!("provider {} has no available account", provider.name),
                    )
                    .with_code("account_unavailable"),
                );
                continue;
            }
            let mut selected = candidates.into_iter().map(Some).collect::<Vec<_>>();
            if let Some(policy) = virtual_key_policy
                && restricted_accounts
            {
                selected.retain(|account| {
                    account.as_ref().is_some_and(|account| {
                        policy.permits_account(&account.record.id, &account.record.name)
                    })
                });
                if selected.is_empty() {
                    last_error = Some(
                        GatewayError::forbidden(format!(
                            "virtual key is not allowed to use provider '{}' accounts",
                            provider.name
                        ))
                        .with_code("account_forbidden"),
                    );
                    continue;
                }
            }
            selected
        };

        for account in selected {
            if budget >= MAX_ATTEMPTS {
                break 'targets;
            }
            if let Some(account) = &account
                && !account.try_acquire(now_ms)
            {
                last_error = Some(
                    GatewayError::new(
                        429,
                        octra_llm_core::ErrorKind::Upstream,
                        format!("account {} is paced", account.record.name),
                    )
                    .with_code("account_paced"),
                );
                continue;
            }
            budget += 1;

            let account_secret_ref = account
                .as_ref()
                .and_then(|account| account.record.secret_ref.as_deref());
            let oauth_account = account
                .as_ref()
                .is_some_and(|account| account.record.credential_kind == "oauth");
            // Any provider that authenticates needs a resolvable secret
            // reference; OAuth accounts resolve theirs from the token store.
            if provider.requires_secret()
                && provider.secret_ref.is_none()
                && account_secret_ref.is_none()
                && !oauth_account
                && upstream.test_key().is_none()
            {
                last_error = Some(
                    GatewayError::config(format!(
                        "provider {} has no secret_ref configured",
                        provider.name
                    ))
                    .with_code("secret_unavailable"),
                );
                continue;
            }

            tracker.record.provider_id = Some(provider.id.clone());
            tracker.record.provider_name = Some(provider.name.clone());
            tracker.record.model_id = Some(target.model_id.clone());
            tracker.record.account_id = account.as_ref().map(|account| account.record.id.clone());
            tracker.record.account_name =
                account.as_ref().map(|account| account.record.name.clone());
            tracker.record.pricing = snapshot.model_pricing(&provider.id, &target.model_id);

            let provider_chain = state.plugins.as_ref().map(|plugins| {
                plugins.provider_chain(
                    meta.consumer_id,
                    meta.virtual_key_id,
                    requested_model,
                    &provider.id,
                )
            });
            trace.chain = provider_chain.clone();
            trace.provider_id = Some(provider.id.clone());
            trace.account_id = account.as_ref().map(|account| account.record.id.clone());
            let provider_info = LlmProviderInfo {
                attempt: budget as u32,
                target_model_id: &target.model_id,
                provider_id: &provider.id,
                provider_name: &provider.name,
                protocol: target.protocol,
                account_id: account.as_ref().map(|account| account.record.id.as_str()),
                account_name: account.as_ref().map(|account| account.record.name.as_str()),
            };
            match try_attempt(
                state,
                snapshot,
                upstream.as_ref(),
                &provider,
                target,
                ir,
                &tracker,
                ingress,
                account.as_ref(),
                provider_chain.as_ref(),
                requested_model,
                meta,
                &provider_info,
                request_state,
            )
            .await
            {
                Ok(outcome) => {
                    // Streaming responses carry their own tracker: usage is
                    // recorded when the stream closes, once the upstream has
                    // reported its final token counts.
                    if !outcome.streaming {
                        tracker.record.usage = outcome.usage.clone();
                        tracker.record.finish_reason =
                            Some(usage::finish_reason_label(&outcome.finish_reason).to_string());
                        tracker.finish_with_status(200);
                        if let Some(account) = &account {
                            account.observe_success(tracker.elapsed_ms());
                            consume_account(account, &tracker);
                        }
                        if let Some(chain) = &provider_chain {
                            let event = LlmLogEvent {
                                model: effective_model,
                                requested_model: Some(requested_model),
                                provider_id: Some(&provider.id),
                                provider_name: Some(&provider.name),
                                account_id: account.as_ref().map(|a| a.record.id.as_str()),
                                account_name: account.as_ref().map(|a| a.record.name.as_str()),
                                status: 200,
                                latency_ms: tracker.elapsed_ms(),
                                usage: &tracker.record.usage,
                                finish_reason: tracker.record.finish_reason.as_deref(),
                                streaming: false,
                                error: None,
                            };
                            chain.on_log(meta, &event, request_state).await;
                        }
                        usage::spawn_record(state.storage.clone(), tracker.record.clone());
                    }
                    return Ok(outcome.response);
                }
                Err(error) => {
                    tracing::warn!(
                        request_id = %tracker.record.request_id,
                        provider = %provider.name,
                        model = %target.model_id,
                        account = ?tracker.record.account_name,
                        status = error.status,
                        retryable = error.kind.is_retryable(),
                        "llm upstream attempt failed"
                    );
                    if let Some(account) = &account {
                        account.observe_failure();
                    }
                    let retryable = error.kind.is_retryable();
                    let rate_limited =
                        error.status == 429 || error.code.as_deref() == Some("quota_exceeded");
                    if rate_limited && let Some(account) = &account {
                        let cooldown_ms =
                            state.config.llm.account_cooldown_secs.max(1) as i64 * 1000;
                        let until = Utc::now().timestamp_millis() + cooldown_ms;
                        account.mark_cooldown(until);
                        spawn_cooldown(
                            state.storage.clone(),
                            account.record.id.clone(),
                            until,
                            error.message.clone(),
                        );
                    }
                    last_error = Some(error);
                    if rate_limited {
                        continue;
                    }
                    if !retryable {
                        break 'targets;
                    }
                }
            }
        }
    }

    let mut error = last_error.unwrap_or_else(|| {
        GatewayError::not_found(format!(
            "model '{effective_model}' has no provider that can serve it"
        ))
    });
    if let Some(chain) = &trace.chain {
        // Sanitize/enrich the error before the log stage sees it, so audit
        // records never carry raw upstream text that redaction would mask.
        if let Err(chain_error) = chain
            .on_error(
                meta,
                trace.provider_id.as_deref(),
                trace.account_id.as_deref(),
                &mut error,
                request_state,
            )
            .await
        {
            error = crate::error::chain_error(chain_error);
        }
        let event = LlmLogEvent {
            model: effective_model,
            requested_model: Some(requested_model),
            provider_id: tracker.record.provider_id.as_deref(),
            provider_name: tracker.record.provider_name.as_deref(),
            account_id: tracker.record.account_id.as_deref(),
            account_name: tracker.record.account_name.as_deref(),
            status: error.status,
            latency_ms: tracker.elapsed_ms(),
            usage: &tracker.record.usage,
            finish_reason: tracker.record.finish_reason.as_deref(),
            streaming: false,
            error: Some(&error.message),
        };
        chain.on_log(meta, &event, request_state).await;
    }
    Err(fail_request(state, &mut tracker, error))
}

/// Emits a log-stage observation for a request rejected before routing.
async fn log_rejection(
    chain: &LlmPluginChain,
    meta: &LlmRequestMeta<'_>,
    model: &str,
    tracker: &UsageTracker,
    error: &GatewayError,
    state: &mut LlmRequestState,
) {
    let event = LlmLogEvent {
        model,
        requested_model: Some(model),
        provider_id: None,
        provider_name: None,
        account_id: None,
        account_name: None,
        status: error.status,
        latency_ms: tracker.elapsed_ms(),
        usage: &tracker.record.usage,
        finish_reason: None,
        streaming: false,
        error: Some(&error.message),
    };
    chain.on_log(meta, &event, state).await;
}

/// Health, quota and capability signals used to rank one routing target.
fn target_signal(snapshot: &RuntimeSnapshot, target: &Target, now_ms: i64) -> TargetSignal {
    let profile = snapshot.model_profile(&target.provider_id, &target.model_id);
    let mut usage = 0i64;
    let mut latency_ms = 0u64;
    let mut failures = 0u64;
    let mut pace_wait_ms = 0u64;
    let mut quota: Option<i64> = None;
    let mut unlimited = false;
    let mut accounts = 0usize;
    for account in snapshot.accounts_for(&target.provider_id) {
        if let Some(pinned) = target.account_id.as_deref()
            && account.id() != pinned
        {
            continue;
        }
        accounts += 1;
        usage = usage.saturating_add(account.used());
        latency_ms = latency_ms.max(account.latency_ewma_ms());
        failures = failures.saturating_add(account.failure_ewma());
        pace_wait_ms = pace_wait_ms.max(account.pace_wait_ms(now_ms));
        match account.quota_remaining() {
            Some(remaining) => {
                *quota.get_or_insert(0) += remaining.max(0);
            }
            None => unlimited = true,
        }
    }
    let price_per_million = snapshot
        .model_pricing(&target.provider_id, &target.model_id)
        .and_then(|pricing| {
            pricing
                .get("input_per_million")
                .and_then(Value::as_f64)
                .or_else(|| pricing.get("input").and_then(Value::as_f64))
        })
        .unwrap_or(0.0);
    TargetSignal {
        context_window: profile.context_window,
        vision: profile.vision,
        tools: profile.tools,
        reasoning: profile.reasoning,
        usage,
        quota_remaining: if accounts == 0 || unlimited {
            i64::MAX
        } else {
            quota.unwrap_or(0)
        },
        latency_ms,
        failures,
        price_per_million,
        pace_wait_ms,
    }
}

/// Adds a completed request to the live account quota counter.
fn consume_account(account: &AccountRuntime, tracker: &UsageTracker) {
    let usage = &tracker.record.usage;
    let tokens = usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .min(i64::MAX as u64) as i64;
    let cost_micros = tracker
        .record
        .pricing
        .as_ref()
        .and_then(|pricing| cost::compute(usage, pricing))
        .map(|cost| cost.micros)
        .unwrap_or(0);
    account.consume(1, tokens, cost_micros);
}

fn spawn_cooldown(
    storage: Arc<octra_storage::Storage>,
    account_id: String,
    until_ms: i64,
    last_error: String,
) {
    let Some(until) = DateTime::<Utc>::from_timestamp_millis(until_ms) else {
        return;
    };
    let until = until.to_rfc3339();
    tokio::spawn(async move {
        if let Err(error) = storage
            .ai
            .set_account_cooldown(&account_id, &until, Some(&last_error))
            .await
        {
            tracing::warn!(account_id, error = ?error, "cannot persist account cooldown");
        }
    });
}

struct AttemptOutcome {
    response: Response,
    usage: Usage,
    finish_reason: octra_llm_core::FinishReason,
    streaming: bool,
}

fn is_unauthorized(error: &GatewayError) -> bool {
    error.status == 401
}

fn stream_meta(
    meta: &LlmRequestMeta<'_>,
    requested_model: &str,
    target: &Target,
    provider: &ProviderConfig,
    account: Option<&Arc<AccountRuntime>>,
    ingress: Protocol,
) -> stream::StreamMeta {
    stream::StreamMeta {
        request_id: meta.request_id.to_string(),
        model: target.model_id.clone(),
        requested_model: requested_model.to_string(),
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        account_id: account.map(|account| account.record.id.clone()),
        account_name: account.map(|account| account.record.name.clone()),
        ingress,
        consumer_id: meta.consumer_id.map(str::to_string),
        virtual_key_id: meta.virtual_key_id.map(str::to_string),
        virtual_key_family: meta.virtual_key_family.map(str::to_string),
        consumer_group_ids: meta.consumer_group_ids.to_vec(),
        session_id: meta.session_id.map(str::to_string),
        client_ip: meta.client_ip,
        user_agent: meta.user_agent.map(str::to_string),
        intent: meta.intent.map(str::to_string),
    }
}

async fn apply_response_chain(
    chain: Option<&Arc<LlmPluginChain>>,
    meta: &LlmRequestMeta<'_>,
    requested_model: &str,
    response: &mut ChatResponse,
    request_state: &mut LlmRequestState,
) -> LlmResult<()> {
    if let Some(chain) = chain {
        let mut usage = response.usage.clone();
        match chain
            .on_response(meta, requested_model, response, &mut usage, request_state)
            .await
        {
            Ok(None) => {}
            Ok(Some(error)) => return Err(error),
            Err(error) => return Err(chain_error(error)),
        }
        response.usage = usage;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn try_attempt(
    state: &LlmDataState,
    snapshot: &RuntimeSnapshot,
    upstream: &Upstream,
    provider: &ProviderConfig,
    target: &Target,
    ir: &ChatRequest,
    tracker: &UsageTracker,
    ingress: Protocol,
    account: Option<&Arc<AccountRuntime>>,
    chain: Option<&Arc<LlmPluginChain>>,
    requested_model: &str,
    meta: &LlmRequestMeta<'_>,
    provider_info: &LlmProviderInfo<'_>,
    request_state: &mut LlmRequestState,
) -> LlmResult<AttemptOutcome> {
    let mut ir = ir.clone();
    if let Some(chain) = chain {
        match chain
            .on_provider_request(meta, provider_info, &mut ir, request_state)
            .await
        {
            Ok(None) => {}
            Ok(Some(error)) => return Err(error),
            Err(error) => return Err(chain_error(error)),
        }
    }
    apply_model_limits(snapshot, provider, target, &mut ir);

    if let Some(cli) = provider.cli.as_ref() {
        let runner = crate::cli::CliRunner::new(cli.clone());
        let account_record = account.map(|account| &account.record);
        if ir.stream == Some(true) {
            let events = runner.stream(ir.clone(), account_record)?;
            let response = stream::start_events(
                &state.config,
                events,
                ingress,
                ir.include_usage.unwrap_or(false),
                state.storage.clone(),
                tracker.clone(),
                account.cloned(),
                chain.cloned(),
                stream_meta(meta, requested_model, target, provider, account, ingress),
                std::mem::take(request_state),
            );
            return Ok(AttemptOutcome {
                response,
                usage: Usage::default(),
                finish_reason: octra_llm_core::FinishReason::Stop,
                streaming: true,
            });
        }
        let mut response = runner.execute(ir.clone(), account_record).await?;
        response.model = requested_model.to_string();
        apply_response_chain(chain, meta, requested_model, &mut response, request_state).await?;
        if let Some(chain) = chain {
            chain
                .on_final_response(meta, requested_model, &response, request_state)
                .await;
        }
        let usage = response.usage.clone();
        let finish_reason = response.finish_reason;
        let body = translate::encode_response(ingress, &response)?;
        return Ok(AttemptOutcome {
            response: json_response(body),
            usage,
            finish_reason,
            streaming: false,
        });
    }

    let mut body = translate::encode_request(target.protocol, &ir)?;
    if target.protocol != Protocol::Gemini {
        body["model"] = json!(target.model_id);
    }

    if ir.stream == Some(true) && target.protocol == Protocol::Openai {
        body["stream_options"] = json!({"include_usage": true});
    }

    let url = upstream.endpoint(provider, &target.model_id, ir.stream == Some(true))?;

    if ir.stream == Some(true) {
        let mut refreshed = false;
        let upstream_response = loop {
            let builder = upstream
                .post_json_with_account(
                    provider,
                    &url,
                    &body,
                    true,
                    account.map(|account| account.as_ref()),
                )
                .await?;
            match timeout(upstream.request_timeout(), upstream.send_stream(builder)).await {
                Err(_) => {
                    return Err(GatewayError::timeout(
                        "upstream did not start streaming in time",
                    ));
                }
                Ok(Ok(response)) => break response,
                Ok(Err(error)) => {
                    if !refreshed
                        && is_unauthorized(&error)
                        && let Some(account) = account
                    {
                        upstream.force_refresh(provider, account).await?;
                        refreshed = true;
                        continue;
                    }
                    return Err(error);
                }
            }
        };
        let response = stream::start(
            &state.config,
            upstream_response,
            target.protocol,
            ingress,
            ir.include_usage.unwrap_or(false),
            state.storage.clone(),
            tracker.clone(),
            account.cloned(),
            chain.cloned(),
            stream_meta(meta, requested_model, target, provider, account, ingress),
            std::mem::take(request_state),
        );
        return Ok(AttemptOutcome {
            response,
            usage: Usage::default(),
            finish_reason: octra_llm_core::FinishReason::Stop,
            streaming: true,
        });
    }

    let mut refreshed = false;
    let value = loop {
        let builder = upstream
            .post_json_with_account(
                provider,
                &url,
                &body,
                false,
                account.map(|account| account.as_ref()),
            )
            .await?;
        match timeout(upstream.request_timeout(), upstream.send_json(builder)).await {
            Err(_) => return Err(GatewayError::timeout("upstream request timed out")),
            Ok(Ok(value)) => break value,
            Ok(Err(error)) => {
                if !refreshed
                    && is_unauthorized(&error)
                    && let Some(account) = account
                {
                    upstream.force_refresh(provider, account).await?;
                    refreshed = true;
                    continue;
                }
                return Err(error);
            }
        }
    };
    let mut response = translate::decode_response(target.protocol, value)?;
    // Clients see the model name they asked for, not the routed provider
    // model, so a model alias stays opaque.
    response.model = requested_model.to_string();
    apply_response_chain(chain, meta, requested_model, &mut response, request_state).await?;
    if let Some(chain) = chain {
        chain
            .on_final_response(meta, requested_model, &response, request_state)
            .await;
    }
    let usage = response.usage.clone();
    let finish_reason = response.finish_reason;
    let body = translate::encode_response(ingress, &response)?;
    Ok(AttemptOutcome {
        response: json_response(body),
        usage,
        finish_reason,
        streaming: false,
    })
}

/// Applies model limits (max output tokens) and media/tool restrictions from
/// the routing target's model record when one exists.
fn apply_model_limits(
    snapshot: &RuntimeSnapshot,
    provider: &ProviderConfig,
    target: &Target,
    ir: &mut ChatRequest,
) {
    let Some(model) = snapshot
        .data
        .models
        .iter()
        .find(|model| model.provider_id == provider.id && model.model_id == target.model_id)
    else {
        return;
    };
    if let Some(max_output) = model.max_output_tokens
        && max_output > 0
    {
        let capped = max_output.min(u32::MAX as i64) as u32;
        ir.max_tokens = Some(match ir.max_tokens {
            Some(requested) => requested.min(capped),
            None => capped,
        });
    }
    let capabilities: serde_json::Map<String, Value> =
        serde_json::from_str(&model.capabilities).unwrap_or_default();
    let supports_images = capabilities
        .get("vision")
        .or_else(|| capabilities.get("images"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !supports_images {
        for message in &mut ir.messages {
            if let Some(content) = &mut message.content {
                content
                    .parts
                    .retain(|part| !matches!(part, ContentPart::Image(_) | ContentPart::File(_)));
            }
        }
        for message in &mut ir.messages {
            for result in &mut message.tool_results {
                result
                    .content
                    .parts
                    .retain(|part| !matches!(part, ContentPart::Image(_) | ContentPart::File(_)));
            }
        }
    }
    if capabilities.get("tools").and_then(Value::as_bool) == Some(false) {
        ir.tools.clear();
        ir.tool_choice = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{Message, UserContent};

    fn header_map(pairs: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn estimates_tokens_for_text_and_cjk() {
        let mut request = ChatRequest::new("m", vec![Message::user("abcd")]);
        request.system = octra_llm_core::SystemPrompt::text("abcd");
        let ascii = estimate_tokens(&request);
        request.messages[0].content = Some(UserContent::text("你好世界"));
        let cjk = estimate_tokens(&request);
        assert!(cjk >= ascii);
    }

    #[test]
    fn anthropic_detection_from_headers() {
        let request = Incoming {
            method: axum::http::Method::POST,
            uri: "/v1/messages".parse().unwrap(),
            headers: header_map(&[("anthropic-version", "2023-06-01")]),
            client_ip: None,
        };
        assert!(wants_anthropic(&request));
        let request = Incoming {
            method: axum::http::Method::POST,
            uri: "/v1/messages".parse().unwrap(),
            headers: header_map(&[("user-agent", "Anthropic/JS 0.30")]),
            client_ip: None,
        };
        assert!(wants_anthropic(&request));
        let request = Incoming {
            method: axum::http::Method::POST,
            uri: "/v1/chat/completions".parse().unwrap(),
            headers: header_map(&[("user-agent", "openai-python/1.0")]),
            client_ip: None,
        };
        assert!(!wants_anthropic(&request));
    }
}
