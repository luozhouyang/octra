//! Server-Sent Events bridging between upstream and ingress protocols.
//!
//! Upstream chunks are decoded into canonical events with the protocol
//! adapters (fed by the shared `octra_data_plane::sse::SseDecoder`) and
//! re-encoded for the ingress protocol, so cross-protocol streaming shares
//! the same codecs as the rest of the gateway.

use std::borrow::Cow;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::stream::{BoxStream, StreamExt, unfold};
use octra_config::OctraConfig;
use octra_data_plane::sse::SseDecoder;
use octra_llm_core::{
    AssistantContent, ChatResponse, FinishReason, GatewayError, Message, Protocol, Reasoning, Role,
    StreamEvent, ToolCallKind, Usage, Value,
};
use octra_llm_plugin_runtime::LlmPluginChain;
use octra_llm_plugin_types::{LlmLogEvent, LlmRequestMeta, LlmRequestState};
use octra_llm_protocol::{StreamDecoder, StreamEncoder};
use octra_storage::Storage;
use reqwest::Response as UpstreamResponse;

use crate::accounts::AccountRuntime;
use crate::usage::{self, UsageTracker};

/// Starts a bridged SSE response. The HTTP status is already 200; upstream
/// failures after this point are reported as in-stream error events.
#[allow(clippy::too_many_arguments)]
pub fn start(
    config: &OctraConfig,
    upstream: UpstreamResponse,
    upstream_protocol: Protocol,
    ingress: Protocol,
    include_usage: bool,
    storage: Arc<Storage>,
    tracker: UsageTracker,
    account: Option<Arc<AccountRuntime>>,
    plugins: Option<Arc<LlmPluginChain>>,
    meta: StreamMeta,
    plugin_state: LlmRequestState,
) -> Response {
    let idle = Duration::from_secs(config.llm.stream_idle_timeout_secs.max(1));
    let state = BridgeState {
        upstream,
        framing: Framing::for_protocol(upstream_protocol),
        protocol_decoder: octra_llm_protocol::stream_decoder(upstream_protocol),
        encoder: octra_llm_protocol::stream_encoder_with_usage(ingress, include_usage),
        ingress,
        storage,
        tracker,
        account,
        plugins,
        meta,
        plugin_state,
        idle,
        finished: false,
        saw_finish: false,
        usage: Usage::default(),
        accumulator: StreamAccumulator::default(),
        pending_terminal: Vec::new(),
    };

    let body_stream = unfold(state, |mut state| async move {
        if state.finished {
            return None;
        }
        loop {
            let chunk = tokio::time::timeout(state.idle, state.upstream.chunk()).await;
            let bytes = match chunk {
                Ok(Ok(Some(bytes))) => bytes,
                Ok(Ok(None)) => {
                    let out = state.finish_stream(None).await;
                    state.finished = true;
                    if out.is_empty() {
                        return None;
                    }
                    return Some((Ok::<_, Infallible>(Bytes::from(out)), state));
                }
                Ok(Err(error)) => {
                    let message = if error.is_timeout() {
                        "upstream stream stalled".to_string()
                    } else {
                        format!("upstream stream failed: {error}")
                    };
                    let error =
                        GatewayError::upstream(502, message).with_code("stream_interrupted");
                    let out = state.fail_stream(error).await;
                    state.finished = true;
                    return Some((Ok(Bytes::from(out)), state));
                }
                Err(_) => {
                    let error = GatewayError::timeout("upstream stream idle timeout exceeded")
                        .with_code("stream_idle_timeout");
                    let out = state.fail_stream(error).await;
                    state.finished = true;
                    return Some((Ok(Bytes::from(out)), state));
                }
            };

            let mut out = Vec::new();
            let mut failed = None;
            if state.framing.overflowed() {
                failed = Some(
                    GatewayError::upstream(
                        502,
                        "upstream stream exceeded the SSE event buffer limit",
                    )
                    .with_code("upstream_stream_too_large"),
                );
            }
            for framed in state.framing.push(&bytes) {
                let payload = match framed {
                    Ok(framed) => match framed.event {
                        Some(name) => format!("{name}\n{}", framed.data),
                        None => framed.data,
                    },
                    Err(error) => {
                        failed = Some(
                            GatewayError::upstream(
                                502,
                                format!("invalid upstream stream frame: {error}"),
                            )
                            .with_code("upstream_stream_error"),
                        );
                        break;
                    }
                };
                let events = match state.protocol_decoder.decode_chunk(&payload) {
                    Ok(events) => events,
                    Err(error) => {
                        failed = Some(
                            GatewayError::upstream(502, error.message)
                                .with_code("upstream_stream_error"),
                        );
                        break;
                    }
                };
                let events = match state.plugins.clone() {
                    Some(chain) => {
                        let meta = state.meta.as_meta();
                        match chain
                            .on_stream_events(&meta, events, &mut state.plugin_state)
                            .await
                        {
                            Ok(events) => events,
                            Err(error) => {
                                failed = Some(crate::error::chain_error(error));
                                break;
                            }
                        }
                    }
                    None => events,
                };
                for event in &events {
                    state.encode(event, &mut out);
                }
            }
            if let Some(error) = failed {
                out.extend(state.fail_stream(error).await);
                return Some((Ok(Bytes::from(out)), state));
            }
            if state.finished && out.is_empty() {
                return None;
            }
            if !out.is_empty() {
                return Some((Ok(Bytes::from(out)), state));
            }
            // Keep-alive chunks produce no frames; read the next one.
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(body_stream))
        .unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot build stream response",
            )
                .into_response()
        })
}

