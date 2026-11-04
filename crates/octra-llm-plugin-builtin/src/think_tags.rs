//! Strips `<think>...</think>` blocks from streamed text.
//!
//! A provider may emit reasoning inline in the text stream. The plugin keeps
//! a small buffer so tags split across chunks are still removed, and drops
//! the enclosed reasoning when `strip` is enabled. The tag pair is
//! configurable, so providers that use `<thinking>` or a custom marker can use
//! the same plugin.

use std::sync::Arc;

use async_trait::async_trait;
use octra_api_plugin_types::{Capability, PluginError, PluginManifest};
use octra_llm_core::{AssistantContent, StreamEvent};
use octra_llm_plugin_types::{
    LlmAction, LlmPlugin, LlmPluginFactory, LlmPluginResult, LlmResponseContext, LlmStreamAction,
    LlmStreamContext, LlmStreamResult,
};
use serde_json::{Value, json};

use crate::manifest;

const DEFAULT_OPEN: &str = "<think>";
const DEFAULT_CLOSE: &str = "</think>";

#[derive(Debug)]
pub struct ThinkTags {
    manifest: PluginManifest,
    strip: bool,
    open: String,
    close: String,
}

pub struct ThinkTagsFactory;

impl LlmPluginFactory for ThinkTagsFactory {
    fn manifest(&self) -> PluginManifest {
        let mut manifest = manifest(
            "llm-think-tags",
            "Remove inline <think> reasoning blocks from streamed text",
        );
        manifest.config_schema = Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "llm-think-tags config",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "strip": {
                    "type": "boolean",
                    "default": true
                },
                "open": {
                    "type": "string",
                    "minLength": 1,
                    "default": "<think>",
                    "description": "Opening tag; must differ from `close`."
                },
                "close": {
                    "type": "string",
                    "minLength": 1,
                    "default": "</think>",
                    "description": "Closing tag; must differ from `open`."
                }
            }
        }));
        manifest.capabilities = vec![Capability::ResponseBody];
        manifest
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        if let Some(strip) = config.get("strip")
            && !strip.is_boolean()
        {
            return Err(PluginError::fail_closed(
                "llm-think-tags `strip` must be a boolean",
            ));
        }
        let open = tag(&config, "open", DEFAULT_OPEN)?;
        let close = tag(&config, "close", DEFAULT_CLOSE)?;
        if open == close {
            return Err(PluginError::fail_closed(
                "llm-think-tags `open` and `close` must differ",
            ));
        }
        Ok(Arc::new(ThinkTags {
            manifest: self.manifest(),
            strip: config.get("strip").and_then(Value::as_bool).unwrap_or(true),
            open,
            close,
        }))
    }
}

fn tag(config: &Value, key: &str, default: &str) -> Result<String, PluginError> {
    match config.get(key) {
        None => Ok(default.to_string()),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
        Some(_) => Err(PluginError::fail_closed(format!(
            "llm-think-tags `{key}` must be a non-empty string"
        ))),
    }
}

#[async_trait]
impl LlmPlugin for ThinkTags {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Non-streaming responses use the same tag grammar as the stream path.
    async fn on_response(&self, cx: &mut LlmResponseContext<'_>) -> LlmPluginResult {
        if !self.strip {
            return Ok(LlmAction::Continue);
        }
        for block in &mut cx.response.message.assistant {
            match block {
                AssistantContent::Text { text } | AssistantContent::Refusal { text } => {
                    *text = strip_text(text, &self.open, &self.close);
                }
                _ => {}
            }
        }
        Ok(LlmAction::Continue)
    }

