//! Reference LLM Gateway plugin built with the authoring SDK.
//!
//! The host calls `configure` once per component instance and then one stage
//! method per lifecycle event. This plugin:
//!
//!   * rejects requests for models listed in `reject_models` with a 429,
//!   * tags successful requests with `header_name: header_value`,
//!   * records the requested model in per-request state.

#[allow(warnings)]
mod bindings;

use octra_llm_plugin_sdk::{Decision, Error, GatewayError, LlmPlugin, RequestContext, Result};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default)]
    reject_models: Vec<String>,
    #[serde(default = "default_header_name")]
    header_name: String,
    #[serde(default = "default_header_value")]
    header_value: String,
}

fn default_header_name() -> String {
    "x-llm-demo".to_string()
}

fn default_header_value() -> String {
    "active".to_string()
}

struct LlmDemo {
    config: Config,
}

impl LlmPlugin for LlmDemo {
    fn configure(config: Value) -> Result<Self> {
        let config: Config = serde_json::from_value(config)
            .map_err(|error| Error::new(format!("invalid config: {error}")))?;
        Ok(Self { config })
    }

    fn on_request(&mut self, cx: &mut RequestContext<'_>) -> Result<Decision> {
        let model = cx.request_model().unwrap_or_default().to_string();
        if self
            .config
            .reject_models
            .iter()
            .any(|blocked| blocked == &model)
        {
            return Ok(Decision::Reject(
                GatewayError::new(
                    429,
                    "rate_limited",
                    format!("model '{model}' is blocked by llm-demo"),
                )
                .with_code("model_blocked"),
            ));
        }

        cx.headers.push((
            self.config.header_name.clone(),
            self.config.header_value.clone(),
        ));
        *cx.state = serde_json::json!({ "model": model });
        Ok(Decision::Continue)
    }
}

// The generated bindings module carries the WIT world; the SDK supplies the
// configure/on-stage adapter and the JSON envelope handling.
octra_llm_plugin_sdk::export_plugin!(LlmDemo, bindings);