/// Owned request metadata for the stream's lifetime.
#[derive(Clone)]
pub struct StreamMeta {
    pub request_id: String,
    pub model: String,
    pub requested_model: String,
    pub provider_id: String,
    pub provider_name: String,
    pub account_id: Option<String>,
    pub account_name: Option<String>,
    pub ingress: Protocol,
    pub consumer_id: Option<String>,
    pub virtual_key_id: Option<String>,
    pub virtual_key_family: Option<String>,
    pub consumer_group_ids: Vec<String>,
    pub session_id: Option<String>,
    pub client_ip: Option<std::net::IpAddr>,
    pub user_agent: Option<String>,
    pub intent: Option<String>,
    /// Keeps the virtual-key budget reservation alive until the stream ends.
    pub reservation: Option<Arc<crate::vk_reservation::VkReservation>>,
}

impl StreamMeta {
    fn as_meta(&self) -> LlmRequestMeta<'_> {
        LlmRequestMeta {
            request_id: &self.request_id,
            ingress: self.ingress,
            consumer_id: self.consumer_id.as_deref(),
            virtual_key_id: self.virtual_key_id.as_deref(),
            virtual_key_family: self.virtual_key_family.as_deref(),
            consumer_group_ids: &self.consumer_group_ids,
            session_id: self.session_id.as_deref(),
            client_ip: self.client_ip,
            user_agent: self.user_agent.as_deref(),
            intent: self.intent.as_deref(),
        }
    }
}

/// One tool call assembled from a start event and argument deltas.
#[derive(Debug)]
struct AccumulatedToolCall {
    index: usize,
    id: String,
    name: String,
    kind: ToolCallKind,
    arguments: String,
}

/// One provider-executed tool result observed in the stream.
#[derive(Debug)]
struct AccumulatedToolResult {
    index: usize,
    tool_use_id: String,
    content: Value,
    is_error: bool,
}

/// Reconstructs a canonical [`ChatResponse`] from the events that reached the
/// client. The response is only built after a successful finish: failed or
/// truncated streams must never enter a response cache.
#[derive(Debug, Default)]
struct StreamAccumulator {
    id: Option<String>,
    model: Option<String>,
    text: String,
    reasoning: String,
    signature: String,
    tool_calls: Vec<AccumulatedToolCall>,
    tool_results: Vec<AccumulatedToolResult>,
    usage: Usage,
    finish_reason: Option<FinishReason>,
    failed: bool,
}

impl StreamAccumulator {
    fn observe(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::MessageStart { id, model } => {
                self.id = Some(id.clone());
                self.model = Some(model.clone());
            }
            StreamEvent::TextDelta { text } => self.text.push_str(text),
            StreamEvent::ReasoningDelta { text } => self.reasoning.push_str(text),
            StreamEvent::ReasoningSignatureDelta { signature } => {
                self.signature.push_str(signature);
            }
            StreamEvent::ToolCallStart {
                index,
                id,
                name,
                kind,
            } => {
                if let Some(call) = self.tool_calls.iter_mut().find(|call| call.index == *index) {
                    call.id = id.clone();
                    call.name = name.clone();
                    call.kind = *kind;
                } else {
                    self.tool_calls.push(AccumulatedToolCall {
                        index: *index,
                        id: id.clone(),
                        name: name.clone(),
                        kind: *kind,
                        arguments: String::new(),
                    });
                }
            }
            StreamEvent::ToolCallDelta { index, arguments } => {
                if let Some(call) = self.tool_calls.iter_mut().find(|call| call.index == *index) {
                    call.arguments.push_str(arguments);
                } else {
                    self.tool_calls.push(AccumulatedToolCall {
                        index: *index,
                        id: String::new(),
                        name: String::new(),
                        kind: ToolCallKind::Function,
                        arguments: arguments.clone(),
                    });
                }
            }
            // The IR cannot represent the provider tool metadata start block;
            // only the result block is reconstructed.
            StreamEvent::ProviderToolStart { .. } => {}
            StreamEvent::ProviderToolResult {
                index,
                tool_use_id,
                content,
                is_error,
            } => {
                self.tool_results.push(AccumulatedToolResult {
                    index: *index,
                    tool_use_id: tool_use_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                });
            }
            StreamEvent::Usage(usage) => self.usage = usage.clone(),
            StreamEvent::Finish { reason } => self.finish_reason = Some(*reason),
            StreamEvent::Error(_) => self.failed = true,
        }
    }

    fn build(&self, requested_model: &str) -> Option<ChatResponse> {
        if self.failed {
            return None;
        }
        let finish_reason = self.finish_reason?;
        let mut message = Message::new(Role::Assistant);
        if !self.reasoning.is_empty() || !self.signature.is_empty() {
            message.assistant.push(AssistantContent::Reasoning {
                text: self.reasoning.clone(),
                signature: (!self.signature.is_empty()).then(|| self.signature.clone()),
            });
        }
        if !self.text.is_empty() {
            message.assistant.push(AssistantContent::Text {
                text: self.text.clone(),
            });
        }
        let mut tool_calls: Vec<&AccumulatedToolCall> = self.tool_calls.iter().collect();
        tool_calls.sort_by_key(|call| call.index);
        for call in tool_calls {
            let arguments = serde_json::from_str(&call.arguments)
                .unwrap_or_else(|_| Value::String(call.arguments.clone()));
            message.assistant.push(AssistantContent::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments,
                kind: (call.kind != ToolCallKind::Function).then_some(call.kind),
            });
        }
        let mut tool_results: Vec<&AccumulatedToolResult> = self.tool_results.iter().collect();
        tool_results.sort_by_key(|result| result.index);
        for result in tool_results {
            message.assistant.push(AssistantContent::ToolResult {
                tool_use_id: result.tool_use_id.clone(),
                content: result.content.clone(),
                is_error: result.is_error,
            });
        }
        let reasoning = if !self.reasoning.is_empty() || !self.signature.is_empty() {
            Some(Reasoning {
                text: self.reasoning.clone(),
                signature: (!self.signature.is_empty()).then(|| self.signature.clone()),
                encrypted: Vec::new(),
            })
        } else {
            None
        };
        let model = if requested_model.is_empty() {
            self.model.clone().unwrap_or_default()
        } else {
            requested_model.to_string()
        };
        Some(ChatResponse {
            id: self
                .id
                .clone()
                .unwrap_or_else(|| "octra-cached".to_string()),
            model,
            message,
            finish_reason,
            usage: self.usage.clone(),
            reasoning,
            programmatic: None,
            extra: Default::default(),
        })
    }
}

