//! Claude Code adapter (`~/.claude/settings.json`).
//!
//! Claude Code reads its gateway settings from the `env` object. Octra only
//! touches those keys and leaves permissions, hooks and MCP servers intact.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::adapter::{
    AgentAdapter, AgentContext, ApplyProfile, ConfigChange, ConfigField, ConfigView, RenderedConfig,
    mask_secret,
};
use crate::AgentError;

pub const BASE_URL_KEY: &str = "ANTHROPIC_BASE_URL";
pub const TOKEN_KEY: &str = "ANTHROPIC_AUTH_TOKEN";
pub const MODEL_KEY: &str = "ANTHROPIC_MODEL";
pub const OPUS_MODEL_KEY: &str = "ANTHROPIC_DEFAULT_OPUS_MODEL";
pub const SONNET_MODEL_KEY: &str = "ANTHROPIC_DEFAULT_SONNET_MODEL";
pub const HAIKU_MODEL_KEY: &str = "ANTHROPIC_DEFAULT_HAIKU_MODEL";

const MODEL_KEYS: [&str; 4] = [MODEL_KEY, OPUS_MODEL_KEY, SONNET_MODEL_KEY, HAIKU_MODEL_KEY];

#[derive(Debug, Default)]
pub struct ClaudeCodeAdapter;

impl AgentAdapter for ClaudeCodeAdapter {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn name(&self) -> &'static str {
        "Claude Code"
    }

    fn vendor(&self) -> &'static str {
        "Anthropic"
    }

    fn binary(&self) -> &'static str {
        "claude"
    }

    fn config_path(&self, ctx: &AgentContext) -> PathBuf {
        ctx.home.join(".claude").join("settings.json")
    }

    fn read(&self, content: &str, path: &Path) -> Result<ConfigView, AgentError> {
        let root = parse_object(content, path)?;
        let env = root.get("env").and_then(Value::as_object);
        let base_url = env_string(env, BASE_URL_KEY);
        let token = env_string(env, TOKEN_KEY);
        let model = env_string(env, MODEL_KEY);

        let mut fields = Vec::new();
        fields.push(field(BASE_URL_KEY, base_url, false));
        fields.push(field(MODEL_KEY, model, false));
        fields.push(field(TOKEN_KEY, token, true));
        for key in [OPUS_MODEL_KEY, SONNET_MODEL_KEY, HAIKU_MODEL_KEY] {
            if let Some(value) = env_string(env, key) {
                fields.push(ConfigField {
                    key: key.to_string(),
                    value: Some(value.to_string()),
                    masked: false,
                });
            }
        }

        let mut warnings = Vec::new();
        if token.is_some() && base_url.is_none() {
            warnings.push("an auth token is set without a base URL".to_string());
        }

        Ok(ConfigView {
            path: path.display().to_string(),
            exists: true,
            base_url: base_url.map(str::to_string),
            model: model.map(str::to_string),
            provider_configured: base_url.is_some() && token.is_some(),
            fields,
            warnings,
        })
    }

    fn render(
        &self,
        current: Option<&str>,
        path: &Path,
        profile: &ApplyProfile,
    ) -> Result<RenderedConfig, AgentError> {
        let mut root = match current {
            Some(content) => Value::Object(parse_object(content, path)?),
            None => Value::Object(Map::new()),
        };
        let object = root.as_object_mut().expect("object was constructed above");
        let env = object
            .entry("env")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or_else(|| {
                AgentError::InvalidConfig(format!("{}: `env` must be an object", path.display()))
            })?;

        let base_url = profile.base_url.trim_end_matches('/').to_string();
        let mut changes = Vec::new();
        set_env(env, BASE_URL_KEY, &base_url, false, &mut changes);
        set_env(env, TOKEN_KEY, &profile.virtual_key, true, &mut changes);
        for key in MODEL_KEYS {
            set_env(env, key, &profile.virtual_model_id, false, &mut changes);
        }

        let content = serde_json::to_string_pretty(&root)
            .map_err(|error| AgentError::InvalidConfig(format!("cannot encode settings: {error}")))?;

        Ok(RenderedConfig {
            content: format!("{content}\n"),
            changes,
            warnings: Vec::new(),
        })
    }
}

