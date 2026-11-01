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
use futures_util::stream::unfold;
use octra_config::OctraConfig;
use octra_data_plane::sse::SseDecoder;
use octra_llm_core::{GatewayError, Protocol, StreamEvent, Usage};
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
                    let out = state.finish_stream(None);
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
                    let out = state.fail_stream(error);
                    state.finished = true;
                    return Some((Ok(Bytes::from(out)), state));
                }
                Err(_) => {
                    let error = GatewayError::timeout("upstream stream idle timeout exceeded")
                        .with_code("stream_idle_timeout");
                    let out = state.fail_stream(error);
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
                match state.protocol_decoder.decode_chunk(&payload) {
                    Ok(events) => {
                        for event in events {
                            state.encode(&event, &mut out);
                        }
                    }
                    Err(error) => {
                        failed = Some(
                            GatewayError::upstream(502, error.message)
                                .with_code("upstream_stream_error"),
                        );
                        break;
                    }
                }
            }
            if let Some(error) = failed {
                out.extend(state.fail_stream(error));
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

struct BridgeState {
    upstream: UpstreamResponse,
    decoder: SseDecoder,
    protocol_decoder: Box<dyn StreamDecoder + Send>,
    encoder: Box<dyn StreamEncoder + Send>,
    ingress: Protocol,
    storage: Arc<Storage>,
    tracker: UsageTracker,
    account: Option<Arc<AccountRuntime>>,
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

    /// Emits the protocol tail when the upstream closed the stream: any
    /// pending decoder events, a synthetic finish, and the encoder's own
    /// tail (usage frames and the `[DONE]` / `message_stop` terminator).
    fn finish_stream(&mut self, _error: Option<GatewayError>) -> Vec<u8> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        let decoder_events = self.protocol_decoder.finish();
        for event in decoder_events {
            self.encode(&event, &mut out);
        }
        let saw_finish = self.saw_finish;
        if !saw_finish {
            let finish = StreamEvent::Finish {
                reason: octra_llm_core::FinishReason::Stop,
            };
            self.encode(&finish, &mut out);
        }
        // Forward the tail usage before the encoder's own terminator so the
        // encoder buffers it for the final usage frame.
        if !self.usage.is_empty() {
            self.encode(&StreamEvent::Usage(self.usage.clone()), &mut out);
        }
        for frame in self.encoder.finish().unwrap_or_default() {
            out.extend(frame.to_bytes());
        }
        self.record(None);
        self.finished = true;
        out
    }

    /// Emits an error frame and records the failure.
    fn fail_stream(&mut self, error: GatewayError) -> Vec<u8> {
        let frames = octra_llm_protocol::encode_stream_error(self.ingress, &error);
        self.record(Some(error));
        self.finished = true;
        frames
            .into_iter()
            .flat_map(|frame| frame.to_bytes())
            .collect()
    }

    fn record(&mut self, error: Option<GatewayError>) {
        let mut tracker = self.tracker.clone();
        tracker.record.usage = self.usage.clone();
        match error {
            Some(error) => {
                tracker.record.error = Some(error.message);
                tracker.finish_with_status(error.status);
            }
            None => tracker.finish_with_status(200),
        }
        if let Some(account) = &self.account {
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
        usage::spawn_record(self.storage.clone(), tracker.record);
    }
}