/// Replays a stored response as protocol-native SSE. This is used when a
/// streaming request is served from a response cache.
pub fn cached_response(
    response: &ChatResponse,
    ingress: Protocol,
    include_usage: bool,
) -> Result<Response, GatewayError> {
    fn encode_error(error: octra_llm_core::ProtocolError) -> GatewayError {
        GatewayError::internal(format!("cannot encode a cached stream: {}", error.message))
    }

    let mut encoder = octra_llm_protocol::stream_encoder_with_usage(ingress, include_usage);
    let mut events = vec![StreamEvent::MessageStart {
        id: response.id.clone(),
        model: response.model.clone(),
    }];
    let mut index = 0usize;
    for block in &response.message.assistant {
        match block {
            AssistantContent::Reasoning { text, signature } => {
                if !text.is_empty() {
                    events.push(StreamEvent::ReasoningDelta { text: text.clone() });
                }
                if let Some(signature) = signature
                    && !signature.is_empty()
                {
                    events.push(StreamEvent::ReasoningSignatureDelta {
                        signature: signature.clone(),
                    });
                }
            }
            AssistantContent::Text { text } => {
                events.push(StreamEvent::TextDelta { text: text.clone() });
            }
            AssistantContent::Refusal { text } => {
                // There is no separate refusal stream event; the refusal text
                // is visible content for streaming clients.
                events.push(StreamEvent::TextDelta { text: text.clone() });
            }
            AssistantContent::ToolCall {
                id,
                name,
                arguments,
                kind,
            } => {
                events.push(StreamEvent::ToolCallStart {
                    index,
                    id: id.clone(),
                    name: name.clone(),
                    kind: kind.unwrap_or(ToolCallKind::Function),
                });
                events.push(StreamEvent::ToolCallDelta {
                    index,
                    arguments: arguments.to_string(),
                });
                index += 1;
            }
            AssistantContent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let tool_type = content
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("web_search_tool_result")
                    .to_string();
                events.push(StreamEvent::ProviderToolStart { index, tool_type });
                events.push(StreamEvent::ProviderToolResult {
                    index,
                    tool_use_id: tool_use_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                });
                index += 1;
            }
            AssistantContent::Raw { .. } => {}
        }
    }
    if !response.usage.is_empty() {
        events.push(StreamEvent::Usage(response.usage.clone()));
    }
    events.push(StreamEvent::Finish {
        reason: response.finish_reason,
    });

    let mut bytes = Vec::new();
    for event in &events {
        for frame in encoder.encode(event).map_err(encode_error)? {
            bytes.extend(frame.to_bytes());
        }
    }
    for frame in encoder.finish().map_err(encode_error)? {
        bytes.extend(frame.to_bytes());
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from(bytes))
        .map_err(|error| GatewayError::internal(format!("cannot build a cached stream: {error}")))
}

/// Runs the error stage before an in-stream error frame is encoded, so
/// plugins such as PII redaction also apply after the first byte.
async fn apply_error_chain(
    chain: Option<Arc<LlmPluginChain>>,
    meta: &StreamMeta,
    account_id: Option<&str>,
    error: &mut GatewayError,
    plugin_state: &mut LlmRequestState,
) {
    let Some(chain) = chain else {
        return;
    };
    let meta = meta.clone();
    let request_meta = meta.as_meta();
    if let Err(chain_error) = chain
        .on_error(
            &request_meta,
            Some(&meta.provider_id),
            account_id,
            error,
            plugin_state,
        )
        .await
    {
        *error = crate::error::chain_error(chain_error);
    }
}

/// Upstream streaming framing: SSE for most providers, AWS event-stream for
/// Bedrock ConverseStream.
enum Framing {
    Sse(SseDecoder),
    Bedrock(octra_llm_protocol::bedrock::eventstream::Framer),
}

/// One upstream frame normalized to the `event\npayload` convention consumed
/// by the protocol decoders.
struct FramedPayload {
    event: Option<String>,
    data: String,
}

impl Framing {
    fn for_protocol(protocol: Protocol) -> Self {
        match protocol {
            Protocol::Bedrock => {
                Framing::Bedrock(octra_llm_protocol::bedrock::eventstream::Framer::new())
            }
            _ => Framing::Sse(SseDecoder::new()),
        }
    }

