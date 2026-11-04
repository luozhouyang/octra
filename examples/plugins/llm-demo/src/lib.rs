//! Reference LLM Gateway plugin.
//!
//! The host calls `configure` once per component instance and then `on-stage`
//! for every lifecycle stage. This plugin:
//!
//!   * rejects requests for models listed in `reject_models` with a 429,
//!   * tags successful requests with `header_name: header_value`,
//!   * records the requested model in per-request state.

#[allow(warnings)]
mod bindings;

use bindings::exports::octra::llm_plugin::plugin_api::{Guest, Invocation, Outcome};
use bindings::octra::llm_plugin::types::Stage;
use serde::Deserialize;
use serde_json::{Value, json};

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

impl Config {
    fn parse(raw: &str) -> Result<Self, String> {
        let value: Value =
            serde_json::from_str(raw).map_err(|error| format!("invalid config JSON: {error}"))?;
        serde_json::from_value(value).map_err(|error| format!("invalid config: {error}"))
    }
}

struct LlmDemo;

impl Guest for LlmDemo {
    fn configure(config_json: String) -> Result<(), String> {
        Config::parse(&config_json).map(|_| ())
    }

    fn on_stage(call: Invocation) -> Result<Outcome, String> {
        let config = Config::parse(&call.config_json)?;
        match call.stage {
            Stage::Request => on_request(&config, &call),
            _ => Ok(passthrough(&call)),
        }
    }
}

fn on_request(config: &Config, call: &Invocation) -> Result<Outcome, String> {
    let payload: Value = serde_json::from_str(&call.payload_json)
        .map_err(|error| format!("invalid request payload: {error}"))?;
    let model = payload
        .get("request")
        .and_then(|request| request.get("model"))
        .and_then(Value::as_str)
        .unwrap_or_default();

    if config.reject_models.iter().any(|blocked| blocked == model) {
        let error = json!({
            "status": 429,
            "kind": "rate_limited",
            "code": "model_blocked",
            "message": format!("model '{model}' is blocked by llm-demo"),
        });
        return Ok(Outcome {
            action: "reject".to_string(),
            payload_json: None,
            state_json: call.state_json.clone(),
            error_json: Some(error.to_string()),
            headers: Vec::new(),
        });
    }

    Ok(Outcome {
        action: "continue".to_string(),
        payload_json: None,
        state_json: json!({ "model": model }).to_string(),
        error_json: None,
        headers: vec![(
            config.header_name.clone(),
            config.header_value.clone(),
        )],
    })
}

fn passthrough(call: &Invocation) -> Outcome {
    Outcome {
        action: "continue".to_string(),
        payload_json: None,
        state_json: call.state_json.clone(),
        error_json: None,
        headers: Vec::new(),
    }
}

bindings::export!(LlmDemo with_types_in bindings);
