//! Strips `<think>...</think>` blocks from streamed text.
//!
//! A provider may emit reasoning inline in the text stream. The plugin keeps
//! a small buffer so tags split across chunks are still removed, and drops
//! the enclosed reasoning when `strip` is enabled.

use std::sync::Arc;

use async_trait::async_trait;
use octra_llm_core::StreamEvent;
use octra_llm_plugin_api::{
    LlmPlugin, LlmPluginFactory, LlmStreamAction, LlmStreamContext, LlmStreamResult,
};
use octra_plugin_api::{PluginError, PluginManifest};
use serde_json::{Value, json};

use crate::manifest;

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

#[derive(Debug)]
pub struct ThinkTags {
    manifest: PluginManifest,
    strip: bool,
}

pub struct ThinkTagsFactory;

impl LlmPluginFactory for ThinkTagsFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "llm-think-tags",
            "Remove inline <think> reasoning blocks from streamed text",
        )
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        if let Some(strip) = config.get("strip")
            && !strip.is_boolean()
        {
            return Err(PluginError::fail_closed(
                "llm-think-tags `strip` must be a boolean",
            ));
        }
        Ok(Arc::new(ThinkTags {
            manifest: self.manifest(),
            strip: config.get("strip").and_then(Value::as_bool).unwrap_or(true),
        }))
    }
}

#[async_trait]
impl LlmPlugin for ThinkTags {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
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
                drain(&mut buffer, &mut in_think, &mut out);
                if out.is_empty() {
                    LlmStreamAction::Replace(Vec::new())
                } else {
                    LlmStreamAction::Replace(vec![StreamEvent::TextDelta { text: out }])
                }
            }
            StreamEvent::Finish { reason } => {
                if !in_think && !buffer.is_empty() {
                    let text = std::mem::take(&mut buffer);
                    LlmStreamAction::Replace(vec![
                        StreamEvent::TextDelta { text },
                        StreamEvent::Finish { reason: *reason },
                    ])
                } else {
                    buffer.clear();
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
fn drain(buffer: &mut String, in_think: &mut bool, out: &mut String) {
    loop {
        if *in_think {
            if let Some(index) = buffer.find(CLOSE) {
                buffer.drain(..index + CLOSE.len());
                *in_think = false;
            } else {
                // Drop the reasoning but keep a partial closing tag.
                let keep = longest_suffix_prefix(buffer, CLOSE);
                buffer.drain(..buffer.len() - keep);
                break;
            }
        } else if let Some(index) = buffer.find(OPEN) {
            out.push_str(&buffer[..index]);
            buffer.drain(..index + OPEN.len());
            *in_think = true;
        } else {
            let keep = longest_suffix_prefix(buffer, OPEN);
            let emit = buffer.len() - keep;
            out.push_str(&buffer[..emit]);
            buffer.drain(..emit);
            break;
        }
    }
}

fn longest_suffix_prefix(value: &str, tag: &str) -> usize {
    let max = tag.len().min(value.len());
    (0..=max)
        .rev()
        .find(|&length| value.ends_with(&tag[..length]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{FinishReason, Protocol};
    use octra_llm_plugin_api::LlmRequestMeta;

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
                session_id: None,
                client_ip: None,
                user_agent: None,
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
            session_id: None,
            client_ip: None,
            user_agent: None,
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
}