    fn overflowed(&self) -> bool {
        match self {
            Framing::Sse(decoder) => decoder.overflowed(),
            Framing::Bedrock(_) => false,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<Result<FramedPayload, String>> {
        match self {
            Framing::Sse(decoder) => decoder
                .push(bytes)
                .into_iter()
                .map(|event| {
                    Ok(FramedPayload {
                        event: event.event,
                        data: event.data,
                    })
                })
                .collect(),
            Framing::Bedrock(framer) => match framer.push(bytes) {
                Ok(frames) => frames
                    .into_iter()
                    .map(|frame| {
                        Ok(FramedPayload {
                            event: Some(frame.event_type),
                            data: frame.payload,
                        })
                    })
                    .collect(),
                Err(error) => vec![Err(error)],
            },
        }
    }
}

/// Replaces an empty streamed model with the client-facing model so Bedrock
/// clients never see the synthetic empty model from `messageStart`.
fn is_terminal(event: &StreamEvent) -> bool {
    matches!(
        event,
        StreamEvent::Usage(_) | StreamEvent::Finish { .. } | StreamEvent::Error(_)
    )
}

fn is_held_terminal(event: &StreamEvent) -> bool {
    matches!(event, StreamEvent::Usage(_) | StreamEvent::Finish { .. })
}

fn with_requested_model<'a>(event: &'a StreamEvent, requested_model: &str) -> Cow<'a, StreamEvent> {
    match event {
        StreamEvent::MessageStart { id, model } if model.is_empty() => {
            Cow::Owned(StreamEvent::MessageStart {
                id: id.clone(),
                model: requested_model.to_string(),
            })
        }
        _ => Cow::Borrowed(event),
    }
}

struct BridgeState {
    upstream: UpstreamResponse,
    framing: Framing,
    protocol_decoder: Box<dyn StreamDecoder + Send>,
    encoder: Box<dyn StreamEncoder + Send>,
    ingress: Protocol,
    storage: Arc<Storage>,
    tracker: UsageTracker,
    account: Option<Arc<AccountRuntime>>,
    plugins: Option<Arc<LlmPluginChain>>,
    meta: StreamMeta,
    plugin_state: LlmRequestState,
    idle: Duration,
    finished: bool,
    saw_finish: bool,
    usage: Usage,
    accumulator: StreamAccumulator,
    pending_terminal: Vec<StreamEvent>,
}

impl BridgeState {
    fn encode(&mut self, event: &StreamEvent, out: &mut Vec<u8>) {
        let requested_model = self.meta.requested_model.clone();
        let event = with_requested_model(event, &requested_model);
        if is_held_terminal(&event) {
            self.observe_terminal(&event);
            self.pending_terminal.push(event.into_owned());
            return;
        }
        self.encode_now(&event, out);
    }

    fn encode_now(&mut self, event: &StreamEvent, out: &mut Vec<u8>) {
        if let StreamEvent::Usage(usage) = event {
            self.usage = usage.clone();
        }
        if let StreamEvent::Finish { .. } = event {
            self.saw_finish = true;
        }
        self.accumulator.observe(event);
        // Usage may arrive after the finish chunk upstream, so the encoder
        // tail and the usage record both wait for stream close.
        for frame in self.encoder.encode(event).unwrap_or_default() {
            out.extend(frame.to_bytes());
        }
    }

    fn observe_terminal(&mut self, event: &StreamEvent) {
        if let StreamEvent::Usage(usage) = event {
            self.usage = usage.clone();
        }
        if let StreamEvent::Finish { .. } = event {
            self.saw_finish = true;
        }
        self.accumulator.observe(event);
    }

    async fn run_stream_end(&mut self, out: &mut Vec<u8>) {
        let Some(chain) = self.plugins.clone() else {
            return;
        };
        let Some(response) = self.accumulator.build(&self.meta.requested_model) else {
            return;
        };
        let meta = self.meta.as_meta();
        let mut appended = Vec::new();
        chain
            .on_stream_end(&meta, &response, &mut appended, &mut self.plugin_state)
            .await;
        let requested_model = self.meta.requested_model.clone();
        for event in appended {
            if is_terminal(&event) {
                continue;
            }
            let event = with_requested_model(&event, &requested_model);
            self.encode_now(&event, out);
        }
    }

    /// Stores the reconstructed response through the final-response hook.
    /// Best-effort: hook errors never fail the stream.
    async fn store_final_response(&mut self) {
        let Some(chain) = self.plugins.clone() else {
            return;
        };
        let Some(response) = self.accumulator.build(&self.meta.requested_model) else {
            return;
        };
        let meta = self.meta.as_meta();
        chain
            .on_final_response(
                &meta,
                &self.meta.requested_model,
                &response,
                &mut self.plugin_state,
            )
            .await;
    }

