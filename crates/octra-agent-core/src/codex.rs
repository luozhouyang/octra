//! Codex CLI adapter (`~/.codex/config.toml`).
//!
//! Codex configuration is TOML with comments and ordering that users care
//! about. `toml_edit` updates the document in place so unrelated sections and
//! comments survive.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, value};

use crate::adapter::{
    AgentAdapter, AgentContext, ApplyProfile, ConfigChange, ConfigField, ConfigView, RenderedConfig,
    mask_secret,
};
use crate::AgentError;

pub const PROVIDER_ID: &str = "octra";
const AUTHORIZATION_KEY: &str = "Authorization";

#[derive(Debug, Default)]
pub struct CodexAdapter;

impl AgentAdapter for CodexAdapter {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn name(&self) -> &'static str {
        "Codex"
    }

    fn vendor(&self) -> &'static str {
        "OpenAI"
    }

    fn binary(&self) -> &'static str {
        "codex"
    }

    fn config_path(&self, ctx: &AgentContext) -> PathBuf {
        ctx.home.join(".codex").join("config.toml")
    }

    fn read(&self, content: &str, path: &Path) -> Result<ConfigView, AgentError> {
        let document = parse_document(content, path)?;
        let model = document.get("model").and_then(Item::as_str);
        let model_provider = document.get("model_provider").and_then(Item::as_str);
        let provider = provider_table(&document);

        let base_url = provider
            .and_then(|provider| provider.get("base_url"))
            .and_then(Item::as_str);
        let authorization = provider
            .and_then(|provider| provider.get("http_headers"))
            .and_then(Item::as_table_like)
            .and_then(|headers| headers.get(AUTHORIZATION_KEY))
            .and_then(Item::as_str);
        let env_key = provider
            .and_then(|provider| provider.get("env_key"))
            .and_then(Item::as_str);

        let mut fields = vec![
            ConfigField {
                key: "model".to_string(),
                value: model.map(str::to_string),
                masked: false,
            },
            ConfigField {
                key: "model_provider".to_string(),
                value: model_provider.map(str::to_string),
                masked: false,
            },
            ConfigField {
                key: "model_providers.octra.base_url".to_string(),
                value: base_url.map(str::to_string),
                masked: false,
            },
        ];
        if let Some(authorization) = authorization {
            fields.push(ConfigField {
                key: format!("model_providers.octra.http_headers.{AUTHORIZATION_KEY}"),
                value: Some(mask_secret(authorization)),
                masked: true,
            });
        }
        if let Some(env_key) = env_key {
            fields.push(ConfigField {
                key: "model_providers.octra.env_key".to_string(),
                value: Some(env_key.to_string()),
                masked: false,
            });
        }

        let mut warnings = Vec::new();
        if model_provider == Some(PROVIDER_ID) && provider.is_none() {
            warnings.push(format!(
                "model_provider is '{PROVIDER_ID}' but no provider table is defined"
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
        let mut document = match current {
            Some(content) => parse_document(content, path)?,
            None => DocumentMut::new(),
        };
        let mut changes = Vec::new();

        let previous_model = document.get("model").and_then(Item::as_str).map(str::to_string);
        if previous_model.as_deref() != Some(profile.virtual_model_id.as_str()) {
            changes.push(ConfigChange {
                field: "model".to_string(),
                before: previous_model,
                after: Some(profile.virtual_model_id.clone()),
            });
        }
        document_insert(&mut document, "model", value(&profile.virtual_model_id));

        let previous_provider = document
            .get("model_provider")
            .and_then(Item::as_str)
            .map(str::to_string);
        if previous_provider.as_deref() != Some(PROVIDER_ID) {
            changes.push(ConfigChange {
                field: "model_provider".to_string(),
                before: previous_provider,
                after: Some(PROVIDER_ID.to_string()),
            });
        }
        document_insert(&mut document, "model_provider", value(PROVIDER_ID));

        let previous_base = provider_table(&document)
            .and_then(|provider| provider.get("base_url"))
            .and_then(Item::as_str)
            .map(str::to_string);
        let base_url = format!("{}/v1", profile.base_url.trim_end_matches('/'));
        if previous_base.as_deref() != Some(base_url.as_str()) {
            changes.push(ConfigChange {
                field: "model_providers.octra.base_url".to_string(),
                before: previous_base,
                after: Some(base_url.clone()),
            });
        }

        if !document.contains_key("model_providers") {
            document.insert("model_providers", Item::Table(Table::new()));
        }
        let providers = document
            .get_mut("model_providers")
            .and_then(Item::as_table_mut)
            .ok_or_else(|| table_error(path, "model_providers"))?;
        if !providers.contains_key(PROVIDER_ID) {
            providers.insert(PROVIDER_ID, Item::Table(Table::new()));
        }
        let provider = providers
            .get_mut(PROVIDER_ID)
            .and_then(Item::as_table_mut)
            .ok_or_else(|| table_error(path, "model_providers.octra"))?;

        set_item(provider, "name", value("Octra"));
        set_item(provider, "base_url", value(&base_url));
        set_item(provider, "wire_api", value("responses"));
        provider.remove("env_key");

        if !provider.contains_key("http_headers") {
            provider.insert("http_headers", Item::Table(Table::new()));
        }
        let headers = provider
            .get_mut("http_headers")
            .and_then(Item::as_table_mut)
            .ok_or_else(|| table_error(path, "model_providers.octra.http_headers"))?;
        let previous_authorization = headers
            .get(AUTHORIZATION_KEY)
            .and_then(Item::as_str)
            .map(str::to_string);
        let authorization = format!("Bearer {}", profile.virtual_key);
        if previous_authorization.as_deref() != Some(authorization.as_str()) {
            changes.push(ConfigChange {
                field: format!("model_providers.octra.http_headers.{AUTHORIZATION_KEY}"),
                before: previous_authorization.map(|value| mask_secret(&value)),
                after: Some(mask_secret(&authorization)),
            });
        }
        set_item(headers, AUTHORIZATION_KEY, value(&authorization));

        Ok(RenderedConfig {
            content: document.to_string(),
            changes,
            warnings: Vec::new(),
        })
    }
}

fn parse_document(content: &str, path: &Path) -> Result<DocumentMut, AgentError> {
    content.parse::<DocumentMut>().map_err(|error| {
        AgentError::InvalidConfig(format!("{} is not valid TOML: {error}", path.display()))
    })
}

fn provider_table(document: &DocumentMut) -> Option<&dyn toml_edit::TableLike> {
    document
        .get("model_providers")
        .and_then(Item::as_table_like)
        .and_then(|providers| providers.get(PROVIDER_ID))
        .and_then(Item::as_table_like)
}

fn table_error(path: &Path, field: &str) -> AgentError {
    AgentError::InvalidConfig(format!("{}: `{field}` must be a table", path.display()))
}

/// Replaces an existing item in place so the key's decor (comments and
/// whitespace) survives; inserts when the key is new.
fn set_item(table: &mut dyn toml_edit::TableLike, key: &str, item: Item) {
    if let Some(existing) = table.get_mut(key) {
        *existing = item;
    } else {
        table.insert(key, item);
    }
}

fn document_insert(document: &mut DocumentMut, key: &str, item: Item) {
    set_item(document.as_table_mut(), key, item);
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::CodexAdapter;
    use crate::adapter::{AgentAdapter, ApplyProfile};

    fn profile() -> ApplyProfile {
        ApplyProfile {
            base_url: "http://127.0.0.1:8080/".to_string(),
            virtual_model_id: "octra-default".to_string(),
            virtual_key: "octra-abcdefghijklmnop".to_string(),
        }
    }

    #[test]
    fn preserves_comments_and_unrelated_sections() {
        let existing = r#"# personal config
model = "gpt-5"
model_provider = "openai"

[model_providers.openai]
name = "OpenAI"
base_url = "https://api.openai.com/v1"
"#;

        let rendered = CodexAdapter
            .render(Some(existing), Path::new("/tmp/config.toml"), &profile())
            .unwrap();

        assert!(rendered.content.contains("# personal config"));
        assert!(rendered.content.contains("[model_providers.openai]"));
        assert!(rendered.content.contains("model = \"octra-default\""));
        assert!(rendered.content.contains("[model_providers.octra]"));
        assert!(rendered.content.contains("base_url = \"http://127.0.0.1:8080/v1\""));
        assert!(rendered
            .content
            .contains("Authorization = \"Bearer octra-abcdefghijklmnop\""));
        assert!(!rendered
            .changes
            .iter()
            .any(|change| change.after.as_deref().is_some_and(|value| value.contains("abcdefghijklmnop"))));
    }

    #[test]
    fn reads_and_masks_existing_configuration() {
        let content = r#"
model = "octra-default"
model_provider = "octra"

[model_providers.octra]
name = "Octra"
base_url = "http://127.0.0.1:8080/v1"

[model_providers.octra.http_headers]
Authorization = "Bearer octra-abcdefghijklmnop"
"#;

        let view = CodexAdapter.read(content, Path::new("/tmp/config.toml")).unwrap();

        assert_eq!(view.base_url.as_deref(), Some("http://127.0.0.1:8080/v1"));
        assert!(view.provider_configured);
        let authorization = view
            .fields
            .iter()
            .find(|field| field.key.ends_with("Authorization"))
            .unwrap();
        assert!(authorization.masked);
        assert!(!authorization.value.as_deref().unwrap().contains("abcdefghijklmnop"));
    }
}
