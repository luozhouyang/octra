//! OpenCode adapter (`$XDG_CONFIG_HOME/opencode/opencode.json`).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::AgentError;
use crate::adapter::{
    AgentAdapter, AgentContext, ApplyProfile, ConfigChange, ConfigField, ConfigView,
    RenderedConfig, base_url_for, mask_secret, same_base_url,
};

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

    fn matches_base_url(&self, value: &str, expected: &str) -> bool {
        same_base_url(value, &base_url_for(expected, "/v1"))
    }

    fn read(&self, content: &str, path: &Path) -> Result<ConfigView, AgentError> {
        let (root, rewrote) = parse_object(content, path)?;
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
        if rewrote {
            warnings.push(
                "the configuration uses JSONC comments or trailing commas; applying rewrites it as plain JSON"
                    .to_string(),
            );
        }
        if provider.is_some()
            && provider
                .and_then(|provider| provider.get("models"))
                .is_none()
        {
            warnings.push("the octra provider has no models map".to_string());
        }
        if let Some(model) = model
            && !model.starts_with(&format!("{PROVIDER_ID}/"))
        {
            warnings.push(format!(
                "the default model '{model}' does not use the {PROVIDER_ID} provider"
            ));
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
        let (mut root, rewrote) = match current {
            Some(content) => {
                let (root, rewrote) = parse_object(content, path)?;
                (Value::Object(root), rewrote)
            }
            None => (Value::Object(Map::new()), false),
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
        provider.insert("npm".to_string(), json!("@ai-sdk/openai-compatible"));
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
        models.insert(profile.virtual_model_id.clone(), json!({ "name": "Octra" }));

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
            warnings: if rewrote {
                vec![
                    "comments and trailing commas are not preserved; the file is rewritten as plain JSON"
                        .to_string(),
                ]
            } else {
                Vec::new()
            },
        })
    }
}

fn parse_object(content: &str, path: &Path) -> Result<(Map<String, Value>, bool), AgentError> {
    let (normalized, rewrote) = normalized_jsonc(content);
    let value: Value = serde_json::from_str(&normalized).map_err(|error| {
        AgentError::InvalidConfig(format!("{} is not valid JSON: {error}", path.display()))
    })?;
    let object = value.as_object().cloned().ok_or_else(|| {
        AgentError::InvalidConfig(format!("{} must contain a JSON object", path.display()))
    })?;
    Ok((object, rewrote))
}

/// Accepts JSONC by removing comments and trailing commas before parsing.
fn normalized_jsonc(content: &str) -> (String, bool) {
    let (without_comments, had_comments) = strip_comments(content);
    let (without_commas, had_trailing_commas) = strip_trailing_commas(&without_comments);
    (without_commas, had_comments || had_trailing_commas)
}

fn strip_comments(content: &str) -> (String, bool) {
    let bytes = content.as_bytes();
    let mut output = String::with_capacity(content.len());
    let mut copied = 0;
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut changed = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        match byte {
            b'"' => {
                in_string = true;
                index += 1;
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                output.push_str(&content[copied..index]);
                let mut next = index + 2;
                while next < bytes.len() && bytes[next] != b'\n' {
                    next += 1;
                }
                copied = next;
                index = next;
                changed = true;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                output.push_str(&content[copied..index]);
                let mut next = index + 2;
                while next + 1 < bytes.len() && !(bytes[next] == b'*' && bytes[next + 1] == b'/') {
                    next += 1;
                }
                copied = (next + 2).min(bytes.len());
                index = copied;
                changed = true;
            }
            _ => index += 1,
        }
    }

    output.push_str(&content[copied..]);
    (output, changed)
}

fn strip_trailing_commas(content: &str) -> (String, bool) {
    let bytes = content.as_bytes();
    let mut output = String::with_capacity(content.len());
    let mut copied = 0;
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut changed = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        if byte == b'"' {
            in_string = true;
            index += 1;
            continue;
        }

        let next = bytes[index + 1..]
            .iter()
            .find(|byte| !byte.is_ascii_whitespace())
            .copied();
        if byte == b',' && matches!(next, Some(b'}') | Some(b']')) {
            output.push_str(&content[copied..index]);
            copied = index + 1;
            changed = true;
        }
        index += 1;
    }

    output.push_str(&content[copied..]);
    (output, changed)
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
        assert_eq!(
            root["provider"]["octra"]["options"]["baseURL"],
            "http://127.0.0.1:8080/v1"
        );
        assert_eq!(root["model"], "octra/octra-default");
        assert_eq!(
            root["provider"]["octra"]["models"]["octra-default"]["name"],
            "Octra"
        );
        assert!(!rendered.changes.iter().any(|change| {
            change
                .after
                .as_deref()
                .is_some_and(|value| value.contains("abcdefghijklmnop"))
        }));
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
        assert!(
            !api_key
                .value
                .as_deref()
                .unwrap()
                .contains("abcdefghijklmnop")
        );
    }

    #[test]
    fn parses_jsonc_and_rewrites_it_as_plain_json() {
        let existing = r#"{
          // line comment
          "model": "old",
          "notes": "http://example.com//keep",
          "provider": {"anthropic": {"name": "Anthropic"}}, /* block */
        }"#;

        let rendered = OpenCodeAdapter
            .render(Some(existing), Path::new("/tmp/opencode.json"), &profile())
            .unwrap();
        let root: serde_json::Value = serde_json::from_str(&rendered.content).unwrap();

        assert!(!rendered.content.contains("// line"));
        assert!(!rendered.content.contains("/*"));
        assert_eq!(root["notes"], "http://example.com//keep");
        assert_eq!(root["provider"]["anthropic"]["name"], "Anthropic");
        assert_eq!(root["model"], "octra/octra-default");
        assert!(
            rendered
                .warnings
                .iter()
                .any(|warning| warning.contains("plain JSON"))
        );
    }

    #[test]
    fn reads_jsonc_with_a_warning() {
        let view = OpenCodeAdapter
            .read(
                "{\n// comment\n\"model\": \"octra/octra-default\",\n}",
                Path::new("/tmp/opencode.json"),
            )
            .unwrap();

        assert_eq!(view.model.as_deref(), Some("octra/octra-default"));
        assert!(
            view.warnings
                .iter()
                .any(|warning| warning.contains("JSONC"))
        );
    }

    #[test]
    fn rejects_invalid_json_without_rewriting_it() {
        let error = OpenCodeAdapter
            .render(
                Some("{\"model\": }"),
                Path::new("/tmp/opencode.json"),
                &profile(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("not valid JSON"));
    }

    #[test]
    fn treats_the_v1_prefix_as_the_same_gateway() {
        assert!(
            OpenCodeAdapter.matches_base_url("http://127.0.0.1:8080/v1", "http://127.0.0.1:8080")
        );
        assert!(
            !OpenCodeAdapter.matches_base_url("http://127.0.0.1:8080/v2", "http://127.0.0.1:8080")
        );
    }
}