    /// Runs the plugin tail: decoder leftovers plus a synthetic Finish so
    /// plugins can flush buffered state even when the upstream did not end
    /// cleanly.
    async fn flush_pending(&mut self) -> Result<Vec<u8>, GatewayError> {
        let mut out = Vec::new();
        let mut events = self.protocol_decoder.finish();
        // Only synthesize a finish when the upstream never produced one. A
        // synthetic Finish after a real tool_calls/refusal finish would
        // overwrite the client-visible reason and corrupt the cached response.
        if !self.saw_finish
            && !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Finish { .. }))
        {
            events.push(StreamEvent::Finish {
                reason: octra_llm_core::FinishReason::Stop,
            });
        }
        let events = match self.plugins.clone() {
            Some(chain) => {
                let meta = self.meta.as_meta();
                chain
                    .on_stream_events(&meta, events, &mut self.plugin_state)
                    .await
                    .map_err(crate::error::chain_error)?
            }
            None => events,
        };
        for event in &events {
            self.encode(event, &mut out);
        }
        Ok(out)
    }

    /// Emits the protocol tail when the upstream closed the stream: any
    /// pending decoder events, a synthetic finish, and the encoder's own
    /// tail (usage frames and the `[DONE]` / `message_stop` terminator).
    async fn finish_stream(&mut self, _error: Option<GatewayError>) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        let mut out = match self.flush_pending().await {
            Ok(out) => out,
            Err(mut error) => {
                let account_id = self
                    .account
                    .as_ref()
                    .map(|account| account.record.id.clone());
                apply_error_chain(
                    self.plugins.clone(),
                    &self.meta,
                    account_id.as_deref(),
                    &mut error,
                    &mut self.plugin_state,
                )
                .await;
                let mut out = Vec::new();
                for frame in octra_llm_protocol::encode_stream_error(self.ingress, &error) {
                    out.extend(frame.to_bytes());
                }
                self.record(Some(error)).await;
                self.finished = true;
                return out;
            }
        };
        self.run_stream_end(&mut out).await;
        let mut terminal = std::mem::take(&mut self.pending_terminal);
        terminal.sort_by_key(|event| match event {
            StreamEvent::Usage(_) => 0,
            StreamEvent::Finish { .. } => 1,
            _ => 2,
        });
        for event in terminal {
            self.encode_now(&event, &mut out);
        }
        for frame in self.encoder.finish().unwrap_or_default() {
            out.extend(frame.to_bytes());
        }
        self.store_final_response().await;
        self.record(None).await;
        self.finished = true;
        out
    }

    /// Emits an error frame and records the failure. Buffered plugin output
    /// is flushed first so the client sees partial text before the error.
    async fn fail_stream(&mut self, mut error: GatewayError) -> Vec<u8> {
        let mut out = self.flush_pending().await.unwrap_or_default();
        let account_id = self
            .account
            .as_ref()
            .map(|account| account.record.id.clone());
        apply_error_chain(
            self.plugins.clone(),
            &self.meta,
            account_id.as_deref(),
            &mut error,
            &mut self.plugin_state,
        )
        .await;
        for frame in octra_llm_protocol::encode_stream_error(self.ingress, &error) {
            out.extend(frame.to_bytes());
        }
        self.record(Some(error)).await;
        self.finished = true;
        out
    }

    async fn record(&mut self, error: Option<GatewayError>) {
        if let Some(reason) = self.accumulator.finish_reason {
            self.tracker.record.finish_reason =
                Some(crate::usage::finish_reason_label(&reason).to_string());
        }
        record_stream(
            &self.storage,
            &self.tracker,
            &self.account,
            &self.plugins,
            &self.meta,
            &mut self.plugin_state,
            &self.usage,
            error,
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn record_stream(
    storage: &Arc<Storage>,
    tracker: &UsageTracker,
    account: &Option<Arc<AccountRuntime>>,
    plugins: &Option<Arc<LlmPluginChain>>,
    meta: &StreamMeta,
    plugin_state: &mut LlmRequestState,
    usage: &Usage,
    error: Option<GatewayError>,
) {
    let mut tracker = tracker.clone();
    tracker.record.usage = usage.clone();
    match error {
        Some(error) => {
            tracker.record.error = Some(error.message);
            tracker.finish_with_status(error.status);
        }
        None => tracker.finish_with_status(200),
    }
    if let Some(account) = account {
        match tracker.record.error {
            Some(_) => account.observe_failure(),
            None => account.observe_success(tracker.record.latency_ms.unwrap_or_default()),
        }
        let usage = &tracker.record.usage;
        let tokens = usage
            .input_tokens
            .saturating_add(usage.output_tokens)
            .min(i64::MAX as u64) as i64;
        let cost = tracker
            .record
            .pricing
            .as_ref()
            .and_then(|pricing| crate::cost::compute(usage, pricing))
            .map(|cost| cost.micros)
            .unwrap_or(0);
        account.consume(1, tokens, cost);
    }
    if let Some(chain) = plugins.clone() {
        let request_meta = meta.as_meta();
        let event = LlmLogEvent {
            model: &meta.model,
            requested_model: Some(&meta.requested_model),
            provider_id: Some(&meta.provider_id),
            provider_name: Some(&meta.provider_name),
            account_id: account.as_ref().map(|account| account.record.id.as_str()),
            account_name: account.as_ref().map(|account| account.record.name.as_str()),
            status: tracker.record.status_code.unwrap_or(200),
            latency_ms: tracker.record.latency_ms.unwrap_or_default(),
            usage: &tracker.record.usage,
            finish_reason: tracker.record.finish_reason.as_deref(),
            streaming: true,
            error: tracker.record.error.as_deref(),
            cache_status: None,
            response_source: None,
        };
        chain.on_log(&request_meta, &event, plugin_state).await;
    }
    usage::spawn_record(storage.clone(), tracker.record);
}

/// Bridges canonical events (a CLI process, a synthetic source) to SSE.
///
/// The status is already 200; failures become in-stream error events, the
/// same contract as the HTTP bridge.
#[allow(clippy::too_many_arguments)]
pub fn start_events(
    config: &OctraConfig,
    events: BoxStream<'static, Result<StreamEvent, GatewayError>>,
    ingress: Protocol,
    include_usage: bool,
    storage: Arc<Storage>,
    tracker: UsageTracker,
    account: Option<Arc<AccountRuntime>>,
    plugins: Option<Arc<LlmPluginChain>>,
    meta: StreamMeta,
    plugin_state: LlmRequestState,
) -> Response {
    let idle = Duration::from_secs(config.llm.stream_idle_timeout_secs.max(1));
    let state = EventBridgeState {
        source: events,
        encoder: octra_llm_protocol::stream_encoder_with_usage(ingress, include_usage),
        ingress,
        storage,
        tracker,
        account,
        plugins,
        meta,
        plugin_state,
        idle,
        finished: false,
        saw_finish: false,
        usage: Usage::default(),
        accumulator: StreamAccumulator::default(),
        pending_terminal: Vec::new(),
    };

    let body_stream = unfold(state, |mut state| async move {
        state
            .next_chunk()
            .await
            .map(|bytes| (Ok::<_, Infallible>(bytes), state))
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(body_stream))
        .unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot build stream response",
            )
                .into_response()
        })
}