    async fn on_stream_event(&self, cx: &mut LlmStreamContext<'_>) -> LlmStreamResult {
        if !self.strip {
            return Ok(LlmStreamAction::Continue);
        }
        let mut buffer = state_string(cx.state, "buffer");
        let mut in_think = state_bool(cx.state, "in_think");

        let action = match cx.event {
            StreamEvent::TextDelta { text } => {
                buffer.push_str(text);
                let mut out = String::new();
                drain(
                    &mut buffer,
                    &mut in_think,
                    &mut out,
                    &self.open,
                    &self.close,
                );
                if out.is_empty() {
                    LlmStreamAction::Replace(Vec::new())
                } else {
                    LlmStreamAction::Replace(vec![StreamEvent::TextDelta { text: out }])
                }
            }
            StreamEvent::Finish { reason } => {
                if in_think {
                    tracing::warn!(
                        request_id = cx.meta.request_id,
                        "llm-think-tags reached the end of a stream inside an unclosed think block; dropping the remaining reasoning"
                    );
                    buffer.clear();
                    LlmStreamAction::Replace(vec![StreamEvent::Finish { reason: *reason }])
                } else if !buffer.is_empty() {
                    let text = std::mem::take(&mut buffer);
                    LlmStreamAction::Replace(vec![
                        StreamEvent::TextDelta { text },
                        StreamEvent::Finish { reason: *reason },
                    ])
                } else {
                    LlmStreamAction::Replace(vec![StreamEvent::Finish { reason: *reason }])
                }
            }
            _ => LlmStreamAction::Continue,
        };

        cx.state["buffer"] = json!(buffer);
        cx.state["in_think"] = json!(in_think);
        Ok(action)
    }
}

