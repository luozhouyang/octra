//! Server-Sent Events bridging between upstream and ingress protocols.
//!
//! Upstream chunks are decoded into canonical events with the protocol
//! adapters (fed by the shared `octra_data_plane::sse::SseDecoder`) and
//! re-encoded for the ingress protocol, so cross-protocol streaming shares
//! the same codecs as the rest of the gateway.

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
use octra_llm_core::{GatewayError, Protocol, StreamEvent, Usage};
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
        decoder: SseDecoder::new(),
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
            for event in state.decoder.push(&bytes) {
                let payload = match event.event {
                    Some(name) => format!("{name}\n{}", event.data),
                    None => event.data,
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
pub struct StreamMeta {
    pub request_id: String,
    pub model: String,
    pub provider_id: String,
    pub provider_name: String,
    pub ingress: Protocol,
    pub consumer_id: Option<String>,
    pub virtual_key_id: Option<String>,
    pub session_id: Option<String>,
    pub client_ip: Option<std::net::IpAddr>,
    pub user_agent: Option<String>,
    pub intent: Option<String>,
}

impl StreamMeta {
    fn as_meta(&self) -> LlmRequestMeta<'_> {
        LlmRequestMeta {
            request_id: &self.request_id,
            ingress: self.ingress,
            consumer_id: self.consumer_id.as_deref(),
            virtual_key_id: self.virtual_key_id.as_deref(),
            session_id: self.session_id.as_deref(),
            client_ip: self.client_ip,
            user_agent: self.user_agent.as_deref(),
            intent: self.intent.as_deref(),
        }
    }
}

struct BridgeState {
    upstream: UpstreamResponse,
    decoder: SseDecoder,
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
}

impl BridgeState {
    fn encode(&mut self, event: &StreamEvent, out: &mut Vec<u8>) {
        if let StreamEvent::Usage(usage) = event {
            self.usage = usage.clone();
        }
        if let StreamEvent::Finish { .. } = event {
            self.saw_finish = true;
        }
        // Usage may arrive after the finish chunk upstream, so the encoder
        // tail and the usage record both wait for stream close.
        for frame in self.encoder.encode(event).unwrap_or_default() {
            out.extend(frame.to_bytes());
        }
    }

    /// Runs the plugin tail: decoder leftovers plus a synthetic Finish so
    /// plugins can flush buffered state even when the upstream did not end
    /// cleanly.
    async fn flush_pending(&mut self) -> Result<Vec<u8>, GatewayError> {
        let mut out = Vec::new();
        let mut events = self.protocol_decoder.finish();
        if !events
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
            Err(error) => {
                let mut out = Vec::new();
                for frame in octra_llm_protocol::encode_stream_error(self.ingress, &error) {
                    out.extend(frame.to_bytes());
                }
                self.record(Some(error)).await;
                self.finished = true;
                return out;
            }
        };
        if !self.usage.is_empty() {
            self.encode(&StreamEvent::Usage(self.usage.clone()), &mut out);
        }
        for frame in self.encoder.finish().unwrap_or_default() {
            out.extend(frame.to_bytes());
        }
        self.record(None).await;
        self.finished = true;
        out
    }

    /// Emits an error frame and records the failure. Buffered plugin output
    /// is flushed first so the client sees partial text before the error.
    async fn fail_stream(&mut self, error: GatewayError) -> Vec<u8> {
        let mut out = self.flush_pending().await.unwrap_or_default();
        for frame in octra_llm_protocol::encode_stream_error(self.ingress, &error) {
            out.extend(frame.to_bytes());
        }
        self.record(Some(error)).await;
        self.finished = true;
        out
    }

    async fn record(&mut self, error: Option<GatewayError>) {
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
        if let StreamEvent::Usage(usage) = event {
            self.usage = usage.clone();
        }
        if let StreamEvent::Finish { .. } = event {
            self.saw_finish = true;
        }
        for frame in self.encoder.encode(event).unwrap_or_default() {
            out.extend(frame.to_bytes());
        }
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
        if !self.usage.is_empty() {
            self.encode(&StreamEvent::Usage(self.usage.clone()), &mut out);
        }
        for frame in self.encoder.finish().unwrap_or_default() {
            out.extend(frame.to_bytes());
        }
        self.record(None).await;
        out
    }

    async fn fail(&mut self, error: GatewayError) -> Vec<u8> {
        self.finished = true;
        let mut out = Vec::new();
        for frame in octra_llm_protocol::encode_stream_error(self.ingress, &error) {
            out.extend(frame.to_bytes());
        }
        self.record(Some(error)).await;
        out
    }

    async fn record(&mut self, error: Option<GatewayError>) {
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