struct EventBridgeState {
    source: BoxStream<'static, Result<StreamEvent, GatewayError>>,
    encoder: Box<dyn StreamEncoder + Send>,
    ingress: Protocol,
    storage: Arc<Storage>,
    tracker: UsageTracker,
    account: Option<Arc<AccountRuntime>>,
    plugins: Option<Arc<LlmPluginChain>>,
    meta: StreamMeta,
    plugin_state: LlmRequestState,
    idle: Duration,
    finished: bool,
    saw_finish: bool,
    usage: Usage,
    accumulator: StreamAccumulator,
    pending_terminal: Vec<StreamEvent>,
}

impl EventBridgeState {
    async fn next_chunk(&mut self) -> Option<Bytes> {
        loop {
            if self.finished {
                return None;
            }
            match tokio::time::timeout(self.idle, self.source.next()).await {
                Err(_) => {
                    let error = GatewayError::timeout("event stream idle timeout exceeded")
                        .with_code("stream_idle_timeout");
                    return Some(Bytes::from(self.fail(error).await));
                }
                Ok(None) => return Some(Bytes::from(self.finish().await)),
                Ok(Some(Err(error))) => return Some(Bytes::from(self.fail(error).await)),
                Ok(Some(Ok(StreamEvent::Error(error)))) => {
                    return Some(Bytes::from(self.fail(error).await));
                }
                Ok(Some(Ok(event))) => {
                    let mut out = Vec::new();
                    let events = vec![event];
                    let events = match self.plugins.clone() {
                        Some(chain) => {
                            let meta = self.meta.as_meta();
                            match chain
                                .on_stream_events(&meta, events, &mut self.plugin_state)
                                .await
                            {
                                Ok(events) => events,
                                Err(error) => {
                                    let error = crate::error::chain_error(error);
                                    return Some(Bytes::from(self.fail(error).await));
                                }
                            }
                        }
                        None => events,
                    };
                    for event in &events {
                        self.encode(event, &mut out);
                    }
                    if !out.is_empty() {
                        return Some(Bytes::from(out));
                    }
                }
            }
        }
    }

    fn encode(&mut self, event: &StreamEvent, out: &mut Vec<u8>) {
        let requested_model = self.meta.requested_model.clone();
        let event = with_requested_model(event, &requested_model);
        if is_held_terminal(&event) {
            self.observe_terminal(&event);
            self.pending_terminal.push(event.into_owned());
            return;
        }
        self.encode_now(&event, out);
    }

    fn encode_now(&mut self, event: &StreamEvent, out: &mut Vec<u8>) {
        if let StreamEvent::Usage(usage) = event {
            self.usage = usage.clone();
        }
        if let StreamEvent::Finish { .. } = event {
            self.saw_finish = true;
        }
        self.accumulator.observe(event);
        for frame in self.encoder.encode(event).unwrap_or_default() {
            out.extend(frame.to_bytes());
        }
    }

    fn observe_terminal(&mut self, event: &StreamEvent) {
        if let StreamEvent::Usage(usage) = event {
            self.usage = usage.clone();
        }
        if let StreamEvent::Finish { .. } = event {
            self.saw_finish = true;
        }
        self.accumulator.observe(event);
    }

    async fn run_stream_end(&mut self, out: &mut Vec<u8>) {
        let Some(chain) = self.plugins.clone() else {
            return;
        };
        let Some(response) = self.accumulator.build(&self.meta.requested_model) else {
            return;
        };
        let meta = self.meta.as_meta();
        let mut appended = Vec::new();
        chain
            .on_stream_end(&meta, &response, &mut appended, &mut self.plugin_state)
            .await;
        let requested_model = self.meta.requested_model.clone();
        for event in appended {
            if is_terminal(&event) {
                continue;
            }
            let event = with_requested_model(&event, &requested_model);
            self.encode_now(&event, out);
        }
    }

    /// Stores the reconstructed response through the final-response hook.
    /// Best-effort: hook errors never fail the stream.
    async fn store_final_response(&mut self) {
        let Some(chain) = self.plugins.clone() else {
            return;
        };
        let Some(response) = self.accumulator.build(&self.meta.requested_model) else {
            return;
        };
        let meta = self.meta.as_meta();
        chain
            .on_final_response(
                &meta,
                &self.meta.requested_model,
                &response,
                &mut self.plugin_state,
            )
            .await;
    }