fn state_string(state: &Value, key: &str) -> String {
    state
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn state_bool(state: &Value, key: &str) -> bool {
    state.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Moves outside-text into `out`, keeping only a possible partial tag at the
/// end of the buffer. Text inside a think block is dropped.
fn drain(buffer: &mut String, in_think: &mut bool, out: &mut String, open: &str, close: &str) {
    loop {
        if *in_think {
            if let Some(index) = buffer.find(close) {
                buffer.drain(..index + close.len());
                *in_think = false;
            } else {
                // Drop the reasoning but keep a partial closing tag.
                let keep = longest_suffix_prefix(buffer, close);
                buffer.drain(..buffer.len() - keep);
                break;
            }
        } else if let Some(index) = buffer.find(open) {
            out.push_str(&buffer[..index]);
            buffer.drain(..index + open.len());
            *in_think = true;
        } else {
            let keep = longest_suffix_prefix(buffer, open);
            let emit = buffer.len() - keep;
            out.push_str(&buffer[..emit]);
            buffer.drain(..emit);
            break;
        }
    }
}

fn strip_text(text: &str, open: &str, close: &str) -> String {
    let mut buffer = text.to_string();
    let mut in_think = false;
    let mut out = String::new();
    drain(&mut buffer, &mut in_think, &mut out, open, close);
    out
}

fn longest_suffix_prefix(value: &str, tag: &str) -> usize {
    let max = tag.len().min(value.len());
    (0..=max)
        .rev()
        .filter(|&length| tag.is_char_boundary(length))
        .find(|&length| value.ends_with(&tag[..length]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{ChatResponse, FinishReason, Message, Protocol, Role, Usage};
    use octra_llm_plugin_types::LlmRequestMeta;

    struct Harness {
        plugin: Arc<dyn LlmPlugin>,
        state: Value,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                plugin: ThinkTagsFactory.create(json!({})).unwrap(),
                state: Value::Null,
            }
        }

        async fn event(&mut self, mut event: StreamEvent) -> Vec<StreamEvent> {
            let meta = LlmRequestMeta {
                request_id: "r",
                ingress: Protocol::Openai,
                consumer_id: None,
                virtual_key_id: None,
                virtual_key_family: None,
                consumer_group_ids: &[],
                session_id: None,
                client_ip: None,
                user_agent: None,
                intent: None,
            };
            let mut cx = LlmStreamContext {
                meta: &meta,
                event: &mut event,
                state: &mut self.state,
            };
            match self.plugin.on_stream_event(&mut cx).await.unwrap() {
                LlmStreamAction::Continue => vec![event],
                LlmStreamAction::Replace(events) => events,
                LlmStreamAction::Reject(_) => Vec::new(),
            }
        }
    }

    fn text(events: Vec<StreamEvent>) -> String {
        events
            .into_iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta { text } => Some(text),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn strips_a_complete_block() {
        let mut harness = Harness::new();
        let out = harness
            .event(StreamEvent::TextDelta {
                text: "a<think>b</think>c".into(),
            })
            .await;
        assert_eq!(text(out), "ac");
    }

    #[tokio::test]
    async fn handles_tags_split_across_chunks() {
        let mut harness = Harness::new();
        let mut out = String::new();
        for chunk in ["hello <thi", "nk>secret</thi", "nk> world"] {
            out.push_str(&text(
                harness
                    .event(StreamEvent::TextDelta { text: chunk.into() })
                    .await,
            ));
        }
        assert_eq!(out, "hello  world");
    }

    #[tokio::test]
    async fn flushes_buffered_text_on_finish() {
        let mut harness = Harness::new();
        let mut out = String::new();
        for chunk in ["hi <thi", "nk>secret</think> ta"] {
            out.push_str(&text(
                harness
                    .event(StreamEvent::TextDelta { text: chunk.into() })
                    .await,
            ));
        }
        assert_eq!(out, "hi  ta");
        let finish = harness
            .event(StreamEvent::Finish {
                reason: FinishReason::Stop,
            })
            .await;
        assert!(matches!(finish[0], StreamEvent::Finish { .. }));
    }

    #[tokio::test]
    async fn strip_false_is_a_no_op() {
        let plugin = ThinkTagsFactory.create(json!({"strip": false})).unwrap();
        let mut state = Value::Null;
        let mut event = StreamEvent::TextDelta {
            text: "<think>x</think>".into(),
        };
        let meta = LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        };
        let mut cx = LlmStreamContext {
            meta: &meta,
            event: &mut event,
            state: &mut state,
        };
        assert!(matches!(
            plugin.on_stream_event(&mut cx).await.unwrap(),
            LlmStreamAction::Continue
        ));
    }

    #[tokio::test]
    async fn strips_non_streaming_text() {
        let plugin = ThinkTagsFactory.create(json!({})).unwrap();
        let meta = LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        };
        let mut response = ChatResponse::new("id", "m", Message::new(Role::Assistant));
        response.message.assistant = vec![AssistantContent::Text {
            text: "visible<think>secret</think>".into(),
        }];
        let mut usage = Usage::default();
        let mut cx = LlmResponseContext {
            meta: &meta,
            request_model: "m",
            response: &mut response,
            usage: &mut usage,
            state: &mut Value::Null,
        };
        plugin.on_response(&mut cx).await.unwrap();
        match &response.message.assistant[0] {
            AssistantContent::Text { text } => assert_eq!(text, "visible"),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn supports_custom_tag_pairs() {
        let plugin = ThinkTagsFactory
            .create(json!({"open": "<thinking>", "close": "</thinking>"}))
            .unwrap();
        let mut state = Value::Null;
        let mut event = StreamEvent::TextDelta {
            text: "a<thinking>b</thinking>c".into(),
        };
        let meta = LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        };
        let mut cx = LlmStreamContext {
            meta: &meta,
            event: &mut event,
            state: &mut state,
        };
        match plugin.on_stream_event(&mut cx).await.unwrap() {
            LlmStreamAction::Replace(events) => assert_eq!(text(events), "ac"),
            _ => panic!("expected replaced text"),
        }
    }

    #[tokio::test]
    async fn unclosed_block_is_dropped_without_leaking_text() {
        let mut harness = Harness::new();
        let out = harness
            .event(StreamEvent::TextDelta {
                text: "visible<think>secret".into(),
            })
            .await;
        assert_eq!(text(out), "visible");
        let finish = harness
            .event(StreamEvent::Finish {
                reason: FinishReason::Stop,
            })
            .await;
        assert!(matches!(finish[0], StreamEvent::Finish { .. }));
    }
}