fn parse_object(content: &str, path: &Path) -> Result<Map<String, Value>, AgentError> {
    let value: Value = serde_json::from_str(content).map_err(|error| {
        AgentError::InvalidConfig(format!("{} is not valid JSON: {error}", path.display()))
    })?;
    value.as_object().cloned().ok_or_else(|| {
        AgentError::InvalidConfig(format!("{} must contain a JSON object", path.display()))
    })
}

fn env_string<'a>(env: Option<&'a Map<String, Value>>, key: &str) -> Option<&'a str> {
    env.and_then(|env| env.get(key)).and_then(Value::as_str)
}

fn field(key: &str, value: Option<&str>, secret: bool) -> ConfigField {
    ConfigField {
        key: key.to_string(),
        value: value.map(|value| if secret { mask_secret(value) } else { value.to_string() }),
        masked: secret && value.is_some(),
    }
}

fn set_env(
    env: &mut Map<String, Value>,
    key: &str,
    value: &str,
    secret: bool,
    changes: &mut Vec<ConfigChange>,
) {
    let before = env.get(key).and_then(Value::as_str).map(str::to_string);
    if before.as_deref() != Some(value) {
        changes.push(ConfigChange {
            field: key.to_string(),
            before: before.map(|before| if secret { mask_secret(&before) } else { before }),
            after: Some(if secret { mask_secret(value) } else { value.to_string() }),
        });
    }
    env.insert(key.to_string(), json!(value));
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{BASE_URL_KEY, ClaudeCodeAdapter, MODEL_KEY, TOKEN_KEY};
    use crate::adapter::{AgentAdapter, ApplyProfile};

    fn profile() -> ApplyProfile {
        ApplyProfile {
            base_url: "http://127.0.0.1:8080/".to_string(),
            virtual_model_id: "octra-default".to_string(),
            virtual_key: "octra-abcdefghijklmnop".to_string(),
        }
    }

    #[test]
    fn reads_existing_settings_and_masks_the_token() {
        let content = r#"{
          "env": {
            "ANTHROPIC_BASE_URL": "http://127.0.0.1:8080",
            "ANTHROPIC_AUTH_TOKEN": "octra-abcdefghijklmnop",
            "ANTHROPIC_MODEL": "octra-default"
          },
          "permissions": {"allow": ["Bash"]}
        }"#;

        let view = ClaudeCodeAdapter
            .read(content, Path::new("/tmp/settings.json"))
            .unwrap();

        assert_eq!(view.base_url.as_deref(), Some("http://127.0.0.1:8080"));
        assert!(view.provider_configured);
        let token = view.fields.iter().find(|field| field.key == TOKEN_KEY).unwrap();
        assert!(token.masked);
        assert_eq!(token.value.as_deref(), Some("octr…mnop"));
    }

    #[test]
    fn preserves_unrelated_settings_and_other_env_entries() {
        let existing = r#"{
          "env": {"KEEP_ME": "1"},
          "permissions": {"allow": ["Bash"]}
        }"#;

        let rendered = ClaudeCodeAdapter
            .render(Some(existing), Path::new("/tmp/settings.json"), &profile())
            .unwrap();
        let root: serde_json::Value = serde_json::from_str(&rendered.content).unwrap();

        assert_eq!(root["env"]["KEEP_ME"], "1");
        assert_eq!(root["permissions"]["allow"][0], "Bash");
        assert_eq!(root["env"][BASE_URL_KEY], "http://127.0.0.1:8080");
        assert_eq!(root["env"][MODEL_KEY], "octra-default");
        assert!(rendered
            .changes
            .iter()
            .all(|change| !change.after.as_deref().is_some_and(|value| value.contains("abcdefghijklmnop"))));
    }

    #[test]
    fn rejects_a_non_object_document() {
        let error = ClaudeCodeAdapter
            .render(Some("[]"), Path::new("/tmp/settings.json"), &profile())
            .unwrap_err();
        assert!(error.to_string().contains("must contain a JSON object"));
    }
}