    async fn finish(&mut self) -> Vec<u8> {
        self.finished = true;
        let mut out = Vec::new();
        if !self.saw_finish {
            let events = vec![StreamEvent::Finish {
                reason: octra_llm_core::FinishReason::Stop,
            }];
            let events = match self.plugins.clone() {
                Some(chain) => {
                    let meta = self.meta.as_meta();
                    match chain
                        .on_stream_events(&meta, events, &mut self.plugin_state)
                        .await
                    {
                        Ok(events) => events,
                        Err(error) => {
                            let error = crate::error::chain_error(error);
                            let out = self.fail(error).await;
                            return out;
                        }
                    }
                }
                None => events,
            };
            for event in &events {
                self.encode(event, &mut out);
            }
        }
        self.run_stream_end(&mut out).await;
        let mut terminal = std::mem::take(&mut self.pending_terminal);
        terminal.sort_by_key(|event| match event {
            StreamEvent::Usage(_) => 0,
            StreamEvent::Finish { .. } => 1,
            _ => 2,
        });
        for event in terminal {
            self.encode_now(&event, &mut out);
        }
        for frame in self.encoder.finish().unwrap_or_default() {
            out.extend(frame.to_bytes());
        }
        self.store_final_response().await;
        self.record(None).await;
        out
    }

    async fn fail(&mut self, mut error: GatewayError) -> Vec<u8> {
        self.finished = true;
        let mut out = Vec::new();
        let account_id = self
            .account
            .as_ref()
            .map(|account| account.record.id.clone());
        apply_error_chain(
            self.plugins.clone(),
            &self.meta,
            account_id.as_deref(),
            &mut error,
            &mut self.plugin_state,
        )
        .await;
        for frame in octra_llm_protocol::encode_stream_error(self.ingress, &error) {
            out.extend(frame.to_bytes());
        }
        self.record(Some(error)).await;
        out
    }

