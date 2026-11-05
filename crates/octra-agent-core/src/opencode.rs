//! OpenCode adapter (`$XDG_CONFIG_HOME/opencode/opencode.json`).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::adapter::{
    AgentAdapter, AgentContext, ApplyProfile, ConfigChange, ConfigField, ConfigView, RenderedConfig,
    base_url_for, mask_secret,
};
use crate::AgentError;

pub const PROVIDER_ID: &str = "octra";

#[derive(Debug, Default)]
pub struct OpenCodeAdapter;

impl AgentAdapter for OpenCodeAdapter {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn name(&self) -> &'static str {
        "OpenCode"
    }

    fn vendor(&self) -> &'static str {
        "OpenCode"
    }

    fn binary(&self) -> &'static str {
        "opencode"
    }

    fn config_path(&self, ctx: &AgentContext) -> PathBuf {
        ctx.config_dir.join("opencode").join("opencode.json")
    }

    fn read(&self, content: &str, path: &Path) -> Result<ConfigView, AgentError> {
        let root = parse_object(content, path)?;
        let provider = root
            .get("provider")
            .and_then(Value::as_object)
            .and_then(|providers| providers.get(PROVIDER_ID))
            .and_then(Value::as_object);
        let options = provider
            .and_then(|provider| provider.get("options"))
            .and_then(Value::as_object);
        let base_url = options
            .and_then(|options| options.get("baseURL"))
            .and_then(Value::as_str);
        let api_key = options
            .and_then(|options| options.get("apiKey"))
            .and_then(Value::as_str);
        let model = root.get("model").and_then(Value::as_str);

        let mut fields = vec![
            ConfigField {
                key: "provider.octra.options.baseURL".to_string(),
                value: base_url.map(str::to_string),
                masked: false,
            },
            ConfigField {
                key: "model".to_string(),
                value: model.map(str::to_string),
                masked: false,
            },
        ];
        if let Some(api_key) = api_key {
            fields.push(ConfigField {
                key: "provider.octra.options.apiKey".to_string(),
                value: Some(mask_secret(api_key)),
                masked: true,
            });
        }

        let mut warnings = Vec::new();
        if provider.is_some() && provider.and_then(|provider| provider.get("models")).is_none() {
            warnings.push("the octra provider has no models map".to_string());
        }
        if let Some(model) = model {
            if !model.starts_with(&format!("{PROVIDER_ID}/")) {
                warnings.push(format!(
                    "the default model '{model}' does not use the {PROVIDER_ID} provider"
                ));
            }
        }

        Ok(ConfigView {
            path: path.display().to_string(),
            exists: true,
            base_url: base_url.map(str::to_string),
            model: model.map(str::to_string),
            provider_configured: provider.is_some(),
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

        if !object.contains_key("$schema") {
            object.insert(
                "$schema".to_string(),
                json!("https://opencode.ai/config.json"),
            );
        }

        let providers = ensure_object(object, "provider", path)?;
        let provider = ensure_object(providers, PROVIDER_ID, path)?;
        provider.insert(
            "npm".to_string(),
            json!("@ai-sdk/openai-compatible"),
        );
        provider.insert("name".to_string(), json!("Octra"));

        let base_url = base_url_for(&profile.base_url, "/v1");
        let mut changes = Vec::new();

        let options = ensure_object(provider, "options", path)?;
        let previous_base = options
            .get("baseURL")
            .and_then(Value::as_str)
            .map(str::to_string);
        if previous_base.as_deref() != Some(base_url.as_str()) {
            changes.push(ConfigChange {
                field: "provider.octra.options.baseURL".to_string(),
                before: previous_base,
                after: Some(base_url.clone()),
            });
        }
        options.insert("baseURL".to_string(), json!(base_url));

        let previous_key = options
            .get("apiKey")
            .and_then(Value::as_str)
            .map(str::to_string);
        if previous_key.as_deref() != Some(profile.virtual_key.as_str()) {
            changes.push(ConfigChange {
                field: "provider.octra.options.apiKey".to_string(),
                before: previous_key.map(|value| mask_secret(&value)),
                after: Some(mask_secret(&profile.virtual_key)),
            });
        }
        options.insert("apiKey".to_string(), json!(profile.virtual_key));

        let models = ensure_object(provider, "models", path)?;
        models.insert(
            profile.virtual_model_id.clone(),
            json!({ "name": "Octra" }),
        );

        let model = format!("{PROVIDER_ID}/{}", profile.virtual_model_id);
        let previous_model = object
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        if previous_model.as_deref() != Some(model.as_str()) {
            changes.push(ConfigChange {
                field: "model".to_string(),
                before: previous_model,
                after: Some(model.clone()),
            });
        }
        object.insert("model".to_string(), json!(model));

        let content = serde_json::to_string_pretty(&root)
            .map_err(|error| AgentError::InvalidConfig(format!("cannot encode config: {error}")))?;

        Ok(RenderedConfig {
            content: format!("{content}\n"),
            changes,
            warnings: Vec::new(),
        })
    }
}

fn parse_object(content: &str, path: &Path) -> Result<Map<String, Value>, AgentError> {
    let value: Value = serde_json::from_str(content).map_err(|error| {
        AgentError::InvalidConfig(format!(
            "{} is not valid JSON (JSONC comments are not supported yet): {error}",
            path.display()
        ))
    })?;
    value.as_object().cloned().ok_or_else(|| {
        AgentError::InvalidConfig(format!("{} must contain a JSON object", path.display()))
    })
}

fn ensure_object<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<&'a mut Map<String, Value>, AgentError> {
    parent
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            AgentError::InvalidConfig(format!("{}: `{key}` must be an object", path.display()))
        })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::OpenCodeAdapter;
    use crate::adapter::{AgentAdapter, ApplyProfile};

    fn profile() -> ApplyProfile {
        ApplyProfile {
            base_url: "http://127.0.0.1:8080".to_string(),
            virtual_model_id: "octra-default".to_string(),
            virtual_key: "octra-abcdefghijklmnop".to_string(),
        }
    }

    #[test]
    fn keeps_other_providers_and_adds_octra() {
        let existing = r#"{
          "$schema": "https://opencode.ai/config.json",
          "provider": {
            "anthropic": {"npm": "@ai-sdk/anthropic", "name": "Anthropic"}
          }
        }"#;

        let rendered = OpenCodeAdapter
            .render(Some(existing), Path::new("/tmp/opencode.json"), &profile())
            .unwrap();
        let root: serde_json::Value = serde_json::from_str(&rendered.content).unwrap();

        assert_eq!(root["provider"]["anthropic"]["name"], "Anthropic");
        assert_eq!(root["provider"]["octra"]["options"]["baseURL"], "http://127.0.0.1:8080/v1");
        assert_eq!(root["model"], "octra/octra-default");
        assert_eq!(root["provider"]["octra"]["models"]["octra-default"]["name"], "Octra");
        assert!(!rendered
            .changes
            .iter()
            .any(|change| change.after.as_deref().is_some_and(|value| value.contains("abcdefghijklmnop"))));
    }

    #[test]
    fn reads_and_masks_an_existing_key() {
        let content = r#"{
          "provider": {
            "octra": {
              "options": {
                "baseURL": "http://127.0.0.1:8080/v1",
                "apiKey": "octra-abcdefghijklmnop"
              },
              "models": {"octra-default": {"name": "Octra"}}
            }
          },
          "model": "octra/octra-default"
        }"#;

        let view = OpenCodeAdapter
            .read(content, Path::new("/tmp/opencode.json"))
            .unwrap();

        assert!(view.provider_configured);
        let api_key = view
            .fields
            .iter()
            .find(|field| field.key.ends_with("apiKey"))
            .unwrap();
        assert!(api_key.masked);
        assert!(!api_key.value.as_deref().unwrap().contains("abcdefghijklmnop"));
    }

    #[test]
    fn rejects_json_with_comments_without_rewriting_it() {
        let error = OpenCodeAdapter
            .render(
                Some("{ // comment\n }"),
                Path::new("/tmp/opencode.json"),
                &profile(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("JSONC comments are not supported"));
    }
}
