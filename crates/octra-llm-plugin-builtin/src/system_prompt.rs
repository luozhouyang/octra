//! Injects or replaces the system prompt on the canonical request.

use std::sync::Arc;

use async_trait::async_trait;
use octra_api_plugin_types::{PluginError, PluginManifest};
use octra_llm_plugin_types::{
    LlmAction, LlmPlugin, LlmPluginFactory, LlmPluginResult, LlmRequestContext,
};
use serde_json::Value;

use crate::manifest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Prepend,
    Append,
    Replace,
}

#[derive(Debug)]
pub struct SystemPromptPlugin {
    manifest: PluginManifest,
    prompt: String,
    mode: Mode,
}

pub struct SystemPromptFactory;

impl LlmPluginFactory for SystemPromptFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "llm-system-prompt",
            "Prepend, append or replace the system prompt",
        )
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        let prompt = config
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
            .ok_or_else(|| {
                PluginError::fail_closed("llm-system-prompt requires a non-empty `prompt`")
            })?
            .to_string();
        let mode = match config.get("mode").and_then(Value::as_str) {
            None | Some("prepend") => Mode::Prepend,
            Some("append") => Mode::Append,
            Some("replace") => Mode::Replace,
            Some(other) => {
                return Err(PluginError::fail_closed(format!(
                    "llm-system-prompt mode '{other}' must be prepend, append or replace"
                )));
            }
        };
        Ok(Arc::new(SystemPromptPlugin {
            manifest: self.manifest(),
            prompt,
            mode,
        }))
    }
}

#[async_trait]
impl LlmPlugin for SystemPromptPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
        match self.mode {
            Mode::Replace => {
                cx.request.system.blocks = vec![self.prompt.clone()];
            }
            Mode::Prepend => {
                cx.request.system.blocks.insert(0, self.prompt.clone());
            }
            Mode::Append => {
                cx.request.system.blocks.push(self.prompt.clone());
            }
        }
        Ok(LlmAction::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{ChatRequest, Protocol, SystemPrompt};
    use octra_llm_plugin_types::LlmRequestMeta;
    use serde_json::json;

    async fn apply(config: Value, request: &mut ChatRequest) {
        let plugin = SystemPromptFactory.create(config).unwrap();
        let meta = LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        };
        let mut state = serde_json::Value::Null;
        let mut cx = LlmRequestContext {
            meta: &meta,
            request,
            state: &mut state,
        };
        plugin.on_request(&mut cx).await.unwrap();
    }

    #[tokio::test]
    async fn prepend_keeps_existing_blocks() {
        let mut request = ChatRequest::new("m", Vec::new());
        request.system = SystemPrompt::text("existing");
        apply(json!({"prompt": "added"}), &mut request).await;
        assert_eq!(request.system.blocks, vec!["added", "existing"]);
    }

    #[tokio::test]
    async fn replace_discards_existing_blocks() {
        let mut request = ChatRequest::new("m", Vec::new());
        request.system = SystemPrompt::text("existing");
        apply(json!({"prompt": "added", "mode": "replace"}), &mut request).await;
        assert_eq!(request.system.blocks, vec!["added"]);
    }

    #[test]
    fn rejects_empty_prompts() {
        assert!(SystemPromptFactory.create(json!({"prompt": " "})).is_err());
        assert!(
            SystemPromptFactory
                .create(json!({"prompt": "x", "mode": "merge"}))
                .is_err()
        );
    }
}