    async fn record(&mut self, error: Option<GatewayError>) {
        if let Some(reason) = self.accumulator.finish_reason {
            self.tracker.record.finish_reason =
                Some(crate::usage::finish_reason_label(&reason).to_string());
        }
        record_stream(
            &self.storage,
            &self.tracker,
            &self.account,
            &self.plugins,
            &self.meta,
            &mut self.plugin_state,
            &self.usage,
            error,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    fn start(id: &str, model: &str) -> StreamEvent {
        StreamEvent::MessageStart {
            id: id.to_string(),
            model: model.to_string(),
        }
    }

    #[test]
    fn accumulates_text_reasoning_usage_and_finish() {
        let mut accumulator = StreamAccumulator::default();
        for event in [
            start("chatcmpl-1", "provider/model"),
            StreamEvent::ReasoningDelta {
                text: "thinking".to_string(),
            },
            StreamEvent::ReasoningSignatureDelta {
                signature: "sig-1".to_string(),
            },
            StreamEvent::TextDelta {
                text: "hello ".to_string(),
            },
            StreamEvent::TextDelta {
                text: "world".to_string(),
            },
            StreamEvent::Usage(Usage {
                input_tokens: 3,
                output_tokens: 4,
                ..Default::default()
            }),
            StreamEvent::Finish {
                reason: FinishReason::Stop,
            },
        ] {
            accumulator.observe(&event);
        }

        let response = accumulator
            .build("requested-model")
            .expect("finished stream");
        assert_eq!(response.id, "chatcmpl-1");
        assert_eq!(response.model, "requested-model");
        assert_eq!(response.finish_reason, FinishReason::Stop);
        assert_eq!(response.usage.input_tokens, 3);
        assert_eq!(response.usage.output_tokens, 4);
        assert_eq!(
            response.message.assistant,
            vec![
                AssistantContent::Reasoning {
                    text: "thinking".to_string(),
                    signature: Some("sig-1".to_string()),
                },
                AssistantContent::Text {
                    text: "hello world".to_string(),
                },
            ]
        );
        let reasoning = response.reasoning.expect("reasoning summary");
        assert_eq!(reasoning.text, "thinking");
        assert_eq!(reasoning.signature.as_deref(), Some("sig-1"));
    }

    #[test]
    fn accumulates_tool_calls_by_index_and_parses_arguments() {
        let mut accumulator = StreamAccumulator::default();
        for event in [
            start("chatcmpl-tool", "provider/model"),
            StreamEvent::ToolCallStart {
                index: 1,
                id: "call-b".to_string(),
                name: "second".to_string(),
                kind: ToolCallKind::Function,
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                arguments: "{\"b\":".to_string(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                arguments: "2}".to_string(),
            },
            StreamEvent::ToolCallStart {
                index: 0,
                id: "call-a".to_string(),
                name: "first".to_string(),
                kind: ToolCallKind::Function,
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                arguments: "{\"a\":1}".to_string(),
            },
            StreamEvent::Finish {
                reason: FinishReason::ToolCalls,
            },
        ] {
            accumulator.observe(&event);
        }

        let response = accumulator.build("m").expect("finished stream");
        let calls: Vec<&AssistantContent> = response
            .message
            .assistant
            .iter()
            .filter(|block| matches!(block, AssistantContent::ToolCall { .. }))
            .collect();
        assert_eq!(calls.len(), 2);
        match calls[0] {
            AssistantContent::ToolCall { id, arguments, .. } => {
                assert_eq!(id, "call-a");
                assert_eq!(arguments, &serde_json::json!({"a": 1}));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
        match calls[1] {
            AssistantContent::ToolCall { id, arguments, .. } => {
                assert_eq!(id, "call-b");
                assert_eq!(arguments, &serde_json::json!({"b": 2}));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn invalid_tool_arguments_are_kept_as_text() {
        let mut accumulator = StreamAccumulator::default();
        for event in [
            start("chatcmpl-tool", "provider/model"),
            StreamEvent::ToolCallStart {
                index: 0,
                id: "call-a".to_string(),
                name: "broken".to_string(),
                kind: ToolCallKind::Function,
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                arguments: "not json".to_string(),
            },
            StreamEvent::Finish {
                reason: FinishReason::ToolCalls,
            },
        ] {
            accumulator.observe(&event);
        }

        let response = accumulator.build("m").expect("finished stream");
        match &response.message.assistant[0] {
            AssistantContent::ToolCall { arguments, .. } => {
                assert_eq!(arguments, &Value::String("not json".to_string()));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn accumulates_provider_tool_results() {
        let mut accumulator = StreamAccumulator::default();
        for event in [
            start("msg-1", "provider/model"),
            StreamEvent::ProviderToolStart {
                index: 0,
                tool_type: "web_search".to_string(),
            },
            StreamEvent::ProviderToolResult {
                index: 0,
                tool_use_id: "srvtoolu_1".to_string(),
                content: serde_json::json!({"type": "web_search_tool_result", "block": {}}),
                is_error: false,
            },
            StreamEvent::Finish {
                reason: FinishReason::Stop,
            },
        ] {
            accumulator.observe(&event);
        }

        let response = accumulator.build("m").expect("finished stream");
        assert_eq!(
            response.message.assistant,
            vec![AssistantContent::ToolResult {
                tool_use_id: "srvtoolu_1".to_string(),
                content: serde_json::json!({"type": "web_search_tool_result", "block": {}}),
                is_error: false,
            }]
        );
    }

    #[test]
    fn sse_framing_keeps_event_and_data() {
        let mut framing = Framing::for_protocol(Protocol::Anthropic);
        let frames = framing.push(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        assert_eq!(frames.len(), 1);
        let frame = frames.into_iter().next().unwrap().unwrap();
        assert_eq!(frame.event.as_deref(), Some("message_stop"));
        assert_eq!(frame.data, r#"{"type":"message_stop"}"#);
    }

    #[test]
    fn bedrock_framing_yields_event_and_payload() {
        let mut framing = Framing::for_protocol(Protocol::Bedrock);
        let frames = framing.push(&bedrock_frame(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"hi"}}"#,
        ));
        assert_eq!(frames.len(), 1);
        let frame = frames.into_iter().next().unwrap().unwrap();
        assert_eq!(frame.event.as_deref(), Some("contentBlockDelta"));
        assert_eq!(
            frame.data,
            r#"{"contentBlockIndex":0,"delta":{"text":"hi"}}"#
        );
    }

    fn bedrock_frame(event: &str, payload: &str) -> Vec<u8> {
        let mut headers = Vec::new();
        headers.push(":event-type".len() as u8);
        headers.extend_from_slice(b":event-type");
        headers.push(7);
        headers.extend_from_slice(&(event.len() as u16).to_be_bytes());
        headers.extend_from_slice(event.as_bytes());

        let total = 12 + headers.len() + payload.len() + 4;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        let prelude = crc32(&out);
        out.extend_from_slice(&prelude.to_be_bytes());
        out.extend_from_slice(&headers);
        out.extend_from_slice(payload.as_bytes());
        let message = crc32(&out);
        out.extend_from_slice(&message.to_be_bytes());
        out
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn empty_message_start_uses_the_requested_model() {
        let event = StreamEvent::MessageStart {
            id: "bedrock".to_string(),
            model: String::new(),
        };
        let patched = with_requested_model(&event, "client-model");
        assert_eq!(
            patched.as_ref(),
            &StreamEvent::MessageStart {
                id: "bedrock".to_string(),
                model: "client-model".to_string(),
            }
        );

        let text = StreamEvent::TextDelta {
            text: "hi".to_string(),
        };
        assert_eq!(with_requested_model(&text, "client-model").as_ref(), &text);
    }

    #[test]
    fn failed_or_unfinished_streams_are_not_built() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.observe(&start("chatcmpl-1", "provider/model"));
        accumulator.observe(&StreamEvent::TextDelta {
            text: "partial".to_string(),
        });
        assert!(accumulator.build("m").is_none(), "no finish observed");

        accumulator.observe(&StreamEvent::Error(GatewayError::internal("boom")));
        accumulator.observe(&StreamEvent::Finish {
            reason: FinishReason::Stop,
        });
        assert!(accumulator.build("m").is_none(), "errored stream");
    }

    #[tokio::test]
    async fn cached_text_response_encodes_to_sse() {
        let mut message = Message::new(Role::Assistant);
        message.assistant.push(AssistantContent::Text {
            text: "cached answer".to_string(),
        });
        let response = ChatResponse::new("chatcmpl-cached", "m", message);

        let encoded = cached_response(&response, Protocol::Openai, false).expect("encode");
        assert_eq!(encoded.status(), StatusCode::OK);
        assert_eq!(
            encoded.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(
            encoded.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        let bytes = to_bytes(encoded.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("cached answer"));
        assert!(text.trim_end().ends_with("data: [DONE]"));
    }
}
