//! Protocol adapters between wire formats and the canonical LLM IR.
//!
//! Each adapter converts requests, responses and streamed chunks in both
//! directions. Adapters are pure functions over JSON and canonical types; all
//! I/O lives in the gateway crate.
//!
//! Mappings are best-effort and documented per adapter. Fields without a
//! canonical meaning are preserved in `extra` maps for same-protocol traffic
//! and approximated when crossing protocols.

pub mod anthropic;
pub mod common;
pub mod gemini;
pub mod openai;
pub mod openai_responses;

pub use common::{
    SseFrame, StreamCodec, UpstreamProtocol, parse_sse_json, sse_data_frame, sse_event_frame,
};

use octra_llm_core::{
    ChatRequest, ChatResponse, GatewayError, Protocol, ProtocolResult, StreamEvent,
};

/// Translates one upstream SSE chunk into canonical events.
pub trait StreamDecoder {
    fn decode_chunk(&mut self, data: &str) -> ProtocolResult<Vec<StreamEvent>>;
    /// Emitted when the upstream closes the stream without a finish signal.
    fn finish(&mut self) -> Vec<StreamEvent> {
        Vec::new()
    }
}

/// Encodes canonical events as ingress SSE frames.
pub trait StreamEncoder {
    fn encode(&mut self, event: &StreamEvent) -> ProtocolResult<Vec<SseFrame>>;
    /// Emitted when the canonical stream ends without a finish event.
    fn finish(&mut self) -> ProtocolResult<Vec<SseFrame>>;
}

pub fn decode_request(protocol: Protocol, value: serde_json::Value) -> ProtocolResult<ChatRequest> {
    match protocol {
        Protocol::Openai => openai::request::decode_request(value),
        Protocol::OpenaiResponses => openai_responses::request::decode_request(value),
        Protocol::Anthropic => anthropic::request::decode_request(value),
        Protocol::Gemini => gemini::request::decode_request(value),
    }
}

pub fn encode_request(
    protocol: Protocol,
    request: &ChatRequest,
) -> ProtocolResult<serde_json::Value> {
    match protocol {
        Protocol::Openai => openai::request::encode_request(request),
        Protocol::OpenaiResponses => openai_responses::request::encode_request(request),
        Protocol::Anthropic => anthropic::request::encode_request(request),
        Protocol::Gemini => gemini::request::encode_request(request),
    }
}

pub fn decode_response(
    protocol: Protocol,
    value: serde_json::Value,
) -> ProtocolResult<ChatResponse> {
    match protocol {
        Protocol::Openai => openai::response::decode_response(value),
        Protocol::OpenaiResponses => openai_responses::response::decode_response(value),
        Protocol::Anthropic => anthropic::response::decode_response(value),
        Protocol::Gemini => gemini::response::decode_response(value),
    }
}

pub fn encode_response(
    protocol: Protocol,
    response: &ChatResponse,
) -> ProtocolResult<serde_json::Value> {
    match protocol {
        Protocol::Openai => openai::response::encode_response(response),
        Protocol::OpenaiResponses => openai_responses::response::encode_response(response),
        Protocol::Anthropic => anthropic::response::encode_response(response),
        Protocol::Gemini => gemini::response::encode_response(response),
    }
}

#[allow(clippy::type_complexity)]
pub fn stream_decoder(protocol: Protocol) -> Box<dyn StreamDecoder + Send> {
    match protocol {
        Protocol::Openai => Box::new(openai::stream::Decoder::new()),
        Protocol::OpenaiResponses => Box::new(openai_responses::stream::Decoder::new()),
        Protocol::Anthropic => Box::new(anthropic::stream::Decoder::new()),
        Protocol::Gemini => Box::new(gemini::stream::Decoder::new()),
    }
}

#[allow(clippy::type_complexity)]
pub fn stream_encoder(protocol: Protocol) -> Box<dyn StreamEncoder + Send> {
    stream_encoder_with_usage(protocol, false)
}

/// Encoder that emits token usage in the stream. OpenAI only includes usage
/// when the client opted in with `stream_options.include_usage`; Anthropic
/// always reports it, so the flag is ignored there.
#[allow(clippy::type_complexity)]
pub fn stream_encoder_with_usage(
    protocol: Protocol,
    include_usage: bool,
) -> Box<dyn StreamEncoder + Send> {
    match protocol {
        Protocol::Openai => Box::new(openai::stream::Encoder::new().with_usage(include_usage)),
        Protocol::OpenaiResponses => Box::new(openai_responses::stream::Encoder::new()),
        Protocol::Anthropic => Box::new(anthropic::stream::Encoder::new()),
        Protocol::Gemini => Box::new(gemini::stream::Encoder::new()),
    }
}

/// Renders an error in the protocol's non-streaming error shape.
pub fn encode_error(protocol: Protocol, error: &GatewayError) -> serde_json::Value {
    match protocol {
        Protocol::Openai => openai::response::error_body(error),
        Protocol::OpenaiResponses => openai_responses::response::error_body(error),
        Protocol::Anthropic => anthropic::response::error_body(error),
        Protocol::Gemini => gemini::response::error_body(error),
    }
}

/// Renders an error as the terminal SSE frame(s) of a stream.
pub fn encode_stream_error(protocol: Protocol, error: &GatewayError) -> Vec<SseFrame> {
    match protocol {
        Protocol::Openai => openai::response::error_frames(error),
        Protocol::OpenaiResponses => openai_responses::response::error_frames(error),
        Protocol::Anthropic => anthropic::response::error_frames(error),
        Protocol::Gemini => gemini::response::error_frames(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatchers_route_to_the_right_adapter() {
        let openai = serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let req = decode_request(Protocol::Openai, openai).unwrap();
        assert_eq!(req.model, "gpt-4o");

        let anthropic = serde_json::json!({
            "model": "claude-3",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        });
        let req = decode_request(Protocol::Anthropic, anthropic).unwrap();
        assert_eq!(req.max_tokens, Some(16));
    }
}
